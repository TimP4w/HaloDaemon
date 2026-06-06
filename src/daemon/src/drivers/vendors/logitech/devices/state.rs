//! Cached device state for `LogitechDevice` — the in-memory mirror of
//! everything queried from the hardware during `initialize`.
//!
//! ## Lock order
//!
//! `LogitechDevice` keeps two independent locks: `state: Arc<Mutex<LogitechDeviceState>>`
//! (this struct) and `transport: Mutex<LogitechTransport>`. They are *separate*
//! locks, not a single combined one.
//!
//! Invariant: the `state` lock is **never held across a `feature_request().await`**
//! (or any other transport I/O). When a method needs both, it locks `state`,
//! snapshots the fields it needs, drops the `state` guard, and only then performs
//! transport work — see `transport_snapshot()` and the `set_*` capability methods.
//! Future edits must preserve this: do not hold a `state` guard while awaiting
//! transport calls, otherwise the notification path (which also locks `state`)
//! can deadlock.

use std::collections::HashMap;

use crate::drivers::vendors::logitech::devices::pk_frame;
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use halod_protocol::types::{ButtonDescriptor, ButtonMapping, RgbState, RgbZone};

#[cfg(test)]
use halod_protocol::types::RgbColor;

// ── State sub-structs ─────────────────────────────────────────────────────────

/// Battery-related cached state.
#[derive(Default)]
pub(super) struct BatteryState {
    pub(super) battery_level: Option<u8>,
    pub(super) battery_charging: bool,
}

/// Report-rate cached state.
#[derive(Default)]
pub(super) struct ReportRateState {
    pub(super) report_rate_ms: Option<u8>,
    pub(super) report_rate_options: Vec<u8>, // available ms values
    pub(super) report_rate_ext: bool,        // true → uses 0x8061
}

/// Onboard-profile cached state: ROM layout and mode selection.
#[derive(Default)]
pub(super) struct ProfileState {
    pub(super) profile_steps: Vec<u16>,      // up to 5 steps in the active profile
    pub(super) profile_sector: u16,          // sector address of active profile
    pub(super) profile_sector_size: usize,   // full sector size in bytes
    pub(super) profile_dir: Vec<(u16, bool)>,// directory entries: (sector address, enabled)
    pub(super) rom_profile_count: u8,        // number of factory (ROM) profiles, from getInfo
    pub(super) onboard_mode: Option<u8>,     // None = absent; Some(0x01) = onboard; Some(0x02) = host
}

/// DPI cached state, including host-mode software DPI.
#[derive(Default)]
pub(super) struct DpiCache {
    pub(super) dpi_current: Option<u16>,
    pub(super) dpi_list: Vec<u16>,
    // Software DPI (host-mode DPI cycling, independent of onboard profiles)
    pub(super) software_dpi_steps: Vec<u16>,
    pub(super) software_dpi_index: usize,
}

/// RGB lighting cached state.
#[derive(Default)]
pub(super) struct RgbCacheState {
    pub(super) rgb_zones: Vec<RgbZone>,
    pub(super) rgb_state: Option<RgbState>,
    pub(super) rgb_static_slots: Vec<u8>, // static effect slot index per zone
    pub(super) rgb_use_pk_lighting: bool, // true → use PER_KEY_LIGHTING instead of RGB_EFFECTS
    pub(super) pk_led_ids: Vec<u8>,       // mouse per-key LED IDs from bitmap (0 < id < 32)
    /// Last per-key colours streamed to each zone, keyed by zone id, used to
    /// diff successive `write_frame` calls. See `pk_frame`.
    pub(super) pk_frame_cache: HashMap<String, pk_frame::PkFrameCache>,
}

/// Key-remapper cached state (REPROG_CONTROLS_V4 / 0x1b04).
#[derive(Default)]
pub(super) struct RemapState {
    pub(super) reprog_cids: Vec<ButtonDescriptor>,
    pub(super) button_mappings: Vec<ButtonMapping>,
    /// Last CID list from a divertedButtonsEvent, used to compute press/release deltas.
    pub(super) prev_diverted_cids: Vec<u16>,
}

// ── State cache ───────────────────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct LogitechDeviceState {
    pub(super) online: bool,
    pub(super) name: String,
    pub(super) features: HashMap<u16, u8>,

    pub(super) battery: BatteryState,
    pub(super) report_rate: ReportRateState,
    pub(super) dpi: DpiCache,
    pub(super) profile: ProfileState,
    pub(super) rgb: RgbCacheState,
    pub(super) remap: RemapState,
}

impl LogitechDeviceState {
    /// Whether key remapping needs the device to be in host mode.
    ///
    /// Only the `REPROG_CONTROLS_V4` backend uses per-control divert, which is
    /// gated on host mode. `GKEY` and `MOUSE_BUTTON_SPY` use a global
    /// software-control toggle that works regardless of onboard/host mode.
    pub(super) fn key_remap_requires_host_mode(&self) -> bool {
        !(self.features.contains_key(&feature::GKEY)
            || self.features.contains_key(&feature::MOUSE_BUTTON_SPY))
    }
}

/// ONBOARD_PROFILES getMode byte → is-host-mode. 0x02 = host, 0x01 = onboard.
pub(super) fn is_host_mode(mode: u8) -> bool {
    mode == 0x02
}

impl ProfileState {
    /// Whether the device is in host mode. The host-side DPI operations —
    /// software-DPI cycle, momentary DPI, direct set — are live only here;
    /// in onboard mode the device's own profiles govern DPI and those
    /// operations must be refused (the software list is seeded in both modes).
    pub(super) fn is_host(&self) -> bool {
        self.onboard_mode.map(is_host_mode).unwrap_or(false)
    }
}

impl DpiCache {
    /// The host-mode "current DPI" — the user's selected software-DPI step.
    ///
    /// Sourced from `software_dpi_steps[software_dpi_index]` (the user's
    /// explicit selection), falling back to the cached hardware DPI only when
    /// no step list is configured. Deliberately decoupled from `dpi_current`:
    /// the device firmware can shift the hardware DPI out from under us — e.g.
    /// a remapped button that also fires its native DPI-shift (double-fire) —
    /// so `dpi_current` must not drive what the UI shows or what a momentary-DPI
    /// press restores to on release.
    pub(super) fn host_current_dpi(&self) -> u16 {
        self.software_dpi_steps
            .get(self.software_dpi_index)
            .copied()
            .unwrap_or(self.dpi_current.unwrap_or(0))
    }

    /// Replace the host-mode software DPI step list, clamping the active index
    /// into the new list. Pure state mutation: the host-mode list lives in
    /// config, never on the device, so this performs no hardware I/O.
    pub(super) fn apply_host_steps(&mut self, steps: Vec<u16>) {
        self.software_dpi_index = self.software_dpi_index.min(steps.len().saturating_sub(1));
        self.software_dpi_steps = steps;
    }

    /// Apply an ONBOARD_PROFILES DPI step-index event (address 0x10): `data[0]`
    /// is the new active step index into `profile_steps`. On an in-range index
    /// it updates `dpi_current` and returns `(index, dpi)`; an out-of-range
    /// index (or empty `profile_steps`) is ignored and returns `None`.
    pub(super) fn apply_dpi_step_event(&mut self, profile_steps: &[u16], data: &[u8]) -> Option<(usize, u16)> {
        let step_idx = data.first().copied().unwrap_or(0) as usize;
        let dpi = *profile_steps.get(step_idx)?;
        self.dpi_current = Some(dpi);
        Some((step_idx, dpi))
    }

    /// Seed the host-mode software DPI list from the active onboard profile
    /// ("profile 1") on first discovery. No-op if a list is already present
    /// (e.g. restored from config) or there are no profile steps to seed from.
    pub(super) fn seed_software_dpi_from_profile(&mut self, profile_steps: &[u16]) {
        if !self.software_dpi_steps.is_empty() || profile_steps.is_empty() {
            return;
        }
        self.software_dpi_steps = profile_steps.to_vec();
        self.software_dpi_index = self
            .dpi_current
            .and_then(|cur| self.software_dpi_steps.iter().position(|&s| s == cur))
            .unwrap_or(0);
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    // ── Per-key frame diff cache ─────────────────────────────────────────────

    // apply(), set_online(false) and reinitialize_and_reapply() all clear
    // `pk_frame_cache` so the device's LED state can never silently diverge
    // from what write_frame's diff assumes. This verifies that clearing the
    // map forces the next streamed frame to be sent in full.
    #[test]
    fn pk_frame_cache_clear_forces_full_frame() {
        let mut state = LogitechDeviceState::default();
        let keys: Vec<(u8, RgbColor)> = (1u8..=20)
            .map(|id| (id, RgbColor { r: 0, g: 0, b: 255 }))
            .collect();
        let zone = "zone_0";

        // First frame against an empty cache → full send.
        let cache = state.rgb.pk_frame_cache.entry(zone.to_string()).or_default();
        assert!(!pk_frame::encode_frame(&keys, cache, 0x01, 0x09).is_empty());
        // Identical frame → diffs to nothing.
        let cache = state.rgb.pk_frame_cache.entry(zone.to_string()).or_default();
        assert!(pk_frame::encode_frame(&keys, cache, 0x01, 0x09).is_empty());
        // Cache cleared (as apply/set_online/reinit do) → full send again.
        state.rgb.pk_frame_cache.clear();
        let cache = state.rgb.pk_frame_cache.entry(zone.to_string()).or_default();
        assert!(!pk_frame::encode_frame(&keys, cache, 0x01, 0x09).is_empty());
    }

#[test]
    fn host_mode_boolean_value_mapping() {
        // is_host_mode decodes the wire byte: 0x02 = host → true, 0x01 = onboard → false.
        assert!(is_host_mode(0x02));
        assert!(!is_host_mode(0x01));

        // ProfileState::is_host() routes through is_host_mode; verify both mode bytes
        // produce the expected host/onboard reading via the actual production path.
        let host_state = ProfileState { onboard_mode: Some(0x02), ..ProfileState::default() };
        assert!(host_state.is_host(), "mode byte 0x02 must report host mode");

        let onboard_state = ProfileState { onboard_mode: Some(0x01), ..ProfileState::default() };
        assert!(!onboard_state.is_host(), "mode byte 0x01 must report onboard mode");
    }

    // ── Momentary-DPI restore point ──────────────────────────────────────────
    //
    // `host_current_dpi()` sources the value from the user's selected software
    // step (`software_dpi_steps[software_dpi_index]`), not the live hardware
    // DPI. The hardware DPI can be shifted by the device firmware itself (a
    // button that double-fires its native DPI action), so a momentary release
    // restores from the selected step rather than `dpi_current`.
    #[test]
    fn host_current_dpi_uses_selected_step_not_polluted_hardware_dpi() {
        let dpi = DpiCache {
            software_dpi_steps: vec![800, 1200, 1600, 2400, 3200],
            software_dpi_index: 4, // user selected 3200
            dpi_current: Some(800), // firmware double-fire shifted hardware to 800
            ..DpiCache::default()
        };
        // The restore point is the user's 3200, not the polluted 800.
        assert_eq!(dpi.host_current_dpi(), 3200);
    }

    #[test]
    fn host_current_dpi_falls_back_to_hardware_when_no_step_list() {
        // No software step list configured → fall back to cached hardware DPI.
        let dpi = DpiCache { dpi_current: Some(1600), ..DpiCache::default() };
        assert_eq!(dpi.host_current_dpi(), 1600);
        // Nothing known at all → 0 (callers treat 0 as "unknown").
        assert_eq!(DpiCache::default().host_current_dpi(), 0);
    }

    // ── Host-mode DPI step list ──────────────────────────────────────────────
    //
    // The host-mode step list is a config-only construct — editing it never
    // touches the device. `apply_host_steps` is pure state mutation: it stores
    // the list and clamps the active index into the new list.
    #[test]
    fn apply_host_steps_stores_list_and_clamps_index() {
        let mut dpi = DpiCache {
            software_dpi_steps: vec![800, 1200, 1600, 2400, 3200],
            software_dpi_index: 4,
            ..DpiCache::default()
        };
        // Shrinking the list clamps the out-of-range index to the new last slot.
        dpi.apply_host_steps(vec![400, 800]);
        assert_eq!(dpi.software_dpi_steps, vec![400, 800]);
        assert_eq!(dpi.software_dpi_index, 1);
        // An in-range index is left untouched.
        dpi.software_dpi_index = 0;
        dpi.apply_host_steps(vec![400, 800, 1600]);
        assert_eq!(dpi.software_dpi_index, 0);
    }

    #[test]
    fn seed_software_dpi_from_profile_seeds_only_when_empty() {
        // First discovery: no software list → seed from profile 1's steps,
        // and point the index at the live hardware DPI if it matches a step.
        let profile_steps = vec![800u16, 1200, 1600, 2400, 3200];
        let mut dpi = DpiCache {
            dpi_current: Some(1600),
            ..DpiCache::default()
        };
        dpi.seed_software_dpi_from_profile(&profile_steps);
        assert_eq!(dpi.software_dpi_steps, vec![800, 1200, 1600, 2400, 3200]);
        assert_eq!(dpi.software_dpi_index, 2); // 1600 is step index 2

        // Idempotent: an already-populated list (e.g. restored from config) is
        // never overwritten by a re-seed.
        let profile_steps2 = vec![800u16, 1200, 1600];
        let mut dpi2 = DpiCache {
            software_dpi_steps: vec![400, 800],
            software_dpi_index: 1,
            ..DpiCache::default()
        };
        dpi2.seed_software_dpi_from_profile(&profile_steps2);
        assert_eq!(dpi2.software_dpi_steps, vec![400, 800]);
        assert_eq!(dpi2.software_dpi_index, 1);
    }

    // ── Host-mode gate for DPI operations ────────────────────────────────────
    //
    // The software DPI step list is seeded in both modes (from profile 1), so
    // `set_dpi_index` / `set_dpi_direct` / DPI cycle / momentary check
    // `is_host()` to stay inert in onboard mode, where the device's own
    // profiles govern DPI.
    #[test]
    fn profile_state_is_host_reflects_onboard_mode() {
        assert!(ProfileState { onboard_mode: Some(0x02), ..ProfileState::default() }.is_host());
        assert!(!ProfileState { onboard_mode: Some(0x01), ..ProfileState::default() }.is_host());
        assert!(!ProfileState { onboard_mode: None, ..ProfileState::default() }.is_host());
    }

    // ── ONBOARD_PROFILES DPI step-index event (address 0x10) ─────────────────
    //
    // `apply_dpi_step_event` is the shared decoder for the wired notification
    // path and the wireless DPI watcher — `data[0]` indexes `profile_steps`.
    #[test]
    fn apply_dpi_step_event_updates_current_on_in_range_index() {
        let profile_steps = vec![800u16, 1600, 3200];
        let mut dpi = DpiCache {
            dpi_current: Some(800),
            ..DpiCache::default()
        };
        assert_eq!(dpi.apply_dpi_step_event(&profile_steps, &[2, 0, 0]), Some((2, 3200)));
        assert_eq!(dpi.dpi_current, Some(3200));
    }

    #[test]
    fn apply_dpi_step_event_ignores_out_of_range_or_empty() {
        let profile_steps = vec![800u16, 1600];
        let mut dpi = DpiCache {
            dpi_current: Some(800),
            ..DpiCache::default()
        };
        // Step index past the list → ignored, DPI unchanged.
        assert_eq!(dpi.apply_dpi_step_event(&profile_steps, &[5]), None);
        assert_eq!(dpi.dpi_current, Some(800));
        // Empty profile_steps → nothing to apply.
        let mut empty = DpiCache { dpi_current: Some(1200), ..DpiCache::default() };
        assert_eq!(empty.apply_dpi_step_event(&[], &[0]), None);
        assert_eq!(empty.dpi_current, Some(1200));
    }
}
