// Libraries that ship with Sensay sit in ../lib next to the binary: usr/lib
// in the AppImage, target/lib in a development build. The path goes in as
// DT_RPATH, not DT_RUNPATH, so the loader also looks there for what those
// libraries need in turn (libxkbcommon-x11 needs libxcb-xkb).

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags,-rpath,$ORIGIN/../lib");
}
