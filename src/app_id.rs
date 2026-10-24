// App ID: the number support asks the user for, the same on every launch on
// this computer and shown in the About window. machine-id(5) calls the machine
// id confidential and asks programs not to show it as it is, so the App ID is
// derived from it, the same way on macOS, Windows and Linux: HMAC-SHA256 with
// the machine id as read (UTF-8, no line break) as the key and APP_KEY as the
// message; the first 16 bytes, written as an uppercase UUID 8-4-4-4-12.

use hmac::{Hmac, Mac};
use sha2::Sha256;

const APP_KEY: &str = "393B50F8-818E-43BA-BE3A-293B7EDCD0F4";

/// Where the machine id is kept, in the order they are read.
const SOURCES: [&str; 2] = ["/etc/machine-id", "/var/lib/dbus/machine-id"];

/// The App ID of this computer; None when no machine id can be read.
pub fn app_id() -> Option<String> {
    let machine_id = SOURCES.iter().find_map(|path| {
        let text = std::fs::read_to_string(path).ok()?;
        let id = text.trim_end_matches(['\n', '\r']);
        (!id.is_empty()).then(|| id.to_string())
    })?;
    derive(&machine_id)
}

fn derive(machine_id: &str) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(machine_id.as_bytes()).ok()?;
    mac.update(APP_KEY.as_bytes());
    let digest = mac.finalize().into_bytes();
    let hex: String = digest[..16].iter().map(|byte| format!("{byte:02X}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::derive;

    #[test]
    fn derives_the_same_number_as_the_other_platforms() {
        // Worked out with Python's hmac and hashlib from the same description.
        assert_eq!(
            derive("0123456789abcdef0123456789abcdef").as_deref(),
            Some("D6E77EE7-A1E7-6D97-74A3-8F0DE91D4B4A")
        );
    }
}
