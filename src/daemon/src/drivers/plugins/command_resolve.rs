// SPDX-License-Identifier: GPL-3.0-or-later
//! Resolve a bare executable name against the daemon's `PATH` — the single
//! resolver shared by the command requirement probe and command execution, so a
//! plugin's declared command is checked and run the same way. Honors `PATHEXT`
//! on Windows and requires an executable regular file on Unix.

use std::path::{Path, PathBuf};

/// Resolve `name` to an absolute path by searching `dirs`, trying each extension
/// in `exts` (`[""]` on Unix; `PATHEXT` entries on Windows). `is_exec` decides
/// whether a candidate is a runnable file — injected so the search is unit-
/// testable without touching the real filesystem or `PATH`.
pub fn resolve_in(
    name: &str,
    dirs: &[PathBuf],
    exts: &[String],
    is_exec: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    for dir in dirs {
        for ext in exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if is_exec(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

#[cfg(windows)]
fn extensions() -> Vec<String> {
    let mut exts = vec![String::new()];
    if let Some(pathext) = std::env::var_os("PATHEXT") {
        exts.extend(
            std::env::split_paths(&pathext).filter_map(|e| e.to_str().map(|s| s.to_owned())),
        );
    }
    exts
}

#[cfg(not(windows))]
fn extensions() -> Vec<String> {
    vec![String::new()]
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Resolve `name` on the daemon's real `PATH`, or `None` if it isn't a runnable
/// executable there.
pub fn resolve(name: &str) -> Option<PathBuf> {
    resolve_in(name, &path_dirs(), &extensions(), &is_executable_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_in_the_first_matching_directory() {
        let dirs = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let exts = vec![String::new()];
        let only_in_b = |p: &Path| p == Path::new("/b/tool");
        assert_eq!(
            resolve_in("tool", &dirs, &exts, &only_in_b),
            Some(PathBuf::from("/b/tool"))
        );
    }

    #[test]
    fn honors_extensions_like_pathext() {
        let dirs = vec![PathBuf::from("/w")];
        let exts = vec![String::new(), ".EXE".into()];
        let only_exe = |p: &Path| p == Path::new("/w/tool.EXE");
        assert_eq!(
            resolve_in("tool", &dirs, &exts, &only_exe),
            Some(PathBuf::from("/w/tool.EXE"))
        );
    }

    #[test]
    fn unresolvable_name_is_none() {
        let dirs = vec![PathBuf::from("/a")];
        let exts = vec![String::new()];
        assert!(resolve_in("missing", &dirs, &exts, &|_| false).is_none());
    }
}
