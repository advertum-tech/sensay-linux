// Window classes and app ids of terminals, where paste is Ctrl+Shift+V. The
// same list as gnome/sensay-pill@sensay.app/paste.js; both come from whisrs
// (MIT), src/daemon/injection.rs.

const TERMINALS: &[&str] = &[
    "alacritty", "blackbox-terminal", "contour", "cool-retro-term", "deepin-terminal",
    "foot", "footclient", "ghostty", "ghostty-debug", "gnome-terminal",
    "gnome-terminal-server", "guake", "hyper", "kgx", "kitty", "koi8rxterm", "konsole",
    "lxterminal", "mate-terminal", "mlterm", "ptyxis", "qterminal", "rio", "roxterm",
    "rxvt", "rxvt-unicode", "sakura", "st", "st-16color", "st-256color", "st-direct",
    "st-mono", "tabby", "terminator", "terminology", "termite", "tilix", "urxvt",
    "urxvtc", "uxterm", "waveterm", "wezterm", "xfce4-terminal", "xterm", "yakuake",
    "com.gexperts.tilix", "com.mitchellh.ghostty", "com.mitchellh.ghostty-debug",
    "com.raggesilver.blackbox", "dev.warp.warp", "io.elementary.terminal",
    "org.contourterminal.contour", "org.gnome.console", "org.gnome.console.devel",
    "org.gnome.ptyxis", "org.gnome.terminal", "org.kde.konsole", "org.kde.yakuake",
    "org.wezfurlong.wezterm", "org.xfce.terminal",
];

/// Last parts of reverse-domain app ids that mean a terminal on their own.
const TERMINAL_LEAVES: &[&str] = &[
    "alacritty", "cool-retro-term", "ghostty", "ghostty-debug", "guake", "kitty",
    "konsole", "ptyxis", "qterminal", "tilix", "waveterm", "wezterm", "yakuake",
];

pub fn is_terminal(window_class: &str) -> bool {
    let name = window_class.trim().to_lowercase();
    if name.is_empty() {
        return false;
    }
    if TERMINALS.contains(&name.as_str()) {
        return true;
    }
    name.rsplit_once('.')
        .is_some_and(|(_, leaf)| TERMINAL_LEAVES.contains(&leaf))
}

#[cfg(test)]
mod tests {
    use super::is_terminal;

    #[test]
    fn knows_terminals_by_class_and_app_id() {
        assert!(is_terminal("Xfce4-terminal"));
        assert!(is_terminal("org.gnome.Ptyxis"));
        assert!(is_terminal("com.example.kitty"));
        assert!(!is_terminal("org.gnome.TextEditor"));
        assert!(!is_terminal(""));
    }
}
