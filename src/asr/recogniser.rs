// Turns a recorded row into a recognised one, one row at a time, as
// Recogniser.swift.
//
// The same path serves the first attempt straight after a dictation and a
// second one started from the history window (step 6). A row waiting its turn
// is sent with the settings in force when its turn comes, so switching
// services moves everything not yet sent too.
//
// A row sent again starts a new attempt; the one before it is cancelled, and
// its answer, if one still comes, is thrown away. Every stage a row reaches
// is written to the history as it happens.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use reqwest::Client;
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;

use super::{ServiceSettings, transcribe};
use crate::history::{History, Transcript, TranscriptState};
use crate::log;

#[derive(Clone)]
pub struct Recogniser {
    inner: Arc<Inner>,
}

struct Inner {
    client: Option<Client>,
    settings: watch::Receiver<ServiceSettings>,
    /// Held for the whole of one request. Tokio's mutex hands itself out in
    /// the order it was asked for, which is what makes this a queue.
    turn: tokio::sync::Mutex<()>,
    attempts: Mutex<Attempts>,
}

#[derive(Default)]
struct Attempts {
    last: u64,
    /// The latest attempt for each row still out.
    current: HashMap<i64, u64>,
    running: HashMap<i64, AbortHandle>,
}

impl Recogniser {
    pub fn new(settings: watch::Receiver<ServiceSettings>) -> Recogniser {
        Recogniser {
            inner: Arc::new(Inner {
                client: super::client(),
                settings,
                turn: tokio::sync::Mutex::new(()),
                attempts: Mutex::new(Attempts::default()),
            }),
        }
    }

    /// Waits for its turn, sends, and returns the row with its result. A row
    /// that fails comes back with the reason on it: a failed dictation is a
    /// history entry, not a lost one.
    ///
    /// None when the row was sent again while this attempt was still out:
    /// the newer attempt owns the row from then on, and the caller writes
    /// nothing.
    pub async fn recognise(&self, row: Transcript, audio: PathBuf, history: History, tag: String) -> Option<Transcript> {
        let id = row.id;
        let (attempt, work) = {
            let mut attempts = self.inner.attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            attempts.last += 1;
            let attempt = attempts.last;
            attempts.current.insert(id, attempt);
            // An earlier attempt for the same row is cancelled. If it was at
            // the head of the queue, this is also what lets the rows behind
            // it go.
            if let Some(earlier) = attempts.running.remove(&id) {
                earlier.abort();
            }
            let inner = self.inner.clone();
            let work = tokio::spawn(async move { inner.send(row, audio, attempt, history, tag).await });
            attempts.running.insert(id, work.abort_handle());
            (attempt, work)
        };

        let result = work.await;
        let mut attempts = self.inner.attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if attempts.current.get(&id) != Some(&attempt) {
            return None;
        }
        attempts.current.remove(&id);
        attempts.running.remove(&id);
        match result {
            Ok(row) => Some(row),
            Err(error) => {
                log::error!(Asr, "recognition of row {id} stopped: {error}");
                None
            }
        }
    }
}

impl Inner {
    fn is_current(&self, id: i64, attempt: u64) -> bool {
        let attempts = self.attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        attempts.current.get(&id) == Some(&attempt)
    }

    async fn send(&self, mut row: Transcript, audio: PathBuf, attempt: u64, history: History, tag: String) -> Transcript {
        let queued_at = Instant::now();
        let _turn = self.turn.lock().await;
        if !self.is_current(row.id, attempt) {
            return row;
        }

        row.state = TranscriptState::Sending;
        history.update(&row).await;
        log::info!(App, "[{tag}] sending after {} ms in the queue", queued_at.elapsed().as_millis());

        // Read at this moment, not when the row was queued.
        let settings = self.settings.borrow().clone();
        let timeout = super::timeout(row.duration);
        let started = Instant::now();
        log::info!(Asr, "[{tag}] to {}", settings.service.log_name());

        // The end of the upload is written in this same task, before the
        // answer is, so a late "recognising" can never land on a finished row.
        let (sent, mut upload_done) = oneshot::channel();
        let request = transcribe(self.client.as_ref(), &settings, &audio, timeout, sent);
        tokio::pin!(request);
        let mut upload_seen = false;
        let result = loop {
            tokio::select! {
                result = &mut request => break result,
                done = &mut upload_done, if !upload_seen => {
                    upload_seen = true;
                    if done.is_ok() && self.is_current(row.id, attempt) {
                        let mut stage = row.clone();
                        stage.state = TranscriptState::Recognising;
                        history.update(&stage).await;
                        log::info!(App, "[{tag}] recognising after {} ms in the queue", queued_at.elapsed().as_millis());
                    }
                }
            }
        };

        let elapsed = started.elapsed().as_secs_f64();
        match result {
            Ok(transcription) => {
                // An empty answer argues for silence but does not prove it:
                // the row stays, with its audio, and can be sent again.
                row.state = if transcription.text.trim().is_empty() {
                    TranscriptState::Empty
                } else {
                    TranscriptState::Done
                };
                log::info!(
                    Asr,
                    "[{tag}] recognised in {elapsed:.2} s, {} characters",
                    transcription.text.chars().count()
                );
                row.text = transcription.text;
                row.language = transcription.language;
                row.recognition_seconds = Some(elapsed);
                row.failure_reason = None;
            }
            Err(error) => {
                row.state = TranscriptState::Failed;
                row.failure_reason = Some(error.history_text());
                row.notice = Some(error.message());
                log::error!(Asr, "[{tag}] failed after {elapsed:.2} s: {error:?}");
            }
        }
        row
    }
}
