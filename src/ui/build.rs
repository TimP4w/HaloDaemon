use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../Cargo.lock");
    println!("cargo:rerun-if-changed=../about.toml");
    println!("cargo:rerun-if-changed=../about_licenses.hbs");
    println!("cargo:rerun-if-changed=../daemon/src");
    println!("cargo:rerun-if-changed=../daemon/assets/NotoSans-Regular-LICENSE.txt");
    println!("cargo:rerun-if-changed=assets/icons");

    glib_build_tools::compile_resources(
        &["assets/icons"],
        "assets/icons/periphctl-icons.gresource.xml",
        "periphctl-icons.gresource",
    );

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    // CARGO_MANIFEST_DIR = <repo>/src/ui  →  workspace root = <repo>/src
    let workspace = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .parent()
        .unwrap()
        .to_owned();

    #[cfg(windows)]
    embed_windows_icon(&workspace, &out_dir);

    match Command::new("cargo-about")
        .args(["generate", "about_licenses.hbs"])
        .current_dir(&workspace)
        .output()
    {
        Ok(o) if o.status.success() => {
            let refs_section = scan_protocol_references(&workspace);
            let mut content = refs_section.into_bytes();
            content.extend_from_slice(&o.stdout);
            append_bundled_asset_licenses(&workspace, &mut content);
            std::fs::write(out_dir.join("third_party_licenses.txt"), &content)
                .expect("failed to write third_party_licenses.txt");
        }
        Ok(o) => {
            println!(
                "cargo:warning=cargo-about failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            );
            write_stub(&out_dir);
        }
        Err(e) => {
            println!(
                "cargo:warning=cargo-about not found or could not run ({e}); \
                 third-party license list will be a stub"
            );
            write_stub(&out_dir);
        }
    }
}

/// Scan SPDX-FileCopyrightText headers from daemon source files to produce a
/// "Protocol References" section. Entries are read directly from the headers
/// we already maintain, so adding a new protocol file with correct SPDX
/// headers is all that's needed to appear here automatically.
fn scan_protocol_references(workspace: &std::path::Path) -> String {
    // license_id -> sorted set of copyright strings (excluding HaloDaemon's own)
    let mut by_license: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    let daemon_src = workspace.join("daemon/src");
    for path in walk_rs_files(&daemon_src) {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };

        let mut license: Option<String> = None;
        let mut copyrights: Vec<String> = Vec::new();

        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(id) = trimmed.strip_prefix("// SPDX-License-Identifier:") {
                license = Some(id.trim().to_string());
            } else if let Some(ct) = trimmed.strip_prefix("// SPDX-FileCopyrightText:") {
                let ct = ct.trim();
                // Skip HaloDaemon's own copyright entries
                if !ct.contains("HaloDaemon") && !ct.contains("HaloD") {
                    copyrights.push(ct.to_string());
                }
            } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
                break; // past the header block
            }
        }

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
         {sep}\n\
         PROTOCOL REFERENCES\n\
         {sep}\n\n\
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
            } else if path.extension().map_or(false, |e| e == "rs") {
                files.push(path);
            }
        }
    }
    files
}

fn append_bundled_asset_licenses(workspace: &std::path::Path, buf: &mut Vec<u8>) {
    let sep = "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━";
    let header = format!(
        "\n{sep}\n\
         BUNDLED ASSETS\n\
         {sep}\n\n\
         NotoSans Regular (font embedded in LCD engine)\n\
         Copyright: 2022 The Noto Project Authors\n\
         License: OFL-1.1\n\
         URL: https://github.com/notofonts/latin-greek-cyrillic\n\n"
    );
    buf.extend_from_slice(header.as_bytes());

    let license_path = workspace.join("daemon/assets/NotoSans-Regular-LICENSE.txt");
    match std::fs::read(&license_path) {
        Ok(text) => buf.extend_from_slice(&text),
        Err(e) => {
            println!("cargo:warning=could not read NotoSans license ({e})");
            buf.extend_from_slice(b"[NotoSans license file not found]\n");
        }
    }
}

/// Render assets/icon.svg to a multi-size ICO in OUT_DIR and embed it as the
/// executable's icon resource. GTK4 on Windows uses the exe's icon for the
/// title bar, taskbar, Alt-Tab and Explorer entries.
#[cfg(windows)]
fn embed_windows_icon(workspace: &std::path::Path, out_dir: &std::path::Path) {
    use resvg::{tiny_skia, usvg};
    use std::fs::File;

    let repo_root = workspace.parent().expect("workspace has parent");
    let svg_path = repo_root.join("assets/icon.svg");
    println!("cargo:rerun-if-changed={}", svg_path.display());

    let bytes = std::fs::read(&svg_path).expect("read assets/icon.svg");
    let tree =
        usvg::Tree::from_data(&bytes, &usvg::Options::default()).expect("parse assets/icon.svg");
    let intrinsic = tree.size().width();

    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
        let scale = size as f32 / intrinsic;
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        let image = ico::IconImage::from_rgba_data(size, size, pixmap.take());
        icon_dir.add_entry(ico::IconDirEntry::encode(&image).expect("encode ico entry"));
    }

    let ico_path = out_dir.join("halod.ico");
    let f = File::create(&ico_path).expect("create halod.ico");
    icon_dir.write(f).expect("write halod.ico");

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("ico path utf-8"));
    res.compile().expect("compile windows resource");
}

fn write_stub(out_dir: &std::path::Path) {
    std::fs::write(
        out_dir.join("third_party_licenses.txt"),
        "[License information unavailable — install cargo-about to generate this list]\n",
    )
    .expect("failed to write license stub");
}
