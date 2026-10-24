// Where Sensay keeps its files, by the XDG base directory rules. The
// counterpart of AppFolder.swift.

use std::path::{Path, PathBuf};

/// `$XDG_DATA_HOME`, or `~/.local/share` when the variable is unset, empty
/// or relative. None without HOME.
pub fn data_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => Some(PathBuf::from(dir)),
        _ => Some(PathBuf::from(std::env::var_os("HOME")?).join(".local").join("share")),
    }
}

/// `$XDG_DATA_HOME/sensay`. None without HOME.
pub fn data_folder() -> Option<PathBuf> {
    data_home().map(|base| base.join("sensay"))
}

/// The recordings, one WAV file per dictation.
pub fn audio_folder() -> Option<PathBuf> {
    data_folder().map(|folder| folder.join("audio"))
}

/// The history, an SQLite file with the table of the macOS version.
pub fn database_path() -> Option<PathBuf> {
    data_folder().map(|folder| folder.join("history.sqlite"))
}
