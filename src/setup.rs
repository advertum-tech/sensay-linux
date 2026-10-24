// sensay setup: the one-time system setup for a copy that did not come from a
// package (the AppImage, a build from source). A package does the same in
// its install script, and the app puts the GNOME extension in at launch.
// Running it again is safe: each part is skipped when it is in place.
//
//   1. A udev rule that gives the user at the seat the keyboards, where the
//      activation key is read, and /dev/uinput, where the paste keys go on
//      KDE. systemd-logind applies it (TAG+="uaccess") at once: no group to
//      join, no logging out, and a Sensay already running picks the
//      keyboards up. Written to /etc/udev/rules.d after one password prompt:
//      sudo in the terminal, else pkexec.
//   2. On GNOME: the Shell extension that draws the pill and pastes, put in
//      and switched on as the app does at launch (gnome_extension.rs). GNOME
//      loads a new extension at the next login.
//   3. From an AppImage: the menu entry and the icon in ~/.local/share, the
//      entry starting the AppImage where it lies now.
//   4. On Sway and Hyprland, which do not run autostart entries: the line
//      that starts Sensay at login, for the user to put in their config.
//
// It runs as the user, not as root: the extension belongs in the user's home.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use crate::{autostart, desktop, gnome_extension, paths};

/// The rule, the same file the packages install into /usr/lib/udev/rules.d.
/// The name sorts it before 73-seat-late.rules, where uaccess is applied.
pub const UDEV_RULE: &str = include_str!("../data/70-sensay.rules");

const RULE_NAME: &str = "70-sensay.rules";
const RULE_DIRS: [&str; 2] = ["/etc/udev/rules.d", "/usr/lib/udev/rules.d"];

const DESKTOP_ENTRY: &str = include_str!("../data/com.advertumtech.Sensay.desktop");
const DESKTOP_ENTRY_NAME: &str = "com.advertumtech.Sensay.desktop";
const ICON: &str = include_str!("../data/icons/hicolor/scalable/apps/com.advertumtech.Sensay.svg");
const ICON_PATH: &str = "icons/hicolor/scalable/apps/com.advertumtech.Sensay.svg";

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
        match gnome_extension::ensure() {
            Ok(ensured) if ensured.files_changed => {
                println!("GNOME extension: installed and switched on; log out and back in to load it.")
            }
            Ok(ensured) if ensured.switched_on => println!("GNOME extension: switched on."),
            Ok(_) => println!("GNOME extension: in place and switched on."),
            Err(reason) => {
                println!("GNOME extension: failed: {reason}");
                failed = true;
            }
        }
    }

    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        match install_menu_entry(&appimage.to_string_lossy()) {
            Ok(path) => println!("Menu entry: {}.", path.display()),
            Err(reason) => {
                println!("Menu entry: failed: {reason}");
                failed = true;
            }
        }
    }

    if let Some((desktop, file, line)) = autostart_line() {
        println!("Start at login: {desktop} does not run autostart entries; add this line to {file}:");
        println!("    {line}");
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
/// present now, as root: through sudo when run in a terminal, else through
/// pkexec. pkexec asks from a terminal through polkit's own helper, which
/// fails where polkit finds no session for it ("No session for cookie",
/// Ubuntu 26.04, 24 October 2026); sudo asks in the terminal itself.
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
    // SAFETY: isatty has no preconditions.
    let terminal = unsafe { libc::isatty(0) } == 1;
    let order = if terminal { ["sudo", "pkexec"] } else { ["pkexec", "sudo"] };
    let mut tried = Vec::new();
    let mut done = false;
    for elevate in order {
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

/// Writes the menu entry for the AppImage and the icon into the user's data
/// folder; returns where the entry went.
fn install_menu_entry(appimage: &str) -> Result<PathBuf, String> {
    let data = paths::data_home().ok_or("HOME is not set")?;
    let entry = data.join("applications").join(DESKTOP_ENTRY_NAME);
    let icon = data.join(ICON_PATH);
    for (path, content) in [(&entry, menu_entry(appimage)), (&icon, ICON.to_string())] {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        }
        std::fs::write(path, content).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    }
    Ok(entry)
}

/// The packages' entry with each Exec starting `program`.
fn menu_entry(program: &str) -> String {
    let program = autostart::quote(program);
    DESKTOP_ENTRY
        .lines()
        .map(|line| match line.strip_prefix("Exec=sensay") {
            Some(arguments) => format!("Exec={program}{arguments}\n"),
            None => format!("{line}\n"),
        })
        .collect()
}

/// For Sway and Hyprland: the desktop, its config file and the line that
/// starts Sensay at login.
fn autostart_line() -> Option<(&'static str, &'static str, String)> {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    let program = autostart::quote(&autostart::program());
    if set("HYPRLAND_INSTANCE_SIGNATURE") {
        Some(("Hyprland", "~/.config/hypr/hyprland.conf", format!("exec-once = {program}")))
    } else if set("SWAYSOCK") {
        Some(("Sway", "~/.config/sway/config", format!("exec {program}")))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::menu_entry;

    #[test]
    fn menu_entry_starts_the_appimage() {
        let entry = menu_entry("/home/u/Apps/Sensay.AppImage");
        let execs: Vec<&str> = entry.lines().filter(|line| line.starts_with("Exec=")).collect();
        assert_eq!(
            execs,
            [
                "Exec=\"/home/u/Apps/Sensay.AppImage\"",
                "Exec=\"/home/u/Apps/Sensay.AppImage\" history",
                "Exec=\"/home/u/Apps/Sensay.AppImage\" settings",
            ]
        );
        assert!(entry.contains("Icon=com.advertumtech.Sensay\n"));
    }
}
