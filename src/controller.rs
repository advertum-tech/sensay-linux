// Ties the activation key to the recorder, the pill, the recognition, the
// history and the paste, as DictationController does on macOS.
//
// Every dictation has a short tag, and every line it writes carries it as
// "[a1b2]", so one run can be read end to end in the log.
//
// One thing never reaches the history: a press too short to hold a word.

use std::collections::HashSet;
use std::sync::Arc;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::UnboundedSender;

use crate::asr::recogniser::Recogniser;
use crate::audio::{AudioRecorder, Recording};
use crate::config::Microphone;
use crate::error_presenter::ErrorPresenter;
use crate::history::{History, Transcript, TranscriptState};
use crate::log;
use crate::paste::TextSink;
use crate::pill::{Pill, PillState};

/// A recording below this is never sent: the speech service refuses it.
/// Matches shortestSentRecording on macOS.
const SHORTEST_SENT_RECORDING: f64 = 0.3;

/// Text that comes back later than this after the key was released goes to
/// the history only: by then the cursor may well be somewhere else. The pill
/// waits no longer than this either (RecognitionTiming.pasteWindow).
const PASTE_WINDOW: Duration = Duration::from_secs(15);

/// What the controller has the main loop bring back to it later.
pub enum ControllerEvent {
    /// The answer for this dictation is in, or the paste window is over.
    StopWaiting(String),
}

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
    pill: Pill,
    /// Dictations whose key is up and whose text has not come back. The pill
    /// waits while there is any.
    waiting: HashSet<String>,
    events: UnboundedSender<ControllerEvent>,
    /// None when the history could not be opened: recording still works, but
    /// nothing is sent, as on macOS.
    history: Option<History>,
    recogniser: Recogniser,
    sink: TextSink,
    /// Pastes go one at a time: each holds the clipboard for half a second,
    /// and a paste started inside that time would take the first one's text
    /// for the user's own clipboard and put the wrong thing back.
    paste_turn: Arc<tokio::sync::Mutex<()>>,
    errors: ErrorPresenter,
}

impl DictationController {
    pub fn new(
        recorder: AudioRecorder,
        microphone: Microphone,
        pill: Pill,
        events: UnboundedSender<ControllerEvent>,
        history: Option<History>,
        recogniser: Recogniser,
        sink: TextSink,
        errors: ErrorPresenter,
    ) -> Self {
        DictationController {
            recorder,
            microphone,
            current: None,
            pill,
            waiting: HashSet::new(),
            events,
            history,
            recogniser,
            sink,
            paste_turn: Arc::new(tokio::sync::Mutex::new(())),
            errors,
        }
    }

    /// From the Settings window; used from the next press on.
    pub fn set_microphone(&mut self, microphone: Microphone) {
        self.microphone = microphone;
    }

    pub fn handle(&mut self, event: ControllerEvent) {
        match event {
            ControllerEvent::StopWaiting(tag) => self.stop_waiting(&tag),
        }
    }

    /// The one place that decides what the pill shows. Recording wins over
    /// waiting; waiting wins over rest.
    fn update_pill(&self) {
        if self.current.is_some() {
            return;
        }
        if self.waiting.is_empty() {
            self.pill.set(PillState::Idle);
        } else {
            self.pill.set(PillState::Waiting);
        }
    }

    /// The pill waits for each dictation no longer than the paste window: text
    /// later than that goes to the history only, and a spinner past it would
    /// promise a paste that is not coming.
    fn start_waiting(&mut self, tag: &str) {
        self.waiting.insert(tag.to_string());
        self.update_pill();
        let events = self.events.clone();
        let tag = tag.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(PASTE_WINDOW).await;
            let _ = events.send(ControllerEvent::StopWaiting(tag));
        });
    }

    /// Safe to call more than once for the same dictation.
    fn stop_waiting(&mut self, tag: &str) {
        if self.waiting.remove(tag) {
            self.update_pill();
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

        self.pill.set(PillState::Listening);
        match self.recorder.start(&self.microphone, &tag).await {
            Ok(()) => {
                log::info!(App, "[{tag}] recording after {} ms", milliseconds(key_down_at.elapsed()));
                self.current = Some(Dictation { tag, key_down_at });
            }
            Err(error) => {
                log::error!(App, "[{tag}] {error}");
                self.errors.show(error.message());
                self.update_pill();
            }
        }
    }

    pub async fn key_up(&mut self) {
        log::info!(Hotkey, "KEY_UP");
        let Some(dictation) = self.current.take() else {
            self.update_pill();
            return;
        };
        let tag = dictation.tag;
        let key_up_at = Instant::now();
        // From here until the answer: writing the file, sending it, waiting.
        self.start_waiting(&tag);
        log::info!(
            App,
            "[{tag}] key released after {} ms",
            milliseconds(dictation.key_down_at.elapsed())
        );

        let recording = match self.recorder.stop().await {
            Ok(recording) => recording,
            Err(error) => {
                log::error!(App, "[{tag}] {error}");
                self.errors.show(error.message());
                self.stop_waiting(&tag);
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
            self.stop_waiting(&tag);
            return;
        }

        // From here on the key is free for the next dictation.
        tokio::spawn(finish(
            tag,
            recording,
            key_up_at,
            Services {
                history: self.history.clone(),
                recogniser: self.recogniser.clone(),
                sink: self.sink.clone(),
                paste_turn: self.paste_turn.clone(),
                events: self.events.clone(),
                errors: self.errors.clone(),
            },
        ));
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

/// What the rest of a dictation needs, handed over from the controller.
struct Services {
    history: Option<History>,
    recogniser: Recogniser,
    sink: TextSink,
    paste_turn: Arc<tokio::sync::Mutex<()>>,
    events: UnboundedSender<ControllerEvent>,
    errors: ErrorPresenter,
}

/// The rest of one dictation, after its audio is on disk: the history row,
/// the queue, the answer, the paste.
async fn finish(tag: String, recording: Recording, key_up_at: Instant, services: Services) {
    let Services {
        history,
        recogniser,
        sink,
        paste_turn,
        events,
        errors,
    } = services;
    // Every way out of here ends the wait, the early ones included.
    let wait = WaitEnd {
        tag: Some(tag.clone()),
        events,
    };

    let Some(history) = history else {
        log::warning!(App, "[{tag}] no history, the recording is not sent");
        return;
    };
    let file_name = recording
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64());

    // The row goes in first, whatever happens next: a dictation the service
    // cannot handle is still a dictation, and its audio is still on disk.
    let Some(stored) = history
        .add(Transcript::recorded(now, recording.duration, file_name))
        .await
    else {
        return;
    };
    log::info!(
        App,
        "[{tag}] history row {} written after {} ms, queued",
        stored.id,
        milliseconds(key_up_at.elapsed())
    );

    let queued_at = Instant::now();
    let Some(row) = recogniser
        .recognise(stored, recording.path.clone(), history.clone(), tag.clone())
        .await
    else {
        log::info!(App, "[{tag}] sent again from the history, this attempt dropped");
        return;
    };
    log::info!(
        App,
        "[{tag}] service answered after {} ms in the queue, state {}",
        milliseconds(queued_at.elapsed()),
        row.state.as_str()
    );

    // The row first; the spinner goes as soon as the answer is in, not after
    // the paste.
    history.update(&row).await;
    wait.end();

    match row.state {
        TranscriptState::Done => {
            log::info!(Asr, "[{tag}] text: {}", row.text);
            let waited = key_up_at.elapsed();
            if waited > PASTE_WINDOW {
                log::info!(
                    App,
                    "[{tag}] text came {:.1} s after the key, kept in the history only",
                    waited.as_secs_f64()
                );
                return;
            }
            let _turn = paste_turn.lock().await;
            if let Err(error) = sink.deliver(&row.text, &tag).await {
                log::error!(Paste, "[{tag}] {error}");
                errors.show(&error.message());
            }
        }
        TranscriptState::Empty => {
            log::info!(Storage, "[{tag}] no text returned, row kept with its audio");
        }
        TranscriptState::Failed => {
            let reason = row.failure_reason.as_deref().unwrap_or("Recognition failed.");
            log::error!(App, "[{tag}] {reason}");
            errors.show(reason);
        }
        TranscriptState::Recorded | TranscriptState::Sending | TranscriptState::Recognising => {}
    }
}

/// Ends the pill's wait for one dictation when told to, or when dropped.
struct WaitEnd {
    tag: Option<String>,
    events: UnboundedSender<ControllerEvent>,
}

impl WaitEnd {
    fn end(mut self) {
        self.send();
    }

    fn send(&mut self) {
        if let Some(tag) = self.tag.take() {
            let _ = self.events.send(ControllerEvent::StopWaiting(tag));
        }
    }
}

impl Drop for WaitEnd {
    fn drop(&mut self) {
        self.send();
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
