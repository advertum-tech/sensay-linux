// Which recordings have outlived their audio (AudioRetention.swift), and the
// sweep that deletes them (AudioFiles.swift).
//
// Only `done` rows age out. For every other state the audio is the reason the
// row exists: it is what makes a second attempt possible. The text is never
// deleted. Sweeping runs at launch and when the history window opens, never
// on the dictation path.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{History, Transcript, TranscriptState};
use crate::log;
use crate::paths;

/// How long the audio of a finished dictation is kept: seven days.
const MAX_AGE: f64 = 7.0 * 24.0 * 60.0 * 60.0;

/// Rows whose audio should go, measured from `now` (seconds since 1970).
fn expired(rows: &[Transcript], now: f64) -> Vec<&Transcript> {
    rows.iter()
        .filter(|row| row.audio_file_name.is_some())
        .filter(|row| row.state == TranscriptState::Done)
        .filter(|row| now - row.created_at > MAX_AGE)
        .collect()
}

/// Deletes audio whose keeping time is up and clears the reference on those
/// rows.
pub async fn sweep(history: History) {
    let Some(folder) = paths::audio_folder() else { return };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64());
    let rows = history.all(5000).await;

    let mut removed = Vec::new();
    for row in expired(&rows, now) {
        let Some(name) = &row.audio_file_name else { continue };
        match std::fs::remove_file(folder.join(name)) {
            Ok(()) => removed.push(row.id),
            Err(error) if error.kind() == io::ErrorKind::NotFound => removed.push(row.id),
            Err(error) => log::error!(Storage, "could not delete {name}: {error}"),
        }
    }
    if removed.is_empty() {
        return;
    }
    let count = removed.len();
    history.forget_audio(removed).await;
    log::info!(Storage, "swept {count} recordings");
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: f64 = 24.0 * 60.0 * 60.0;

    fn row(state: TranscriptState, age_days: f64, now: f64) -> Transcript {
        let mut row = Transcript::recorded(now - age_days * DAY, 1.0, "a.wav".to_string());
        row.state = state;
        row
    }

    #[test]
    fn only_old_finished_rows_lose_their_audio() {
        let now = 1_760_000_000.0;
        let rows = vec![
            row(TranscriptState::Done, 8.0, now),
            row(TranscriptState::Done, 6.0, now),
            row(TranscriptState::Failed, 30.0, now),
            row(TranscriptState::Empty, 30.0, now),
            row(TranscriptState::Recorded, 30.0, now),
        ];
        let gone = expired(&rows, now);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].created_at, now - 8.0 * DAY);
    }
}
