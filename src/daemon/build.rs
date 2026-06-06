//! Build script: stage the vendored PawnIO modules next to the built
//! executable.
//!
//! Windows chipset SMBus access loads `SmbusI801.bin` (Intel) / `SmbusPIIX4.bin`
//! (AMD); SuperIO fan control loads `LpcIO.bin`. The PawnIO transport searches
//! next to the executable, so copying the modules there makes discovery work
//! regardless of the working directory — which matters because the daemon
//! self-elevates and an elevated relaunch resets the CWD.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // PawnIO modules are only used on Windows; skip on other targets.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    // Repo-root `pwnio/` directory, relative to this crate (`src/daemon`).
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"),
    );
    let modules_dir = manifest_dir.join("../../pwnio");

    // OUT_DIR = <target>/<profile>/build/<pkg>-<hash>/out
    //   → executable directory = <target>/<profile> (3 ancestors up).
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let Some(exe_dir) = out_dir.ancestors().nth(3) else {
        println!(
            "cargo:warning=could not derive target dir from OUT_DIR; \
             PawnIO SMBus modules not staged"
        );
        return;
    };

    for name in ["SmbusI801.bin", "SmbusPIIX4.bin", "LpcIO.bin"] {
        let src = modules_dir.join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        if src.exists() {
            if let Err(e) = std::fs::copy(&src, exe_dir.join(name)) {
                println!("cargo:warning=failed to stage {name}: {e}");
            }
        } else {
            let feature = match name {
                "LpcIO.bin" => "SuperIO motherboard fan control",
                _ => "chipset SMBus RGB (DRAM)",
            };
            println!(
                "cargo:warning={} not found — {feature} will be unavailable",
                src.display()
            );
        }
    }
}
