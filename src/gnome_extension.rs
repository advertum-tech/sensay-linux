// The GNOME Shell extension sensay-pill@sensay.app (gnome/ in this
// repository), which draws the pill and pastes on GNOME. The app puts in the
// copy that fits the running GNOME and switches it on at every launch on
// GNOME, and sensay setup does the same:
//
//   GNOME 45 and later   the ES module copy: from a package, in
//                        /usr/share/gnome-shell/extensions, or else in
//                        ~/.local/share/gnome-shell/extensions
//   GNOME 42–44          the old-style copy build.rs makes, always in
//                        ~/.local/share, where it takes the place of a
//                        package's copy with the same uuid
//
// The copy in effect is always the one built into this binary. GNOME reads
// new or changed extension files only at the next login.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths;

pub const UUID: &str = "sensay-pill@sensay.app";

type Files = [(&'static str, &'static str); 3];

const FILES: Files = [
    ("metadata.json", include_str!("../gnome/sensay-pill@sensay.app/metadata.json")),
    ("extension.js", include_str!("../gnome/sensay-pill@sensay.app/extension.js")),
    ("paste.js", include_str!("../gnome/sensay-pill@sensay.app/paste.js")),
];

const LEGACY_FILES: Files = [
    ("metadata.json", include_str!(concat!(env!("OUT_DIR"), "/legacy/metadata.json"))),
    ("extension.js", include_str!(concat!(env!("OUT_DIR"), "/legacy/extension.js"))),
    ("paste.js", include_str!(concat!(env!("OUT_DIR"), "/legacy/paste.js"))),
];

/// The first GNOME that loads extensions as ES modules.
const FIRST_MODULE_GNOME: u32 = 45;

/// What ensure() did.
pub struct Ensured {
    /// Files were written or removed: GNOME loads them at the next login.
    pub files_changed: bool,
    /// The extension was not in the list of extensions switched on.
    pub switched_on: bool,
}

/// Puts the copy for the running GNOME where GNOME takes it from and
/// switches it on.
pub fn ensure() -> Result<Ensured, String> {
    let legacy = shell_version().is_some_and(|major| major < FIRST_MODULE_GNOME);
    let files = if legacy { &LEGACY_FILES } else { &FILES };
    let user = user_folder().ok_or("HOME is not set")?;

    let package_fits = system_copy().is_some_and(|folder| holds(&folder, files));
    let files_changed = if package_fits {
        // A copy of the user's own would hide the package's.
        match std::fs::remove_dir_all(&user) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("cannot remove {}: {error}", user.display())),
        }
    } else {
        write(&user, files)?
    };
    let switched_on = switch_on()?;
    Ok(Ensured {
        files_changed,
        switched_on,
    })
}

/// The major version of the running GNOME Shell, from `gnome-shell
/// --version` ("GNOME Shell 42.9").
fn shell_version() -> Option<u32> {
    let output = Command::new("gnome-shell").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = text.trim().rsplit(' ').next()?;
    version.split('.').next()?.parse().ok()
}

/// ~/.local/share/gnome-shell/extensions/<uuid>, or under XDG_DATA_HOME.
fn user_folder() -> Option<PathBuf> {
    paths::data_home().map(|data| data.join("gnome-shell/extensions").join(UUID))
}

/// The copy a package put in for every user, in one of XDG_DATA_DIRS.
fn system_copy() -> Option<PathBuf> {
    let dirs = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    dirs.split(':')
        .map(|dir| Path::new(dir).join("gnome-shell/extensions").join(UUID))
        .find(|folder| folder.join("metadata.json").is_file())
}

/// Whether `folder` holds exactly these files' contents.
fn holds(folder: &Path, files: &Files) -> bool {
    files
        .iter()
        .all(|(name, content)| std::fs::read_to_string(folder.join(name)).is_ok_and(|text| text == *content))
}

/// Writes the files into `folder`; true when any changed.
fn write(folder: &Path, files: &Files) -> Result<bool, String> {
    std::fs::create_dir_all(folder).map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
    let mut changed = false;
    for (name, content) in files {
        let path = folder.join(name);
        if std::fs::read_to_string(&path).is_ok_and(|text| text == *content) {
            continue;
        }
        std::fs::write(&path, content).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        changed = true;
    }
    Ok(changed)
}

/// Adds the extension to org.gnome.shell enabled-extensions and takes it out
/// of disabled-extensions, which wins over the first list; true when either
/// list changed. GNOME switches on an extension it knows at once, and one it
/// does not know yet at the next login.
fn switch_on() -> Result<bool, String> {
    let enabled = with_extension(&read_list("enabled-extensions")?, UUID);
    let disabled = without_extension(&read_list("disabled-extensions")?, UUID);
    let changed = enabled.is_some() || disabled.is_some();
    if let Some(list) = disabled {
        write_list("disabled-extensions", &list)?;
    }
    if let Some(list) = enabled {
        write_list("enabled-extensions", &list)?;
    }
    Ok(changed)
}

fn read_list(key: &str) -> Result<String, String> {
    let output = Command::new("gsettings")
        .args(["get", "org.gnome.shell", key])
        .output()
        .map_err(|error| format!("gsettings: {error}"))?;
    if !output.status.success() {
        return Err(format!("gsettings get {key} ended with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn write_list(key: &str, list: &str) -> Result<(), String> {
    let status = Command::new("gsettings")
        .args(["set", "org.gnome.shell", key, list])
        .status()
        .map_err(|error| format!("gsettings: {error}"))?;
    if !status.success() {
        return Err(format!("gsettings set {key} ended with {status}"));
    }
    Ok(())
}

/// The uuids of a list as gsettings prints it: `@as []` when empty,
/// `['a', 'b']` otherwise.
fn entries(list: &str) -> Vec<&str> {
    list.trim_start_matches("@as")
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|entry| entry.trim().trim_matches('\''))
        .filter(|entry| !entry.is_empty())
        .collect()
}

fn printed(entries: &[&str]) -> String {
    let quoted: Vec<String> = entries.iter().map(|entry| format!("'{entry}'")).collect();
    format!("[{}]", quoted.join(", "))
}

/// The list with `uuid` added; None when it is there.
fn with_extension(list: &str, uuid: &str) -> Option<String> {
    let mut entries = entries(list);
    if entries.contains(&uuid) {
        return None;
    }
    entries.push(uuid);
    Some(printed(&entries))
}

/// The list without `uuid`; None when it is not there.
fn without_extension(list: &str, uuid: &str) -> Option<String> {
    let entries = entries(list);
    if !entries.contains(&uuid) {
        return None;
    }
    let kept: Vec<&str> = entries.into_iter().filter(|entry| *entry != uuid).collect();
    Some(printed(&kept))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_the_extension_to_the_list_once() {
        assert_eq!(with_extension("@as []", "a@b").as_deref(), Some("['a@b']"));
        assert_eq!(
            with_extension("['ubuntu-dock@ubuntu.com']", "a@b").as_deref(),
            Some("['ubuntu-dock@ubuntu.com', 'a@b']")
        );
        assert_eq!(with_extension("['a@b', 'x@y']", "a@b"), None);
    }

    #[test]
    fn takes_the_extension_out_of_the_list() {
        assert_eq!(without_extension("['x@y', 'a@b']", "a@b").as_deref(), Some("['x@y']"));
        assert_eq!(without_extension("['a@b']", "a@b").as_deref(), Some("[]"));
        assert_eq!(without_extension("@as []", "a@b"), None);
    }

    #[test]
    fn legacy_copy_is_an_old_style_module() {
        let [(_, metadata), (_, extension), (_, paste)] = LEGACY_FILES;
        assert!(metadata.contains("[\"42\", \"43\", \"44\"]"));
        for module in [extension, paste] {
            assert!(!module.lines().any(|line| line.starts_with("import ") || line.starts_with("export ")));
        }
        assert!(extension.contains("const Main = imports.ui.main;"));
        assert!(extension.contains("imports.misc.extensionUtils.getCurrentExtension().imports.paste;"));
        assert!(extension.contains("\nfunction init() {\n    return new SensayPillExtension();\n}\n"));
        assert!(paste.contains("var Paster = class Paster {"));
    }
}
