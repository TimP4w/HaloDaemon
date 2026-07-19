// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux `PATH` self-repair: a dev shell, systemd, or a GUI can launch the
//! daemon with a `PATH` missing the Nix/FHS dirs `pactl`/`nvidia-smi`/
//! `powerprofilesctl` live in. Augmenting our own `PATH` at startup fixes both
//! the healthcheck probe and the real `Command::new(...)` spawns at once.

use std::env::{join_paths, split_paths};
use std::ffi::OsString;
use std::path::PathBuf;

/// `current` with the well-known system/Nix bin dirs appended if missing,
/// preserving existing entries and order. `home`/`user` resolve the per-user
/// Nix profile dirs and may be absent.
pub fn augmented_path(
    current: Option<OsString>,
    home: Option<OsString>,
    user: Option<OsString>,
) -> OsString {
    let mut entries: Vec<PathBuf> = current
        .as_deref()
        .map(|p| {
            split_paths(p)
                .filter(|e| !e.as_os_str().is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/run/wrappers/bin"),
        PathBuf::from("/run/current-system/sw/bin"),
    ];
    if let Some(home) = home.filter(|h| !h.is_empty()) {
        candidates.push(PathBuf::from(home).join(".nix-profile/bin"));
    }
    if let Some(user) = user.filter(|u| !u.is_empty()) {
        candidates.push(PathBuf::from(format!(
            "/etc/profiles/per-user/{}/bin",
            user.to_string_lossy()
        )));
    }
    candidates
        .extend(["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));

    for candidate in candidates {
        if !entries.contains(&candidate) {
            entries.push(candidate);
        }
    }

    join_paths(entries).unwrap_or_else(|_| current.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(path: &OsString) -> Vec<PathBuf> {
        split_paths(path).collect()
    }

    #[test]
    fn appends_missing_nix_dirs() {
        let result = augmented_path(
            Some(OsString::from("/usr/bin:/bin")),
            Some(OsString::from("/home/x")),
            Some(OsString::from("x")),
        );
        let entries = split(&result);
        assert!(entries.contains(&PathBuf::from("/run/current-system/sw/bin")));
        assert!(entries.contains(&PathBuf::from("/home/x/.nix-profile/bin")));
        assert!(entries.contains(&PathBuf::from("/etc/profiles/per-user/x/bin")));
    }

    #[test]
    fn preserves_existing_order_and_does_not_duplicate() {
        let result = augmented_path(
            Some(OsString::from("/run/current-system/sw/bin:/usr/bin")),
            None,
            None,
        );
        let entries = split(&result);
        assert_eq!(entries[0], PathBuf::from("/run/current-system/sw/bin"));
        assert_eq!(
            entries
                .iter()
                .filter(|e| *e == &PathBuf::from("/run/current-system/sw/bin"))
                .count(),
            1
        );
    }

    #[test]
    fn none_path_yields_system_candidates() {
        let result = augmented_path(None, None, None);
        let entries = split(&result);
        assert!(entries.contains(&PathBuf::from("/usr/bin")));
        assert!(entries.contains(&PathBuf::from("/run/current-system/sw/bin")));
    }

    #[test]
    fn empty_path_has_no_leading_empty_entry() {
        let result = augmented_path(Some(OsString::from("")), None, None);
        let entries = split(&result);
        assert!(!entries.iter().any(|e| e.as_os_str().is_empty()));
    }

    #[test]
    fn missing_home_and_user_skip_per_user_dirs() {
        let result = augmented_path(Some(OsString::from("/usr/bin")), None, None);
        let entries = split(&result);
        assert!(!entries
            .iter()
            .any(|e| e.to_string_lossy().contains(".nix-profile")));
        assert!(!entries
            .iter()
            .any(|e| e.to_string_lossy().contains("per-user")));
        assert!(entries.contains(&PathBuf::from("/usr/bin")));
    }

    #[test]
    fn existing_entry_is_not_duplicated_by_a_candidate() {
        let result = augmented_path(Some(OsString::from("/usr/local/bin")), None, None);
        let entries = split(&result);
        assert_eq!(
            entries
                .iter()
                .filter(|e| *e == &PathBuf::from("/usr/local/bin"))
                .count(),
            1
        );
    }
}
