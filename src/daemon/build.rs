// SPDX-License-Identifier: GPL-3.0-or-later
//! Build script: stage the vendored PawnIO modules next to the built
//! executable.
//!
//! Windows chipset SMBus access loads `SmbusI801.bin` (Intel) / `SmbusPIIX4.bin`
//! (AMD); SuperIO fan control loads `LpcIO.bin`; AMD Ryzen CPU temperatures load
//! `AMDFamily17.bin`. The PawnIO transport searches
//! next to the executable, so copying the modules there makes discovery work
//! regardless of the working directory — which matters because the daemon
//! self-elevates and an elevated relaunch resets the CWD.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    emit_build_hash();
    stage_embedded_plugin_bundle();

    // PawnIO modules are only used on Windows; skip on other targets.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let manifest_dir =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    stage_pawnio_modules(&manifest_dir, &out_dir);
}

/// Copy a release-supplied plugin bundle into OUT_DIR and generate a small Rust
/// module. Keeping the optional include here lets ordinary developer builds
/// work without a release artifact while official builds can require one.
fn stage_embedded_plugin_bundle() {
    println!("cargo:rerun-if-env-changed=HALOD_OFFICIAL_PLUGIN_BUNDLE");
    println!("cargo:rerun-if-env-changed=HALOD_REQUIRE_PLUGIN_BUNDLE");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let generated = out_dir.join("embedded_plugin_bundle.rs");
    match std::env::var_os("HALOD_OFFICIAL_PLUGIN_BUNDLE") {
        Some(source) if !source.is_empty() => {
            let source = PathBuf::from(source);
            println!("cargo:rerun-if-changed={}", source.display());
            let target = out_dir.join("official-plugins.tar");
            std::fs::copy(&source, &target).unwrap_or_else(|error| {
                panic!(
                    "copying embedded plugin bundle {}: {error}",
                    source.display()
                )
            });
            std::fs::write(
                generated,
                "pub const BUNDLE: Option<&[u8]> = Some(include_bytes!(concat!(env!(\"OUT_DIR\"), \"/official-plugins.tar\")));\n",
            )
            .expect("writing embedded plugin bundle module");
        }
        _ => {
            if std::env::var_os("HALOD_REQUIRE_PLUGIN_BUNDLE")
                .is_some_and(|v| !v.is_empty() && v != "0")
            {
                panic!(
                    "HALOD_REQUIRE_PLUGIN_BUNDLE is set but HALOD_OFFICIAL_PLUGIN_BUNDLE is absent"
                );
            }
            std::fs::write(generated, "pub const BUNDLE: Option<&[u8]> = None;\n")
                .expect("writing empty embedded plugin bundle module");
        }
    }
}

/// Emit `HALOD_BUILD_HASH` (short git commit) so `env!` picks it up at compile
/// time. Falls back to `unknown` outside a git checkout (e.g. a `cargo package`
/// build). Rebuilds when the checked-out commit changes.
fn emit_build_hash() {
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=HALOD_BUILD_HASH={hash}");

    // Rebuild when HEAD moves (branch checkout / new commit).
    if let Some(git_dir) = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
    {
        println!("cargo:rerun-if-changed={git_dir}/HEAD");
    }
}

fn stage_pawnio_modules(manifest_dir: &std::path::Path, out_dir: &std::path::Path) {
    // OUT_DIR = <target>/<profile>/build/<pkg>-<hash>/out
    //   → executable directory = <target>/<profile> (3 ancestors up).
    let Some(exe_dir) = out_dir.ancestors().nth(3) else {
        println!(
            "cargo:warning=could not derive target dir from OUT_DIR; \
             PawnIO SMBus modules not staged"
        );
        return;
    };

    // Repo-root `pwnio/` directory, relative to this crate (`src/daemon`).
    let modules_dir = manifest_dir.join("../../pwnio");

    for name in [
        "SmbusI801.bin",
        "SmbusPIIX4.bin",
        "LpcIO.bin",
        "AMDFamily17.bin",
    ] {
        let src = modules_dir.join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        if src.exists() {
            if let Err(e) = std::fs::copy(&src, exe_dir.join(name)) {
                println!("cargo:warning=failed to stage {name}: {e}");
            }
        } else {
            let feature = match name {
                "LpcIO.bin" => "SuperIO motherboard fan control",
                "AMDFamily17.bin" => "AMD Ryzen CPU temperatures",
                _ => "chipset SMBus RGB (DRAM)",
            };
            println!(
                "cargo:warning={} not found — {feature} will be unavailable",
                src.display()
            );
        }
    }
}
