// sensay setup: the one-time system setup for a copy that did not come from a
// package (the AppImage, a build from source). A package does the same in
// its install script. Running it again is safe: each part is skipped when it
// is in place.
//
//   1. A udev rule that gives the user at the seat the keyboards, where the
//      activation key is read, and /dev/uinput, where the paste keys go on
//      KDE. systemd-logind applies it (TAG+="uaccess") at once: no group to
//      join, no logging out, and a Sensay already running picks the
//      keyboards up. Written to /etc/udev/rules.d after one password prompt
//      (pkexec, else sudo in the terminal).
//   2. On GNOME: the Shell extension that draws the pill and pastes, copied
//      into ~/.local/share/gnome-shell/extensions and switched on. GNOME
//      loads a new extension at the next login.
//
// It runs as the user, not as root: the extension belongs in the user's home.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::desktop;

/// The rule, the same file the packages install into /usr/lib/udev/rules.d.
/// The name sorts it before 73-seat-late.rules, where uaccess is applied.
pub const UDEV_RULE: &str = "\
# Sensay: the activation key is read from the keyboards and the paste keys
# go through uinput, for the user at the seat (systemd-logind, uaccess).
SUBSYSTEM==\"input\", KERNEL==\"event*\", ENV{ID_INPUT_KEYBOARD}==\"1\", TAG+=\"uaccess\"
KERNEL==\"uinput\", SUBSYSTEM==\"misc\", TAG+=\"uaccess\", OPTIONS+=\"static_node=uinput\"
";

const RULE_NAME: &str = "70-sensay.rules";
const RULE_DIRS: [&str; 2] = ["/etc/udev/rules.d", "/usr/lib/udev/rules.d"];

const EXTENSION_UUID: &str = "sensay-pill@sensay.app";
const EXTENSION_FILES: [(&str, &str); 3] = [
    ("metadata.json", include_str!("../gnome/sensay-pill@sensay.app/metadata.json")),
    ("extension.js", include_str!("../gnome/sensay-pill@sensay.app/extension.js")),
    ("paste.js", include_str!("../gnome/sensay-pill@sensay.app/paste.js")),
];
/// Where a package puts the extension for every user.
const SYSTEM_EXTENSIONS: &str = "/usr/share/gnome-shell/extensions";

pub fn run() -> ExitCode {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("sensay setup: run it as your own user, without sudo; it asks for the password itself.");
        return ExitCode::from(2);
    }
    let mut failed = false;

    match installed_rule() {
        Some(path) => println!("Keyboard access: in place ({}).", path.display()),
        None => {
            println!("Keyboard access: installing a udev rule, your password is needed once.");
            match install_rule() {
                Ok(()) => println!("Keyboard access: done, works at once."),
                Err(reason) => {
                    println!("Keyboard access: failed: {reason}");
                    failed = true;
                }
            }
        }
    }

    if desktop::is_gnome() {
        match install_extension() {
            Ok(Extension::InPlace) => println!("GNOME extension: in place and switched on."),
            Ok(Extension::Installed) => {
                println!("GNOME extension: installed and switched on; log out and back in to load it.")
            }
            Err(reason) => {
                println!("GNOME extension: failed: {reason}");
                failed = true;
            }
        }
    }

    if failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// The rule file when one with the same content is installed.
fn installed_rule() -> Option<PathBuf> {
    RULE_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(RULE_NAME))
        .find(|path| std::fs::read_to_string(path).is_ok_and(|text| text == UDEV_RULE))
}

/// Writes the rule into /etc/udev/rules.d and applies it to the devices
/// present now, as root through pkexec, or sudo where pkexec is missing.
fn install_rule() -> Result<(), String> {
    let staged = std::env::temp_dir().join(format!("sensay-{}-{RULE_NAME}", std::process::id()));
    std::fs::write(&staged, UDEV_RULE).map_err(|error| format!("cannot write {}: {error}", staged.display()))?;
    // uinput is loaded first: until /dev/uinput opens, nothing loads the
    // module, and the rule reaches a device that is not there only at the
    // next boot.
    let script = format!(
        "install -D -m 0644 \"$1\" /etc/udev/rules.d/{RULE_NAME} \
         && udevadm control --reload-rules \
         && {{ modprobe uinput || true; }} \
         && udevadm trigger --action=change --subsystem-match=input --subsystem-match=misc \
         && udevadm settle"
    );
    let staged_arg = staged.to_string_lossy().into_owned();
    let mut tried = Vec::new();
    let mut done = false;
    for elevate in ["pkexec", "sudo"] {
        let status = Command::new(elevate)
            .args(["/bin/sh", "-c", &script, "sh", &staged_arg])
            .status();
        match status {
            Ok(status) if status.success() => {
                done = true;
                break;
            }
            // pkexec: the password dialog was closed. No second prompt.
            Ok(status) if elevate == "pkexec" && status.code() == Some(126) => {
                tried.push("the password dialog was closed".to_string());
                break;
            }
            Ok(status) => tried.push(format!("{elevate} ended with {status}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => tried.push(format!("{elevate}: {error}")),
        }
    }
    let _ = std::fs::remove_file(&staged);
    if done {
        Ok(())
    } else if tried.is_empty() {
        Err("neither pkexec nor sudo is installed".to_string())
    } else {
        Err(tried.join("; "))
    }
}

enum Extension {
    InPlace,
    Installed,
}

/// Copies the extension into the user's extensions unless a package put it
/// in for every user, and adds it to the extensions GNOME switches on.
fn install_extension() -> Result<Extension, String> {
    let mut changed = false;
    let system_copy = Path::new(SYSTEM_EXTENSIONS).join(EXTENSION_UUID).join("metadata.json");
    if !system_copy.exists() {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let folder = PathBuf::from(home).join(".local/share/gnome-shell/extensions").join(EXTENSION_UUID);
        std::fs::create_dir_all(&folder).map_err(|error| format!("cannot create {}: {error}", folder.display()))?;
        for (name, content) in EXTENSION_FILES {
            let path = folder.join(name);
            if std::fs::read_to_string(&path).is_ok_and(|text| text == content) {
                continue;
            }
            std::fs::write(&path, content).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            changed = true;
        }
    }
    if switch_on()? {
        changed = true;
    }
    Ok(if changed { Extension::Installed } else { Extension::InPlace })
}

/// Adds the extension to org.gnome.shell enabled-extensions; true when it
/// was not there.
fn switch_on() -> Result<bool, String> {
    let output = Command::new("gsettings")
        .args(["get", "org.gnome.shell", "enabled-extensions"])
        .output()
        .map_err(|error| format!("gsettings: {error}"))?;
    if !output.status.success() {
        return Err(format!("gsettings get ended with {}", output.status));
    }
    let current = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let Some(updated) = with_extension(&current, EXTENSION_UUID) else {
        return Ok(false);
    };
    let status = Command::new("gsettings")
        .args(["set", "org.gnome.shell", "enabled-extensions", &updated])
        .status()
        .map_err(|error| format!("gsettings: {error}"))?;
    if !status.success() {
        return Err(format!("gsettings set ended with {status}"));
    }
    Ok(true)
}

/// The list gsettings printed with `uuid` added; None when it is there.
/// gsettings prints `@as []` for an empty list and `['a', 'b']` otherwise.
fn with_extension(list: &str, uuid: &str) -> Option<String> {
    let quoted = format!("'{uuid}'");
    if list.contains(&quoted) {
        return None;
    }
    let inner = list.trim_start_matches("@as").trim().trim_start_matches('[').trim_end_matches(']').trim();
    Some(if inner.is_empty() {
        format!("[{quoted}]")
    } else {
        format!("[{inner}, {quoted}]")
    })
}

#[cfg(test)]
mod tests {
    use super::with_extension;

    #[test]
    fn adds_the_extension_to_the_list_once() {
        assert_eq!(with_extension("@as []", "a@b").as_deref(), Some("['a@b']"));
        assert_eq!(
            with_extension("['ubuntu-dock@ubuntu.com']", "a@b").as_deref(),
            Some("['ubuntu-dock@ubuntu.com', 'a@b']")
        );
        assert_eq!(with_extension("['a@b', 'x@y']", "a@b"), None);
    }
}
