// Libraries that ship with Sensay sit in ../lib next to the binary: usr/lib
// in the AppImage, target/lib in a development build. The path goes in as
// DT_RPATH, not DT_RUNPATH, so the loader also looks there for what those
// libraries need in turn (libxkbcommon-x11 needs libxcb-xkb).
//
// The GNOME extension is written for GNOME 45 and later, which load it as ES
// modules. GNOME 42–44 load old-style GJS modules; their copy is made here,
// into OUT_DIR, by rewriting the imports and exports, and the app puts it in
// on those versions (src/gnome_extension.rs).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const EXTENSION: &str = "gnome/sensay-pill@sensay.app";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags,-rpath,$ORIGIN/../lib");

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("legacy");
    fs::create_dir_all(&out).expect("OUT_DIR is writable");
    for name in ["metadata.json", "extension.js", "paste.js"] {
        let path = Path::new(EXTENSION).join(name);
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let legacy = if name == "metadata.json" { legacy_metadata(&text) } else { legacy_module(name, &text) };
        fs::write(out.join(name), legacy).expect("OUT_DIR is writable");
    }
}

/// The same metadata for GNOME 42–44.
fn legacy_metadata(text: &str) -> String {
    let start = text.find("\"shell-version\"").expect("metadata.json has shell-version");
    let open = start + text[start..].find('[').expect("shell-version is a list");
    let close = open + text[open..].find(']').expect("shell-version list is closed");
    format!("{}[\"42\", \"43\", \"44\"]{}", &text[..open], &text[close + 1..])
}

/// An ES module as an old-style GJS module: imports become imports.*,
/// exported classes become var, the default export becomes init(). Any
/// other import or export stops the build, to be handled here.
fn legacy_module(name: &str, text: &str) -> String {
    let mut lines = Vec::new();
    let mut default_class = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("import ") {
            if let Some(import) = legacy_import(rest) {
                lines.push(import);
            }
        } else if let Some(rest) = line.strip_prefix("export default class ") {
            let class = rest.split_whitespace().next().expect("a class name");
            default_class = Some(class.to_string());
            lines.push(format!("class {class} {{"));
        } else if let Some(rest) = line.strip_prefix("export class ") {
            let class = rest.split_whitespace().next().expect("a class name");
            lines.push(format!("var {class} = class {rest}"));
        } else if line.starts_with("export ") {
            panic!("{name}: no legacy form for: {line}");
        } else {
            lines.push(line.to_string());
        }
    }
    if let Some(class) = default_class {
        lines.push(String::new());
        lines.push("function init() {".to_string());
        lines.push(format!("    return new {class}();"));
        lines.push("}".to_string());
    }
    lines.join("\n") + "\n"
}

/// One import line, after "import "; None for the Extension base class,
/// which old-style extensions do without.
fn legacy_import(rest: &str) -> Option<String> {
    let (what, from) = rest.split_once(" from ").unwrap_or_else(|| panic!("unexpected import {rest}"));
    let from = from.trim_end_matches(';').trim_matches('\'');
    if let Some(library) = from.strip_prefix("gi://") {
        return Some(format!("const {what} = imports.gi.{library};"));
    }
    if from == "cairo" {
        return Some(format!("const {what} = imports.cairo;"));
    }
    if from == "resource:///org/gnome/shell/extensions/extension.js" {
        return None;
    }
    if let Some(module) = from.strip_prefix("resource:///org/gnome/shell/") {
        let name = what.strip_prefix("* as ").unwrap_or_else(|| panic!("unexpected import {rest}"));
        let module = module.trim_end_matches(".js").replace('/', ".");
        return Some(format!("const {name} = imports.{module};"));
    }
    if let Some(module) = from.strip_prefix("./") {
        let module = module.trim_end_matches(".js");
        return Some(format!(
            "const {what} = imports.misc.extensionUtils.getCurrentExtension().imports.{module};"
        ));
    }
    panic!("unexpected import {rest}");
}
