// Ties the activation key to the recorder, the pill and the history, as
// DictationController does on macOS. So far: the key starts and stops a
// recording. Recognition joins in step 4, the pill in step 3.
//
// Every dictation has a short tag, and every line it writes carries it as
// "[a1b2]", so one run can be read end to end in the log.
//
// One thing never goes further: a press too short to hold a word.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audio::AudioRecorder;
use crate::config::Microphone;
use crate::log;

/// A recording below this is never sent: the speech service refuses it.
/// Matches shortestSentRecording on macOS.
const SHORTEST_SENT_RECORDING: f64 = 0.3;

/// The dictation whose key is down.
struct Dictation {
    tag: String,
    key_down_at: Instant,
}

pub struct DictationController {
    recorder: AudioRecorder,
    /// From the settings; turned into a source at every start, so a
    /// microphone plugged in since launch is found.
    microphone: Microphone,
    current: Option<Dictation>,
}

impl DictationController {
    pub fn new(recorder: AudioRecorder, microphone: Microphone) -> Self {
        DictationController {
            recorder,
            microphone,
            current: None,
        }
    }

    pub async fn key_down(&mut self) {
        log::info!(Hotkey, "KEY_DOWN");
        // A second press while recording changes nothing.
        if self.current.is_some() {
            return;
        }

        let tag = make_tag();
        let key_down_at = Instant::now();
        log::info!(App, "[{tag}] dictation started");

        match self.recorder.start(&self.microphone, &tag).await {
            Ok(()) => {
                log::info!(App, "[{tag}] recording after {} ms", milliseconds(key_down_at.elapsed()));
                self.current = Some(Dictation { tag, key_down_at });
            }
            Err(error) => log::error!(App, "[{tag}] {error}"),
        }
    }

    pub async fn key_up(&mut self) {
        log::info!(Hotkey, "KEY_UP");
        let Some(dictation) = self.current.take() else {
            return;
        };
        let tag = dictation.tag;
        let key_up_at = Instant::now();
        log::info!(
            App,
            "[{tag}] key released after {} ms",
            milliseconds(dictation.key_down_at.elapsed())
        );

        let recording = match self.recorder.stop().await {
            Ok(recording) => recording,
            Err(error) => {
                log::error!(App, "[{tag}] {error}");
                return;
            }
        };
        log::info!(
            App,
            "[{tag}] audio written after {} ms, {:.2} s of sound, peak {:.3}",
            milliseconds(key_up_at.elapsed()),
            recording.duration,
            recording.peak
        );

        // A press this short is a stray keystroke, not a word: the service
        // refuses it. Dropped here, before it reaches the history.
        if recording.duration < SHORTEST_SENT_RECORDING {
            if let Err(error) = std::fs::remove_file(&recording.path) {
                log::warning!(
                    Storage,
                    "[{tag}] could not remove {}: {error}",
                    recording.path.display()
                );
            }
            log::info!(Storage, "[{tag}] shorter than {SHORTEST_SENT_RECORDING:.2} s, not sent");
        }
    }

    /// Drops a recording in progress and stops the audio thread. Blocks
    /// while the thread ends.
    pub fn shut_down(self) {
        if let Some(dictation) = &self.current {
            log::info!(App, "[{}] quitting while recording", dictation.tag);
            self.recorder.cancel();
        }
        self.recorder.close();
    }
}

/// Four hex characters, different for every dictation.
fn make_tag() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(nanos);
    format!("{:04x}", hasher.finish() as u16)
}

fn milliseconds(duration: Duration) -> u128 {
    duration.as_millis()
}
