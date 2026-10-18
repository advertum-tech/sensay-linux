// The history: one SQLite file with the table of the macOS version, and the
// only thing that writes to it (History.swift).
//
// Every row that appears, changes or goes away passes through here and is
// announced as what changed, so the history window (step 6) can move one row
// instead of reading the whole list back. Nothing polls.
//
// The file is reached from one connection behind a lock; every call runs on
// tokio's blocking pool, so the dictation path never waits on the disk in an
// async task.

mod retention;
mod store;

use std::os::unix::fs::DirBuilderExt;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

pub use retention::sweep;
use store::Store;

use crate::log;
use crate::paths;

/// Where one dictation stands. The names are the macOS ones, so the two
/// databases read the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptState {
    /// The audio is on disk, waiting for its turn. Shown as "Queued".
    Recorded,
    /// The audio is going up to the service.
    Sending,
    /// The audio is up; the service is working on the text.
    Recognising,
    /// Text came back.
    Done,
    /// The service refused or did not answer. The reason is on the row.
    Failed,
    /// The service answered, with no text in it.
    Empty,
}

impl TranscriptState {
    pub fn as_str(self) -> &'static str {
        match self {
            TranscriptState::Recorded => "recorded",
            TranscriptState::Sending => "sending",
            TranscriptState::Recognising => "recognising",
            TranscriptState::Done => "done",
            TranscriptState::Failed => "failed",
            TranscriptState::Empty => "empty",
        }
    }

    fn parse(raw: &str) -> TranscriptState {
        match raw {
            "sending" => TranscriptState::Sending,
            "recognising" => TranscriptState::Recognising,
            "done" => TranscriptState::Done,
            "failed" => TranscriptState::Failed,
            "empty" => TranscriptState::Empty,
            _ => TranscriptState::Recorded,
        }
    }
}

/// One dictation.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub id: i64,
    /// Seconds since 1970, as created_at on macOS.
    pub created_at: f64,
    pub state: TranscriptState,
    pub text: String,
    pub language: Option<String>,
    /// Length of the recording, in seconds.
    pub duration: f64,
    /// File name inside the audio folder. None once the audio is removed.
    pub audio_file_name: Option<String>,
    pub model: Option<String>,
    /// How long the service took.
    pub recognition_seconds: Option<f64>,
    /// Filled in for `Failed`.
    pub failure_reason: Option<String>,
}

impl Transcript {
    /// A new row for audio that has just been written.
    pub fn recorded(created_at: f64, duration: f64, audio_file_name: String) -> Transcript {
        Transcript {
            id: 0,
            created_at,
            state: TranscriptState::Recorded,
            text: String::new(),
            language: None,
            duration,
            audio_file_name: Some(audio_file_name),
            model: None,
            recognition_seconds: None,
            failure_reason: None,
        }
    }
}

/// What a watcher is told.
#[derive(Debug, Clone)]
// Read by the history window from step 6 on.
#[allow(dead_code)]
pub enum Change {
    Added(Transcript),
    Changed(Transcript),
    Removed(i64),
}

#[derive(Clone)]
pub struct History {
    store: Arc<Mutex<Store>>,
    changes: broadcast::Sender<Change>,
}

impl History {
    /// Opens the file, creating it and its folder when missing, and brings the
    /// schema up to date.
    pub async fn open() -> Result<History, String> {
        let path = paths::database_path().ok_or("neither XDG_DATA_HOME nor HOME is set")?;
        let store = tokio::task::spawn_blocking(move || {
            if let Some(folder) = path.parent() {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(folder)
                    .map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
            }
            Store::open(&path).map_err(|error| format!("cannot open {}: {error}", path.display()))
        })
        .await
        .map_err(|error| error.to_string())??;
        let (changes, _) = broadcast::channel(256);
        Ok(History {
            store: Arc::new(Mutex::new(store)),
            changes,
        })
    }

    /// Changes from now on.
    // The history window subscribes from step 6 on.
    #[allow(dead_code)]
    pub fn watch(&self) -> broadcast::Receiver<Change> {
        self.changes.subscribe()
    }

    fn announce(&self, change: Change) {
        // No watcher is not an error.
        let _ = self.changes.send(change);
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, String> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = store.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            work(&mut store).map_err(|error| error.to_string())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// Returns the row with the id the database gave it, or None if it could
    /// not be written. A dictation carries on either way; only the record is
    /// lost.
    pub async fn add(&self, row: Transcript) -> Option<Transcript> {
        match self.run(move |store| store.add(&row)).await {
            Ok(stored) => {
                self.announce(Change::Added(stored.clone()));
                Some(stored)
            }
            Err(error) => {
                log::error!(Storage, "could not add the history row: {error}");
                None
            }
        }
    }

    pub async fn update(&self, row: &Transcript) {
        let written = row.clone();
        match self.run(move |store| store.update(&written)).await {
            Ok(()) => self.announce(Change::Changed(row.clone())),
            Err(error) => log::error!(Storage, "could not update row {}: {error}", row.id),
        }
    }

    /// Newest first.
    pub async fn all(&self, limit: usize) -> Vec<Transcript> {
        match self.run(move |store| store.all(limit)).await {
            Ok(rows) => rows,
            Err(error) => {
                log::error!(Storage, "could not read the history: {error}");
                Vec::new()
            }
        }
    }

    /// Rows a previous run left queued, sending or being transcribed. They are
    /// not sent again on their own: each gets a reason and the retry button,
    /// and nothing goes out unasked.
    pub async fn mark_interrupted(&self) {
        let stuck = match self
            .run(|store| {
                store.in_states(&[
                    TranscriptState::Recorded,
                    TranscriptState::Sending,
                    TranscriptState::Recognising,
                ])
            })
            .await
        {
            Ok(stuck) => stuck,
            Err(error) => {
                log::error!(Storage, "could not read unfinished rows: {error}");
                return;
            }
        };
        for mut row in stuck.iter().cloned() {
            row.state = TranscriptState::Failed;
            row.failure_reason = Some("Interrupted".to_string());
            self.update(&row).await;
        }
        if !stuck.is_empty() {
            log::info!(Storage, "{} unfinished rows marked as interrupted", stuck.len());
        }
    }

    /// Clears the audio reference of these rows. The rows and their text stay,
    /// so each one is a change, not a removal.
    pub async fn forget_audio(&self, ids: Vec<i64>) {
        if ids.is_empty() {
            return;
        }
        let rows = match self.run(move |store| store.forget_audio(&ids)).await {
            Ok(rows) => rows,
            Err(error) => {
                log::error!(Storage, "could not clear audio references: {error}");
                return;
            }
        };
        for row in rows {
            self.announce(Change::Changed(row));
        }
    }
}
