// Which format the old clipboard is kept in, for the X11 and Wayland paths.
// One format only, as on GNOME (paste.js): reading every format makes some
// programs produce each of them, which can take seconds
// (docs/linux/linux-text-input.md).

/// Formats that are not data: requests, answers about the owner, or ids of
/// the owner's own X resources. Never kept.
const NOT_DATA: &[&str] = &[
    "TARGETS", "TIMESTAMP", "MULTIPLE", "SAVE_TARGETS", "DELETE", "INSERT_SELECTION",
    "INSERT_PROPERTY", "PIXMAP", "BITMAP", "DRAWABLE", "COLORMAP", "PROCESS",
    "CLIENT_WINDOW", "HOST_NAME", "LENGTH", "GTK_TEXT_BUFFER_CONTENTS",
];

/// Names text is asked for by, UTF-8 under all of them.
pub const TEXT_FORMATS: [&str; 5] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "TEXT", "STRING"];

/// Whether a kept format holds UTF-8 text, so it can be handed out under
/// every text name.
pub fn is_utf8_text(name: &str) -> bool {
    matches!(name, "text/plain;charset=utf-8" | "text/plain" | "UTF8_STRING")
}

/// Which of the offered formats the old clipboard is kept in: files first,
/// so a copy in the file manager still pastes as files; then an image; then
/// text.
pub fn pick_format(names: &[String]) -> Option<usize> {
    let find = |wanted: &str| names.iter().position(|name| name == wanted);
    for wanted in ["x-special/gnome-copied-files", "text/uri-list", "image/png"] {
        if let Some(index) = find(wanted) {
            return Some(index);
        }
    }
    if let Some(index) = names.iter().position(|name| name.starts_with("image/")) {
        return Some(index);
    }
    for wanted in ["UTF8_STRING", "text/plain;charset=utf-8", "text/plain", "STRING"] {
        if let Some(index) = find(wanted) {
            return Some(index);
        }
    }
    names.iter().position(|name| !NOT_DATA.contains(&name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::pick_format;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn keeps_files_then_images_then_text() {
        let offered = names(&["TARGETS", "UTF8_STRING", "image/png", "text/uri-list"]);
        assert_eq!(pick_format(&offered), Some(3));
        let offered = names(&["TARGETS", "TIMESTAMP", "UTF8_STRING", "image/jpeg"]);
        assert_eq!(pick_format(&offered), Some(3));
        let offered = names(&["TARGETS", "TIMESTAMP", "text/plain", "UTF8_STRING"]);
        assert_eq!(pick_format(&offered), Some(3));
        let offered = names(&["text/plain;charset=utf-8", "text/html"]);
        assert_eq!(pick_format(&offered), Some(0));
        let offered = names(&["TARGETS", "TIMESTAMP", "application/x-custom"]);
        assert_eq!(pick_format(&offered), Some(2));
        assert_eq!(pick_format(&names(&["TARGETS", "TIMESTAMP", "MULTIPLE"])), None);
    }
}
