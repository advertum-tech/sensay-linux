// The SQLite file itself (TranscriptStore.swift). The table is the macOS one,
// column for column, so a database from either version reads the same.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{Transcript, TranscriptState};

/// Bumped whenever the schema changes; `migrate` walks from whatever the file
/// has to this number.
const SCHEMA_VERSION: i64 = 1;

const COLUMNS: &str = "id, created_at, state, text, language, duration, \
                       audio_file_name, model, recognition_seconds, failure_reason";

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        Store::with(Connection::open(path)?)
    }

    fn with(connection: Connection) -> rusqlite::Result<Store> {
        connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        let store = Store { connection };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        let current: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?;
        if current >= SCHEMA_VERSION {
            return Ok(());
        }
        if current < 1 {
            self.connection.execute_batch(
                "CREATE TABLE transcripts (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at REAL NOT NULL,
                    state TEXT NOT NULL,
                    text TEXT NOT NULL DEFAULT '',
                    language TEXT,
                    duration REAL NOT NULL DEFAULT 0,
                    audio_file_name TEXT,
                    model TEXT,
                    recognition_seconds REAL,
                    failure_reason TEXT
                );
                CREATE INDEX transcripts_created_at ON transcripts (created_at DESC);",
            )?;
        }
        self.connection
            .pragma_update(None, "user_version", SCHEMA_VERSION)
    }

    /// Inserts a row and returns it with the id the database gave it.
    pub fn add(&mut self, row: &Transcript) -> rusqlite::Result<Transcript> {
        self.connection.execute(
            "INSERT INTO transcripts
             (created_at, state, text, language, duration, audio_file_name, model,
              recognition_seconds, failure_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                row.created_at,
                row.state.as_str(),
                row.text,
                row.language,
                row.duration,
                row.audio_file_name,
                row.model,
                row.recognition_seconds,
                row.failure_reason,
            ],
        )?;
        let mut stored = row.clone();
        stored.id = self.connection.last_insert_rowid();
        Ok(stored)
    }

    /// Writes every field of an existing row.
    pub fn update(&mut self, row: &Transcript) -> rusqlite::Result<()> {
        self.connection.execute(
            "UPDATE transcripts SET
                created_at = ?1, state = ?2, text = ?3, language = ?4, duration = ?5,
                audio_file_name = ?6, model = ?7, recognition_seconds = ?8, failure_reason = ?9
             WHERE id = ?10",
            params![
                row.created_at,
                row.state.as_str(),
                row.text,
                row.language,
                row.duration,
                row.audio_file_name,
                row.model,
                row.recognition_seconds,
                row.failure_reason,
                row.id,
            ],
        )?;
        Ok(())
    }

    /// Clears the audio reference of these rows and returns them as they are
    /// now.
    pub fn forget_audio(&mut self, ids: &[i64]) -> rusqlite::Result<Vec<Transcript>> {
        let transaction = self.connection.transaction()?;
        for id in ids {
            transaction.execute("UPDATE transcripts SET audio_file_name = NULL WHERE id = ?1", [id])?;
        }
        transaction.commit()?;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = self.transcript(*id)? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    pub fn delete(&mut self, id: i64) -> rusqlite::Result<()> {
        self.connection.execute("DELETE FROM transcripts WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn transcript(&self, id: i64) -> rusqlite::Result<Option<Transcript>> {
        self.connection
            .query_row(&format!("SELECT {COLUMNS} FROM transcripts WHERE id = ?1"), [id], read)
            .optional()
    }

    /// Newest first.
    pub fn all(&self, limit: usize) -> rusqlite::Result<Vec<Transcript>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {COLUMNS} FROM transcripts ORDER BY created_at DESC, id DESC LIMIT ?1"
        ))?;
        let rows = statement.query_map([limit as i64], read)?;
        rows.collect()
    }

    /// Rows in any of these states, newest first.
    pub fn in_states(&self, states: &[TranscriptState]) -> rusqlite::Result<Vec<Transcript>> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; states.len()].join(", ");
        let mut statement = self.connection.prepare(&format!(
            "SELECT {COLUMNS} FROM transcripts WHERE state IN ({marks}) ORDER BY created_at DESC, id DESC"
        ))?;
        let names: Vec<&str> = states.iter().map(|state| state.as_str()).collect();
        let rows = statement.query_map(rusqlite::params_from_iter(names), read)?;
        rows.collect()
    }
}

fn read(row: &Row<'_>) -> rusqlite::Result<Transcript> {
    let state: String = row.get(2)?;
    Ok(Transcript {
        id: row.get(0)?,
        created_at: row.get(1)?,
        state: TranscriptState::parse(&state),
        text: row.get(3)?,
        language: row.get(4)?,
        duration: row.get(5)?,
        audio_file_name: row.get(6)?,
        model: row.get(7)?,
        recognition_seconds: row.get(8)?,
        failure_reason: row.get(9)?,
        notice: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::with(Connection::open_in_memory().unwrap()).unwrap()
    }

    #[test]
    fn a_row_goes_in_and_comes_back_with_its_changes() {
        let mut store = store();
        let mut row = store
            .add(&Transcript::recorded(1_760_000_000.5, 2.25, "dictation.wav".to_string()))
            .unwrap();
        assert!(row.id > 0);

        row.state = TranscriptState::Done;
        row.text = "Привет, мир.".to_string();
        row.language = Some("ru".to_string());
        row.recognition_seconds = Some(1.5);
        store.update(&row).unwrap();

        assert_eq!(store.transcript(row.id).unwrap(), Some(row));
    }

    #[test]
    fn unfinished_rows_are_found_by_state() {
        let mut store = store();
        let queued = store.add(&Transcript::recorded(1.0, 1.0, "a.wav".to_string())).unwrap();
        let mut done = store.add(&Transcript::recorded(2.0, 1.0, "b.wav".to_string())).unwrap();
        done.state = TranscriptState::Done;
        store.update(&done).unwrap();

        let found = store
            .in_states(&[TranscriptState::Recorded, TranscriptState::Sending, TranscriptState::Recognising])
            .unwrap();
        assert_eq!(found, vec![queued]);
    }

    #[test]
    fn forgetting_audio_keeps_the_row() {
        let mut store = store();
        let row = store.add(&Transcript::recorded(1.0, 1.0, "a.wav".to_string())).unwrap();
        let rows = store.forget_audio(&[row.id]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].audio_file_name, None);
        assert_eq!(store.all(10).unwrap().len(), 1);
    }
}
