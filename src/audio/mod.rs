// Recording from the microphone into a file the speech service accepts:
// WAV, PCM mono 16 bit 16 kHz. The counterpart of AudioRecorder.swift.
//
// The sound server is reached through libpulse; on PipeWire systems that is
// pipewire-pulse. Everything libpulse touches lives on one std thread named
// "audio" (pulse.rs); this type only sends it commands and waits for the
// answers.
//
// Recording counts as started when the stream is ready; the first buffer of
// audio is logged separately, with the time it took to arrive.

mod pulse;
mod sink;

use std::fmt;
use std::io::{PipeWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{oneshot, watch};

use crate::config::Microphone;
use crate::log;

/// How long the first listing of microphones may take before startup goes
/// on without waiting for it.
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a stream may take to become ready.
const START_TIMEOUT: Duration = Duration::from_secs(3);

/// A finished recording on disk.
#[derive(Debug, Clone)]
pub struct Recording {
    /// PCM mono 16 bit 16 kHz, wav.
    pub path: PathBuf,
    /// Seconds of sound in the file.
    pub duration: f64,
    /// Peak of the unsmoothed RMS over the whole recording, 0...1. For the log
    /// and the history row; nothing is thrown away on the strength of it.
    pub peak: f32,
}

#[derive(Debug, Clone)]
pub enum AudioError {
    /// No source to record from: none chosen is connected and the server has
    /// no default either.
    NoMicrophone,
    /// The stream or the file could not be set up, with the reason.
    CouldNotStart(String),
    /// The file could not be finished, with the reason.
    CouldNotWrite(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioError::NoMicrophone => f.write_str("no microphone"),
            AudioError::CouldNotStart(reason) => write!(f, "could not start recording: {reason}"),
            AudioError::CouldNotWrite(reason) => write!(f, "could not write the recording: {reason}"),
        }
    }
}

/// One source the sound server has, monitors of outputs left out.
#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    /// Server index, valid for this connection only.
    pub index: u32,
    /// Source name, stable enough to keep in the settings.
    pub name: String,
    pub description: String,
    /// `device.form_factor`, when the source reports it.
    pub form_factor: Option<String>,
    /// `device.bus`, when the source reports it.
    pub bus: Option<String>,
    pub built_in: bool,
}

/// Every source plus the server's default, as the Settings window shows them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Inputs {
    pub list: Vec<Input>,
    /// `default_source_name` from the server.
    pub default: Option<String>,
}

enum Command {
    Start {
        microphone: Microphone,
        tag: String,
        reply: oneshot::Sender<Result<(), AudioError>>,
    },
    Stop {
        reply: oneshot::Sender<Result<Recording, AudioError>>,
    },
    Cancel,
    Quit,
}

pub struct AudioRecorder {
    commands: mpsc::Sender<Command>,
    /// One byte here wakes the audio thread out of its poll.
    wake: Option<PipeWriter>,
    thread: Option<JoinHandle<()>>,
    level: watch::Receiver<f32>,
    inputs: watch::Receiver<Inputs>,
}

impl AudioRecorder {
    /// Name of the path sound is recorded through, for the `backends:` line.
    pub fn backend_name() -> &'static str {
        "pulse"
    }

    /// Starts the audio thread and waits for its first listing of the
    /// microphones, so those lines land in the log with the other startup
    /// lines. Without a sound server the app runs on; each recording then
    /// fails with a reason in the log.
    pub async fn open() -> AudioRecorder {
        let (commands, commands_rx) = mpsc::channel();
        let (level_tx, level) = watch::channel(0.0);
        let (inputs_tx, inputs) = watch::channel(Inputs::default());
        let (ready_tx, ready_rx) = oneshot::channel();

        let mut recorder = AudioRecorder {
            commands,
            wake: None,
            thread: None,
            level,
            inputs,
        };

        let (wake_rx, wake_tx) = match std::io::pipe() {
            Ok(pipe) => pipe,
            Err(error) => {
                log::error!(Audio, "could not create the wake pipe ({error}), recording is off");
                return recorder;
            }
        };

        let thread = pulse::spawn(pulse::Channels {
            commands: commands_rx,
            wake: wake_rx,
            level: level_tx,
            inputs: inputs_tx,
            ready: ready_tx,
        });
        match thread {
            Ok(thread) => {
                recorder.wake = Some(wake_tx);
                recorder.thread = Some(thread);
            }
            Err(error) => {
                log::error!(Audio, "could not start the audio thread ({error}), recording is off");
                return recorder;
            }
        }

        if tokio::time::timeout(OPEN_TIMEOUT, ready_rx).await.is_err() {
            log::warning!(Audio, "the sound server has not answered in {OPEN_TIMEOUT:?}, going on");
        }
        recorder
    }

    /// Opens a stream from the microphone `microphone` names. Returns once the
    /// stream is ready. `tag` marks the log lines of this dictation.
    pub async fn start(&self, microphone: &Microphone, tag: &str) -> Result<(), AudioError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Start {
            microphone: microphone.clone(),
            tag: tag.to_string(),
            reply,
        })?;
        match tokio::time::timeout(START_TIMEOUT, answer).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AudioError::CouldNotStart("the audio thread has stopped".to_string())),
            Err(_) => {
                // Leaves no half-open stream behind.
                self.cancel();
                Err(AudioError::CouldNotStart(format!(
                    "the stream was not ready after {START_TIMEOUT:?}"
                )))
            }
        }
    }

    /// Closes the stream completely and finishes the file.
    pub async fn stop(&self) -> Result<Recording, AudioError> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Stop { reply })?;
        answer
            .await
            .unwrap_or_else(|_| Err(AudioError::CouldNotWrite("the audio thread has stopped".to_string())))
    }

    /// Closes the stream and removes the file, if anything is recording.
    pub fn cancel(&self) {
        let _ = self.send(Command::Cancel);
    }

    /// Smoothed microphone level, 0...1. The pill reads it 60 times a second
    /// while listening.
    pub fn level(&self) -> watch::Receiver<f32> {
        self.level.clone()
    }

    /// Current microphones, kept up to date as they come and go. Read by the
    /// Settings window.
    pub fn inputs(&self) -> watch::Receiver<Inputs> {
        self.inputs.clone()
    }

    /// Stops the audio thread and waits for it. Blocks.
    pub fn close(mut self) {
        let _ = self.send(Command::Quit);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                log::error!(Audio, "audio thread panicked");
            }
        }
    }

    fn send(&self, command: Command) -> Result<(), AudioError> {
        let not_running = || AudioError::CouldNotStart("the audio thread is not running".to_string());
        let Some(wake) = &self.wake else {
            return Err(not_running());
        };
        self.commands.send(command).map_err(|_| not_running())?;
        let mut wake: &PipeWriter = wake;
        wake.write_all(&[1]).map_err(|_| not_running())
    }
}
