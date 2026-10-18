// Where Sensay keeps its files, by the XDG base directory rules. The
// counterpart of AppFolder.swift.

use std::path::{Path, PathBuf};

/// `$XDG_DATA_HOME/sensay`, or `~/.local/share/sensay` when the variable is
/// unset, empty or relative. None without HOME.
pub fn data_folder() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".local").join("share"),
    };
    Some(base.join("sensay"))
}

/// The recordings, one WAV file per dictation.
pub fn audio_folder() -> Option<PathBuf> {
    data_folder().map(|folder| folder.join("audio"))
}

/// The history, an SQLite file with the table of the macOS version.
pub fn database_path() -> Option<PathBuf> {
    data_folder().map(|folder| folder.join("history.sqlite"))
}
