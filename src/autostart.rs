// Start at login: an XDG autostart entry, the counterpart of
// LaunchAtLogin.swift. GNOME, KDE and the other XDG desktops start what is in
// ~/.config/autostart at login; Sway and Hyprland do not (step 8).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "com.advertumtech.Sensay.desktop";

/// `$XDG_CONFIG_HOME/autostart/…`, or `~/.config/autostart/…`.
fn path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("autostart").join(FILE_NAME))
}

pub fn is_enabled() -> bool {
    path().is_some_and(|path| path.exists())
}

pub fn set(enabled: bool) -> io::Result<()> {
    let path = path().ok_or_else(|| io::Error::other("neither XDG_CONFIG_HOME nor HOME is set"))?;
    if !enabled {
        return match fs::remove_file(&path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(&path, entry(&program()))
}

/// The AppImage when running from one, else this executable.
fn program() -> String {
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        return appimage.to_string_lossy().into_owned();
    }
    std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "sensay".to_string())
}

fn entry(program: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Sensay\n\
         Comment=Dictation: hold the key, speak, let go\n\
         Exec={}\n\
         Icon=com.advertumtech.Sensay\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        quote(program)
    )
}

/// Quoting of an Exec argument by the Desktop Entry specification.
fn quote(argument: &str) -> String {
    let mut quoted = String::from("\"");
    for character in argument.chars() {
        if matches!(character, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_is_quoted() {
        assert_eq!(quote("/home/n/My Apps/sensay"), "\"/home/n/My Apps/sensay\"");
        assert_eq!(quote("/tmp/a$b"), "\"/tmp/a\\$b\"");
    }
}
