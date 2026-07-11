// SPDX-License-Identifier: GPL-3.0-or-later
//! Every daemon-affecting action (rename, hide, upload, paint, …) as a named
//! free function wrapping [`crate::runtime::ipc::send`]. This is the only
//! place `DaemonCommand`s are constructed outside `runtime/` itself — screens
//! call these instead of building commands inline, so every daemon effect is
//! discoverable in one place and duplicate constructions (rename, profile
//! switch, …) collapse to one definition.

pub mod canvas;
pub mod cooling;
pub mod devices;
pub mod integrations;
pub mod keys;
pub mod lcd;
pub mod lighting;
pub mod plugins;
pub mod profiles;
pub mod system;

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// No `ipc::send(` outside `domain/actions/` and `runtime/` — the actions
    /// layer is the only place `DaemonCommand`s are constructed and sent.
    /// Keeps the discipline honest as new screens/features are added.
    #[test]
    fn ipc_send_is_confined_to_actions_and_runtime() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        walk(&src, &mut offenders);
        assert!(
            offenders.is_empty(),
            "ipc::send(...) called outside domain/actions/ or runtime/ — route it through a named action instead:\n{}",
            offenders.join("\n")
        );
    }

    fn walk(dir: &Path, offenders: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, offenders);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path.to_string_lossy().replace('\\', "/");
            // The Debouncer's own flush() is the debounce primitive's send seam,
            // analogous to `runtime` — it isn't a screen constructing a command.
            if rel.contains("/domain/actions/")
                || rel.contains("/runtime/")
                || rel.ends_with("/domain/state/debounce.rs")
            {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (i, line) in contents.lines().enumerate() {
                if line.contains("ipc::send(") {
                    offenders.push(format!("{rel}:{}", i + 1));
                }
            }
        }
    }
}
