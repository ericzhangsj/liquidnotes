// Embeds the application icon into the executable on Windows.
//
// We shell out to `windres` (from the w64devkit / MinGW toolchain this project
// builds with) to compile a tiny .rc referencing `liquidnotes.ico` into a COFF
// object, then link that object into every binary. No extra crate dependency and
// no network access. If `windres` or the .ico is missing the build still
// succeeds — the exe just ships without an embedded icon.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    // Only meaningful when targeting Windows.
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let ico_path = Path::new(&manifest_dir).join("liquidnotes.ico");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=liquidnotes.ico");

    if !ico_path.exists() {
        println!("cargo:warning=liquidnotes.ico not found; building without embedded icon");
        return;
    }

    let out_dir = env::var("OUT_DIR").unwrap();
    let rc_path = Path::new(&out_dir).join("app.rc");
    let obj_path = Path::new(&out_dir).join("app_icon.o");

    // windres wants forward slashes in the .rc path string.
    let ico_str = ico_path.to_string_lossy().replace('\\', "/");
    // Resource id 1: the lowest-id RT_GROUP_ICON becomes the exe's Explorer icon.
    let rc = format!("1 ICON \"{ico_str}\"\n");
    if let Err(e) = fs::write(&rc_path, rc) {
        println!("cargo:warning=failed to write app.rc: {e}; skipping icon");
        return;
    }

    let status = Command::new("windres")
        .arg("-O")
        .arg("coff")
        .arg("-i")
        .arg(&rc_path)
        .arg("-o")
        .arg(&obj_path)
        .status();

    match status {
        Ok(s) if s.success() => {
            // Link the compiled resource object into all binaries.
            println!("cargo:rustc-link-arg-bins={}", obj_path.to_string_lossy());
        }
        Ok(s) => {
            println!("cargo:warning=windres exited with {s}; building without embedded icon");
        }
        Err(e) => {
            println!("cargo:warning=could not run windres ({e}); building without embedded icon");
        }
    }
}
