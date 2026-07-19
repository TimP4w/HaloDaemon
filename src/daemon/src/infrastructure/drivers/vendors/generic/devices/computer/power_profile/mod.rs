// SPDX-License-Identifier: GPL-3.0-or-later
//! System power-profile support for the [`super::ComputerDevice`]: the canonical
//! profile list, the platform [`PowerProfileBackend`] trait, and backend
//! selection. The actual work lives in the platform submodules —
//! power-profiles-daemon (D-Bus, with a `powerprofilesctl` fallback) on Linux,
//! `powercfg` on Windows.

use anyhow::Result;
use async_trait::async_trait;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// The choice `key` under which the power profile is exposed to the UI.
pub const POWER_PROFILE_KEY: &str = "power_profile";

/// Canonical `(id, label)` power profiles, in display order. The ids match the
/// strings power-profiles-daemon uses, so the Linux backend maps them 1:1.
pub const PROFILES: &[(&str, &str)] = &[
    ("performance", "Performance"),
    ("balanced", "Balanced"),
    ("power-saver", "Power Saver"),
];

/// Index of a canonical profile id, or `None` if unknown.
pub fn index_of(id: &str) -> Option<usize> {
    PROFILES.iter().position(|(k, _)| *k == id)
}

// --- Windows power-plan GUIDs (kept platform-neutral so they can be unit
// tested off Windows). Aliases: SCHEME_MIN / SCHEME_BALANCED / SCHEME_MAX. ---
#[cfg(any(target_os = "windows", test))]
pub const WIN_GUID_PERFORMANCE: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";
#[cfg(any(target_os = "windows", test))]
pub const WIN_GUID_BALANCED: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
#[cfg(any(target_os = "windows", test))]
pub const WIN_GUID_POWER_SAVER: &str = "a1841308-3541-4fab-bc81-f71556f20b4a";

/// The Windows power-plan GUID for a canonical profile id.
#[cfg(any(target_os = "windows", test))]
pub fn windows_guid_for(id: &str) -> Option<&'static str> {
    match id {
        "performance" => Some(WIN_GUID_PERFORMANCE),
        "balanced" => Some(WIN_GUID_BALANCED),
        "power-saver" => Some(WIN_GUID_POWER_SAVER),
        _ => None,
    }
}

/// The canonical profile id for a Windows power-plan GUID (case-insensitive).
/// `None` for a custom/unknown plan.
#[cfg(any(target_os = "windows", test))]
pub fn windows_profile_for_guid(guid: &str) -> Option<&'static str> {
    let g = guid.trim().to_ascii_lowercase();
    match g.as_str() {
        WIN_GUID_PERFORMANCE => Some("performance"),
        WIN_GUID_BALANCED => Some("balanced"),
        WIN_GUID_POWER_SAVER => Some("power-saver"),
        _ => None,
    }
}

#[cfg(any(target_os = "windows", test))]
fn is_guid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes().iter().enumerate().all(|(i, &b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Extract the active scheme GUID from `powercfg /getactivescheme` output, e.g.
/// `Power Scheme GUID: 381b4222-... (Balanced)` -> the lowercased GUID.
#[cfg(any(target_os = "windows", test))]
pub fn parse_powercfg_active_guid(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|t| is_guid(t))
        .map(|s| s.to_ascii_lowercase())
}

/// Reads and applies the host's power profile. One implementation per platform.
#[async_trait]
pub trait PowerProfileBackend: Send + Sync {
    /// The currently active profile as a canonical id, if detectable.
    async fn current(&self) -> Option<&'static str>;
    /// Switch the host to the profile with the given canonical id.
    async fn apply(&self, id: &str) -> Result<()>;
}

/// The platform's power-profile backend, or `None` when the host has no usable
/// power-management interface (so the capability is hidden entirely).
#[cfg(target_os = "linux")]
pub async fn make_backend() -> Option<Box<dyn PowerProfileBackend>> {
    linux::LinuxPowerProfile::detect()
        .await
        .map(|b| Box::new(b) as Box<dyn PowerProfileBackend>)
}

#[cfg(target_os = "windows")]
pub async fn make_backend() -> Option<Box<dyn PowerProfileBackend>> {
    Some(Box::new(windows::WindowsPowerProfile) as Box<dyn PowerProfileBackend>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub async fn make_backend() -> Option<Box<dyn PowerProfileBackend>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_of_covers_every_profile_and_rejects_unknown() {
        for (i, (id, _)) in PROFILES.iter().enumerate() {
            assert_eq!(index_of(id), Some(i));
        }
        assert_eq!(index_of("turbo"), None);
    }

    #[test]
    fn windows_guid_round_trips_for_every_profile() {
        for (id, _) in PROFILES {
            let guid = windows_guid_for(id).expect("every profile has a GUID");
            assert_eq!(windows_profile_for_guid(guid), Some(*id));
            // Mapping is case-insensitive.
            assert_eq!(
                windows_profile_for_guid(&guid.to_ascii_uppercase()),
                Some(*id)
            );
        }
        assert_eq!(windows_guid_for("turbo"), None);
        assert_eq!(windows_profile_for_guid("not-a-guid"), None);
    }

    #[test]
    fn windows_guids_match_the_well_known_scheme_ids() {
        // These are the canonical Windows SCHEME_MIN / SCHEME_BALANCED /
        // SCHEME_MAX plan GUIDs. Pinned exactly so a mistyped GUID (which
        // round-trips fine but makes `powercfg /setactive` fail) is caught.
        assert_eq!(WIN_GUID_PERFORMANCE, "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c");
        assert_eq!(WIN_GUID_BALANCED, "381b4222-f694-41f0-9685-ff5bb260df2e");
        assert_eq!(WIN_GUID_POWER_SAVER, "a1841308-3541-4fab-bc81-f71556f20b4a");
    }

    #[test]
    fn parses_active_guid_from_powercfg_output() {
        let out = "Power Scheme GUID: 381b4222-f694-41f0-9685-ff5bb260df2e  (Balanced)\r\n";
        assert_eq!(
            parse_powercfg_active_guid(out).as_deref(),
            Some(WIN_GUID_BALANCED)
        );
        assert_eq!(parse_powercfg_active_guid("no guid here"), None);
    }
}
