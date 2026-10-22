// The libraries winit opens while the program runs instead of linking them:
// libxkbcommon, and on X11 libxkbcommon-x11 as well. winit stops the whole
// program when one is missing, so they are looked for before a window opens;
// without them the windows stay closed and dictation goes on.
//
// The loader looks in ../lib next to the binary first (build.rs), where the
// AppImage and a development build keep their own copies.

use std::ffi::CStr;

/// What keeps the windows from opening, as a line for the log; None when
/// everything is there.
pub fn missing() -> Option<String> {
    let mut needed: Vec<(&CStr, &str)> = vec![(c"libxkbcommon.so.0", "libxkbcommon0")];
    if crate::desktop::windows_use_x11() {
        needed.push((c"libxkbcommon-x11.so.0", "libxkbcommon-x11-0"));
    }
    needed.into_iter().find_map(|(name, package)| {
        open(name).err().map(|reason| {
            format!(
                "{} cannot be loaded ({reason}); the package is {package} on Debian and Ubuntu",
                name.to_string_lossy()
            )
        })
    })
}

/// Opens the library as winit would, and closes it again.
fn open(name: &CStr) -> Result<(), String> {
    // SAFETY: a valid C string; the handle is closed straight away, and
    // dlerror is read on this thread right after the failed call.
    unsafe {
        let handle = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if handle.is_null() {
            let error = libc::dlerror();
            return Err(if error.is_null() {
                "no reason given".to_string()
            } else {
                CStr::from_ptr(error).to_string_lossy().into_owned()
            });
        }
        libc::dlclose(handle);
    }
    Ok(())
}
