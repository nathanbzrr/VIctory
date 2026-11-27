//! Link settings for the Windows executable.

use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(agility_runtime)");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    // The D3D12 Agility SDK exports (src/agility.rs), only when `cargo xtask
    // agility` has put the runtime in this profile's `D3D12\` folder. D3D12
    // uses the runtime an executable names only if it is there: with the
    // exports but no `D3D12Core.dll`, device creation fails (on Windows 11
    // too, D3D12_ERROR_INVALID_REDIST) instead of falling back to the in-box
    // runtime. Without the folder the executable names none, as before.
    // OUT_DIR is <target>/<profile>/build/<package>-<hash>/out.
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let core = out
        .ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "build"))
        .and_then(|p| p.parent())
        .map(|profile| profile.join("D3D12").join("D3D12Core.dll"));
    match core {
        Some(core) if core.exists() => {
            println!("cargo::rerun-if-changed={}", core.display());
            println!("cargo::rustc-cfg=agility_runtime");
            // Only the MSVC linker takes /EXPORT on a binary.
            println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
            println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
        }
        _ => {
            // Watching a missing file would rebuild the executable every
            // time; `cargo xtask agility` touches this file instead.
            println!("cargo::rerun-if-changed=build.rs");
        }
    }
}
