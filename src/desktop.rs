// What desktop the app runs on, for choosing the pill and paste paths, and
// whether it asks for dark windows.

use zbus::Connection;
use zbus::zvariant::{OwnedValue, Value};

/// XDG_CURRENT_DESKTOP names GNOME (Ubuntu says "ubuntu:GNOME") and the
/// session is Wayland. GNOME under X11 takes the X11 paths of step 7.
pub fn is_gnome_wayland() -> bool {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty());
    let gnome = std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|desktops| desktops.split(':').any(|desktop| desktop.eq_ignore_ascii_case("GNOME")));
    wayland && gnome
}

/// A Wayland session: a Wayland display is announced.
pub fn is_wayland() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET")
}

/// Whether winit opens the windows on X11: it takes Wayland when a Wayland
/// display is announced, X11 otherwise.
pub fn windows_use_x11() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    !(set("WAYLAND_DISPLAY") || set("WAYLAND_SOCKET")) && set("DISPLAY")
}

/// Whether the desktop asks for dark windows, from the settings portal
/// (`org.freedesktop.appearance color-scheme`: 1 dark, 2 light, 0 no
/// preference, which GNOME means as light). None when the portal cannot be
/// asked.
pub async fn prefers_dark(connection: &Connection) -> Option<bool> {
    let value = appearance(connection, "color-scheme").await?;
    Some(as_u32(&value)? == 1)
}

/// The desktop's accent colour, red, green and blue in 0...1, from the same
/// portal (GNOME 47 and later). None when there is none to follow.
pub async fn accent_color(connection: &Connection) -> Option<[f64; 3]> {
    let value = appearance(connection, "accent-color").await?;
    let rgb = as_rgb(&value)?;
    // Values out of range mean no preference.
    rgb.iter().all(|channel| (0.0..=1.0).contains(channel)).then_some(rgb)
}

/// One key of `org.freedesktop.appearance` from the settings portal.
async fn appearance(connection: &Connection, key: &str) -> Option<OwnedValue> {
    const DESTINATION: &str = "org.freedesktop.portal.Desktop";
    const PATH: &str = "/org/freedesktop/portal/desktop";
    const INTERFACE: &str = "org.freedesktop.portal.Settings";
    let key = ("org.freedesktop.appearance", key);

    // ReadOne is version 2 of the interface; older portals have only Read,
    // which wraps the value in one more variant.
    let reply = match connection
        .call_method(Some(DESTINATION), PATH, Some(INTERFACE), "ReadOne", &key)
        .await
    {
        Ok(reply) => reply,
        Err(_) => connection
            .call_method(Some(DESTINATION), PATH, Some(INTERFACE), "Read", &key)
            .await
            .ok()?,
    };
    reply.body().deserialize().ok()
}

fn as_rgb(value: &Value<'_>) -> Option<[f64; 3]> {
    match value {
        Value::Value(inner) => as_rgb(inner),
        Value::Structure(structure) => {
            let channel = |value: &Value<'_>| match value {
                Value::F64(channel) => Some(*channel),
                _ => None,
            };
            match structure.fields() {
                [red, green, blue] => Some([channel(red)?, channel(green)?, channel(blue)?]),
                _ => None,
            }
        }
        _ => None,
    }
}

fn as_u32(value: &Value<'_>) -> Option<u32> {
    match value {
        Value::U32(number) => Some(*number),
        Value::Value(inner) => as_u32(inner),
        _ => None,
    }
}
