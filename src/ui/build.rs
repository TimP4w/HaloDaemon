// SPDX-License-Identifier: GPL-3.0-or-later
//! Build script: generate third-party license list (same as the GTK GUI).
//!
//! Runs `cargo-about generate about_licenses.hbs` from the workspace root
//! (`src/`) and writes the result plus protocol-reference headers and bundled
//! asset licenses to `$OUT_DIR/third_party_licenses.txt`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

fn main() {
    emit_build_hash();
    embed_windows_icon();

    println!("cargo:rerun-if-changed=../Cargo.lock");
    println!("cargo:rerun-if-changed=../about.toml");
    println!("cargo:rerun-if-changed=../about_licenses.hbs");
    println!("cargo:rerun-if-changed=../assets/fonts");
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=../../LICENSES");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let workspace = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_owned();

    // The protocol references and bundled-asset licenses are derived from the
    // repo and don't need cargo-about, so they're always emitted — even when the
    // crate-dependency list (cargo-about) can't be generated.
    let refs_section = scan_protocol_references(&workspace);
    match Command::new("cargo-about")
        .args(["generate", "about_licenses.hbs"])
        .current_dir(&workspace)
        .output()
    {
        Ok(o) if o.status.success() => {
            let mut content = refs_section.into_bytes();
            content.extend_from_slice(&o.stdout);
            append_bundled_asset_licenses(&workspace, &mut content);
            std::fs::write(out_dir.join("third_party_licenses.txt"), &content)
                .expect("failed to write third_party_licenses.txt");
        }
        Ok(o) => {
            let msg = format!(
                "cargo-about failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            license_failure(&out_dir, &workspace, refs_section, &msg);
        }
        Err(e) => {
            license_failure(
                &out_dir,
                &workspace,
                refs_section,
                &format!("cargo-about not found or could not run ({e})"),
            );
        }
    }
}

/// Embed the app icon (`assets/icon.ico`) as the Windows exe resourc
#[cfg(windows)]
fn embed_windows_icon() {
    let icon = "../../assets/icon.ico";
    println!("cargo:rerun-if-changed={icon}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(icon);
    if let Err(e) = res.compile() {
        // Don't fail the build on a resource-compiler hiccup; the runtime
        // `with_icon` still sets the window icon.
        println!("cargo:warning=failed to embed Windows icon resource: {e}");
    }
}

#[cfg(not(windows))]
fn embed_windows_icon() {}

/// Emit `HALOD_BUILD_HASH` (short git commit) so `env!` picks it up at compile
/// time. A pre-set `HALOD_BUILD_HASH` in the environment wins (the Nix build has
/// no `.git`, so the flake passes the revision in that way); otherwise it reads
/// git, falling back to `unknown` outside a checkout. Rebuilds when either the
/// env var or the checked-out commit changes.
fn emit_build_hash() {
    println!("cargo:rerun-if-env-changed=HALOD_BUILD_HASH");
    let hash = std::env::var("HALOD_BUILD_HASH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=HALOD_BUILD_HASH={hash}");

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

/// Write a best-effort license file when cargo-about can't produce the Rust
/// crate dependency section: keep the protocol references and bundled-asset
/// licenses (both repo-derived) so the dialog still has real content.
fn license_failure(
    out_dir: &std::path::Path,
    workspace: &std::path::Path,
    refs_section: String,
    msg: &str,
) {
    println!("cargo:rerun-if-env-changed=HALOD_REQUIRE_LICENSES");
    if std::env::var_os("HALOD_REQUIRE_LICENSES").is_some_and(|v| !v.is_empty() && v != "0") {
        panic!(
            "{msg}\n\
             HALOD_REQUIRE_LICENSES is set, refusing to build without the \
             third-party license list. Install cargo-about and ensure it is on PATH."
        );
    }
    println!("cargo:warning={msg}; Rust crate license section will be omitted");
    let sep = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
    let mut content = if refs_section.is_empty() {
        b"Third-Party Licenses\n====================\n\n".to_vec()
    } else {
        refs_section.into_bytes()
    };
    content.extend_from_slice(
        format!(
            "\n{sep}\nRUST CRATE DEPENDENCIES\n{sep}\n\n\
             [The per-crate license list could not be generated at build time\n\
              (cargo-about unavailable or failed). Run `cargo about generate\n\
              about_licenses.hbs` from `src/` to produce it.]\n"
        )
        .as_bytes(),
    );
    append_bundled_asset_licenses(workspace, &mut content);
    std::fs::write(out_dir.join("third_party_licenses.txt"), &content)
        .expect("failed to write license fallback");
}

fn scan_protocol_references(workspace: &std::path::Path) -> String {
    let mut by_license: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let source_roots = [
        "broker/src",
        "daemon/src",
        "hwaccess/src",
        "shared/src",
        "ui/src",
    ];
    for path in source_roots
        .iter()
        .flat_map(|root| walk_rs_files(&workspace.join(root)))
    {
        // Per-file so an edited SPDX header (not just an added/removed file)
        // triggers regeneration.
        println!("cargo:rerun-if-changed={}", path.display());
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut license: Option<String> = None;
        let mut copyrights: Vec<String> = Vec::new();
        // REUSE-IgnoreStart -- the SPDX tag literals below are parser inputs,
        // not this file's own license declaration.
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(id) = trimmed.strip_prefix("// SPDX-License-Identifier:") {
                license = Some(id.trim().to_string());
            } else if let Some(ct) = trimmed.strip_prefix("// SPDX-FileCopyrightText:") {
                let ct = ct.trim();
                if !ct.contains("HaloDaemon") && !ct.contains("HaloD") {
                    copyrights.push(ct.to_string());
                }
            } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
                break;
            }
        }
        // REUSE-IgnoreEnd
        if let (Some(lic), false) = (license, copyrights.is_empty()) {
            by_license.entry(lic).or_default().extend(copyrights);
        }
    }
    if by_license.is_empty() {
        return String::new();
    }
    let sep = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
    let mut out = format!(
        "Third-Party Licenses\n====================\n\n\
         {sep}\nPROTOCOL REFERENCES\n{sep}\n\n\
         The following third-party copyright holders appear in\n\
         HaloDaemon's protocol implementations (grouped by license).\n\
         Full license texts are in the Rust crates section below.\n\n"
    );
    for (license, copyrights) in &by_license {
        out.push_str(&format!("  {}:\n", license));
        for copyright in copyrights {
            out.push_str(&format!("    {}\n", copyright));
        }
        out.push('\n');
    }
    out
}

fn walk_rs_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk_rs_files(&path));
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files
}

fn append_bundled_asset_licenses(workspace: &std::path::Path, buf: &mut Vec<u8>) {
    let sep = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
    // All bundled fonts are SIL OFL-1.1, so a single license text (shipped with
    // NotoSans) covers every one of them.
    let fonts = [
        (
            "NotoSans (sans-serif, LCD engine)",
            "2022 The Noto Project Authors",
            "https://github.com/notofonts/latin-greek-cyrillic",
        ),
        (
            "JetBrains Mono (monospace, GUI and LCD engine)",
            "2020 The JetBrains Mono Project Authors",
            "https://github.com/JetBrains/JetBrainsMono",
        ),
        (
            "Inter Tight (proportional, GUI)",
            "2016 The Inter Project Authors",
            "https://github.com/rsms/inter",
        ),
    ];
    let mut header = format!(
        "\n{sep}\nBUNDLED ASSETS\n{sep}\n\n\
         The following fonts are embedded in HaloDaemon (all SIL OFL-1.1):\n\n"
    );
    for (name, copyright, url) in fonts {
        header.push_str(&format!(
            "  {name}\n    Copyright: {copyright}\n    License: OFL-1.1\n    URL: {url}\n\n"
        ));
    }
    header.push_str(
        "  Material Design Icons (selected SVG glyphs)\n\
           Copyright: Pictogrammers\n\
           License: Apache-2.0\n\
           URL: https://pictogrammers.com/library/mdi/\n\n\
         Google Material Icons (selected SVG glyphs)\n\
           Copyright: Google LLC\n\
           License: Apache-2.0\n\
           URL: https://fonts.google.com/icons\n\n",
    );
    buf.extend_from_slice(header.as_bytes());

    let licenses = workspace
        .parent()
        .expect("workspace must be below repository root")
        .join("LICENSES");
    append_license_text(buf, &licenses, "OFL-1.1", "OFL-1.1.txt");
    append_license_text(buf, &licenses, "Apache-2.0", "Apache-2.0.txt");

    buf.extend_from_slice(format!("\n{sep}\nINCORPORATED SOURCE LICENSES\n{sep}\n\n").as_bytes());
    for (id, file) in [
        ("GPL-2.0-or-later", "GPL-2.0-or-later.txt"),
        ("MPL-2.0", "MPL-2.0.txt"),
        ("MIT", "MIT.txt"),
    ] {
        append_license_text(buf, &licenses, id, file);
    }
}

fn append_license_text(buf: &mut Vec<u8>, licenses_dir: &std::path::Path, id: &str, file: &str) {
    buf.extend_from_slice(format!("\n--- {id} ---\n\n").as_bytes());
    let path = licenses_dir.join(file);
    match std::fs::read(&path) {
        Ok(text) => {
            buf.extend_from_slice(&text);
            if !text.ends_with(b"\n") {
                buf.push(b'\n');
            }
        }
        Err(e) => {
            println!("cargo:warning=could not read {id} license text ({e})");
            buf.extend_from_slice(format!("[{id} license file not found]\n").as_bytes());
        }
    }
}
