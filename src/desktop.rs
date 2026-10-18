// What desktop the app runs on, for choosing the pill and paste paths.

/// XDG_CURRENT_DESKTOP names GNOME (Ubuntu says "ubuntu:GNOME") and the
/// session is Wayland. GNOME under X11 takes the X11 paths of step 7.
pub fn is_gnome_wayland() -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|desktops| desktops.split(':').any(|desktop| desktop.eq_ignore_ascii_case("GNOME")));
    wayland && gnome
}
