// The scale an X11 desktop asks for: read by the pill, and at launch for the
// windows, since winit takes it from Xft.dpi alone. It is kept in two
// places. Xfce at 2x sets Gdk/WindowScalingFactor to 2 and leaves Xft.dpi at
// 96; a desktop that sets Xft.dpi to 192 for 2x is read from there.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

pub struct Scale {
    pub value: f32,
    pub dpi: Option<f32>,
    pub factor: Option<i32>,
}

impl Scale {
    /// For the log: "scale 2 (Xft.dpi 96, Gdk/WindowScalingFactor 2)".
    pub fn describe(&self) -> String {
        format!(
            "scale {} (Xft.dpi {}, Gdk/WindowScalingFactor {})",
            self.value,
            self.dpi.map_or("unset".to_string(), |dpi| dpi.to_string()),
            self.factor.map_or("unset".to_string(), |factor| factor.to_string())
        )
    }
}

/// The scale of this screen, read through a connection already open.
pub fn read(connection: &RustConnection, root: u32, screen_number: usize) -> Scale {
    let dpi = resource_dpi(connection, root);
    let factor = xsettings_integer(connection, screen_number, "Gdk/WindowScalingFactor");
    Scale {
        value: combine(dpi, factor),
        dpi,
        factor,
    }
}

/// The scale of the default screen, through a connection of its own. None
/// when there is no X11 display.
pub fn screen() -> Option<Scale> {
    let (connection, screen_number) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots.get(screen_number)?.root;
    Some(read(&connection, root, screen_number))
}

/// The larger of the two, so a desktop that sets both is not scaled twice.
fn combine(dpi: Option<f32>, factor: Option<i32>) -> f32 {
    let from_dpi = dpi.map_or(1.0, |dpi| dpi / 96.0);
    let from_factor = factor.filter(|factor| *factor >= 1).map_or(1.0, |factor| factor as f32);
    from_dpi.max(from_factor).max(1.0)
}

/// Xft.dpi from the X resources, when set.
fn resource_dpi(connection: &RustConnection, root: u32) -> Option<f32> {
    let reply = connection
        .get_property(false, root, AtomEnum::RESOURCE_MANAGER, AtomEnum::STRING, 0, 1 << 16)
        .ok()?
        .reply()
        .ok()?;
    String::from_utf8_lossy(&reply.value)
        .lines()
        .find_map(|line| line.strip_prefix("Xft.dpi:"))
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|dpi| *dpi > 0.0)
}

/// An integer setting from the XSETTINGS manager of this screen (xfsettingsd
/// on Xfce), when one runs and has the setting.
fn xsettings_integer(connection: &RustConnection, screen_number: usize, name: &str) -> Option<i32> {
    let selection = intern(connection, &format!("_XSETTINGS_S{screen_number}"))?;
    let owner = connection.get_selection_owner(selection).ok()?.reply().ok()?.owner;
    if owner == 0 {
        return None;
    }
    let property = intern(connection, "_XSETTINGS_SETTINGS")?;
    let reply = connection
        .get_property(false, owner, property, AtomEnum::ANY, 0, 1 << 16)
        .ok()?
        .reply()
        .ok()?;
    find_xsettings_integer(&reply.value, name)
}

fn intern(connection: &RustConnection, name: &str) -> Option<u32> {
    Some(connection.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom)
}

/// One integer out of an _XSETTINGS_SETTINGS value, laid out as the XSETTINGS
/// specification gives it: a byte order, a serial and a count, then each
/// setting as type, name, last-change serial and value, padded to 4 bytes.
fn find_xsettings_integer(data: &[u8], wanted: &str) -> Option<i32> {
    let big_endian = match *data.first()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let card16 = |at: usize| -> Option<usize> {
        let bytes: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
        Some(usize::from(if big_endian { u16::from_be_bytes(bytes) } else { u16::from_le_bytes(bytes) }))
    };
    let card32 = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = data.get(at..at + 4)?.try_into().ok()?;
        Some(if big_endian { u32::from_be_bytes(bytes) } else { u32::from_le_bytes(bytes) })
    };
    let count = card32(8)?;
    let mut at = 12;
    for _ in 0..count {
        let kind = *data.get(at)?;
        let name_length = card16(at + 2)?;
        let name = data.get(at + 4..at + 4 + name_length)?;
        // Past the name, its padding and the last-change serial.
        at += 4 + padded(name_length) + 4;
        match kind {
            0 => {
                let value = card32(at)? as i32;
                if name == wanted.as_bytes() {
                    return Some(value);
                }
                at += 4;
            }
            1 => at += 4 + padded(card32(at)? as usize),
            2 => at += 8,
            _ => return None,
        }
    }
    None
}

fn padded(length: usize) -> usize {
    (length + 3) & !3
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An _XSETTINGS_SETTINGS value with a string setting, then an integer.
    fn settings(big_endian: bool) -> Vec<u8> {
        let card16 = |value: u16| if big_endian { value.to_be_bytes() } else { value.to_le_bytes() };
        let card32 = |value: u32| if big_endian { value.to_be_bytes() } else { value.to_le_bytes() };
        let mut data = vec![u8::from(big_endian), 0, 0, 0];
        data.extend(card32(7));
        data.extend(card32(2));

        data.extend([1, 0]);
        data.extend(card16(13));
        data.extend(b"Net/ThemeName\0\0\0");
        data.extend(card32(0));
        data.extend(card32(8));
        data.extend(b"Greybird");

        data.extend([0, 0]);
        data.extend(card16(23));
        data.extend(b"Gdk/WindowScalingFactor\0");
        data.extend(card32(0));
        data.extend(card32(2));
        data
    }

    #[test]
    fn reads_an_integer_in_both_byte_orders() {
        for big_endian in [false, true] {
            let data = settings(big_endian);
            assert_eq!(find_xsettings_integer(&data, "Gdk/WindowScalingFactor"), Some(2));
            assert_eq!(find_xsettings_integer(&data, "Gdk/UnscaledDPI"), None);
        }
    }

    #[test]
    fn gives_nothing_for_a_cut_value() {
        let data = settings(false);
        assert_eq!(find_xsettings_integer(&data[..data.len() - 2], "Gdk/WindowScalingFactor"), None);
        assert_eq!(find_xsettings_integer(&[], "Gdk/WindowScalingFactor"), None);
    }

    #[test]
    fn takes_the_larger_scale() {
        assert_eq!(combine(Some(96.0), Some(2)), 2.0);
        assert_eq!(combine(Some(192.0), Some(2)), 2.0);
        assert_eq!(combine(Some(144.0), None), 1.5);
        assert_eq!(combine(None, None), 1.0);
        assert_eq!(combine(Some(72.0), Some(0)), 1.0);
    }
}
