// SPDX-License-Identifier: GPL-3.0-or-later
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
use std::sync::Arc;

use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::audio::EqReading;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::battery::BatterySource;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::rgb::PkFrameCache;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::settings::{
    ReportRateOption, MODE_HOST,
};
use halod_shared::types::{
    ButtonDescriptor, ButtonMapping, Choice, ChoiceOption, RgbState, RgbZone,
};

#[cfg(test)]
use halod_shared::types::RgbColor;

/// Battery-related cached state. `source` (resolved once from the feature table)
/// selects how the battery is read; `Unified` is notification-capable, `Voltage`
/// is re-polled on a timer.
#[derive(Default)]
pub(super) struct BatteryState {
    pub(super) battery_level: Option<u8>,
    pub(super) battery_charging: bool,
    pub(super) source: BatterySource,
}

/// Audio cached state (headset EQUALIZER + SIDETONE features).
#[derive(Default)]
pub(super) struct AudioState {
    /// Sync-readable mirror of the equalizer, shared by the async `set_eq_bands`
    /// path and the sync `EqualizerCapability::current_state` method.
    pub(super) eq: std::sync::Mutex<EqReading>,
    pub(super) sidetone: Option<u8>,
}

/// Report-rate cached state, mirrored from the protocol's report-rate read.
#[derive(Default)]
pub(super) struct ReportRateState {
    pub(super) options: Vec<ReportRateOption>,
    /// Currently-selected `wire_index`, if known.
    pub(super) current: Option<u8>,
    pub(super) ext: bool,
}

impl ReportRateState {
    /// Wire `Choice` for the report-rate dropdown, or `None` when no rates are
    /// known. `selected` indexes into `options`, matching the index
    /// `ChoiceCapability::set_choice` expects.
    pub(super) fn to_choice(&self) -> Option<Choice> {
        if self.options.is_empty() {
            return None;
        }
        let options: Vec<ChoiceOption> = self
            .options
            .iter()
            .map(|o| ChoiceOption {
                id: o.wire_index.to_string(),
                label: o.label.clone(),
            })
            .collect();
        let selected = self
            .current
            .and_then(|cur| self.options.iter().position(|o| o.wire_index == cur))
            .unwrap_or(0);
        Some(Choice {
            key: "report_rate".to_string(),
            label: "Report Rate".to_string(),
            options,
            selected,
            category: String::new(),
            display: Default::default(),
            visible_when: None,
        })
    }
}

/// Onboard-profile cached state: ROM layout and mode selection.
#[derive(Default)]
pub(super) struct ProfileState {
    pub(super) profile_steps: Vec<u16>, // up to 5 steps in the active profile
    pub(super) profile_sector: u16,     // sector address of active profile
    pub(super) profile_sector_size: usize, // full sector size in bytes
    pub(super) profile_dir: Vec<(u16, bool)>, // directory entries: (sector address, enabled)
    pub(super) rom_profile_count: u8,   // number of factory (ROM) profiles, from getInfo
    pub(super) onboard_mode: Option<u8>, // None = absent; Some(0x01) = onboard; Some(0x02) = host
}

/// DPI cached state, including host-mode software DPI.
pub(super) struct DpiCache {
    pub(super) dpi_current: Option<u16>,
    pub(super) dpi_list: Vec<u16>,
    // Software DPI (host-mode DPI cycling, independent of onboard profiles).
    // Wrapped in `std::sync::Mutex` so the sync `DpiCapability::save_state`
    // can read them without awaiting the outer async state lock.
    pub(super) software_dpi_steps: std::sync::Mutex<Vec<u16>>,
    pub(super) software_dpi_index: std::sync::Mutex<usize>,
}

impl Default for DpiCache {
    fn default() -> Self {
        Self {
            dpi_current: None,
            dpi_list: Vec::new(),
            software_dpi_steps: std::sync::Mutex::new(Vec::new()),
            software_dpi_index: std::sync::Mutex::new(0),
        }
    }
}

/// Which HID++ RGB protocol the device uses for zone-level and per-LED writes.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RgbWire {
    PerKey,
    #[default]
    RgbEffects,
    ColorLedEffects,
}

/// RGB lighting cached state.
#[derive(Default)]
pub(super) struct RgbCacheState {
    pub(super) rgb_zones: Vec<RgbZone>,
    pub(super) rgb_state: Option<RgbState>,
    pub(super) rgb_static_slots: Vec<u8>, // static effect slot index per zone
    pub(super) rgb_wire: RgbWire,         // which protocol to use (set once at init)
    pub(super) pk_led_ids: Vec<u8>,       // mouse per-key LED IDs from bitmap (0 < id < 32)
    /// Last per-key colours streamed to each zone, keyed by zone id, used to
    /// diff successive `write_frame` calls.
    pub(super) pk_frame_cache: HashMap<String, PkFrameCache>,
}

/// Key-remapper cached state (REPROG_CONTROLS_V4 / 0x1b04).
#[derive(Default)]
pub(super) struct RemapState {
    pub(super) reprog_cids: Vec<ButtonDescriptor>,
    pub(super) button_mappings: Vec<ButtonMapping>,
    /// Last CID list from a divertedButtonsEvent, used to compute press/release deltas.
    pub(super) prev_diverted_cids: Vec<u16>,
}

#[derive(Default)]
pub(super) struct LogitechDeviceState {
    pub(super) online: bool,
    pub(super) name: String,
    /// Feature table is immutable after `initialize()`; wrapped in `Arc` so
    /// `hidpp2()` does a cheap atomic increment instead of cloning the whole map.
    pub(super) features: Arc<HashMap<u16, u8>>,

    pub(super) battery: BatteryState,
    pub(super) report_rate: ReportRateState,
    pub(super) dpi: DpiCache,
    pub(super) profile: ProfileState,
    pub(super) rgb: RgbCacheState,
    pub(super) remap: RemapState,
    pub(super) audio: AudioState,
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

/// ONBOARD_PROFILES getMode byte → is-host-mode (`MODE_HOST` = 0x02).
pub(super) fn is_host_mode(mode: u8) -> bool {
    mode == MODE_HOST
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
    /// The host-mode "current DPI" — the user's selected software-DPI step,
    /// falling back to the cached hardware DPI only when no step list exists.
    /// Decoupled from `dpi_current`, which the firmware can shift out from under
    /// us (a button double-firing its native DPI action).
    pub(super) fn host_current_dpi(&self) -> u16 {
        let steps = self
            .software_dpi_steps
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let index = *self
            .software_dpi_index
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        steps
            .get(index)
            .copied()
            .unwrap_or(self.dpi_current.unwrap_or(0))
    }

    /// Replace the host-mode software DPI step list, clamping the active index
    /// into the new list. Pure state mutation: the host-mode list lives in
    /// config, never on the device, so this performs no hardware I/O.
    pub(super) fn apply_host_steps(&self, steps: Vec<u16>) {
        let mut index = self
            .software_dpi_index
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *index = (*index).min(steps.len().saturating_sub(1));
        *self
            .software_dpi_steps
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = steps;
    }

    /// Apply an ONBOARD_PROFILES DPI step-index event (address 0x10): `data[0]`
    /// is the new active step index into `profile_steps`. On an in-range index
    /// it updates `dpi_current` and returns `(index, dpi)`; an out-of-range
    /// index (or empty `profile_steps`) is ignored and returns `None`.
    pub(super) fn apply_dpi_step_event(
        &mut self,
        profile_steps: &[u16],
        data: &[u8],
    ) -> Option<(usize, u16)> {
        let step_idx = data.first().copied().unwrap_or(0) as usize;
        let dpi = *profile_steps.get(step_idx)?;
        self.dpi_current = Some(dpi);
        Some((step_idx, dpi))
    }

    /// Seed the host-mode software DPI list from the active onboard profile
    /// ("profile 1") on first discovery. No-op if a list is already present
    /// (e.g. restored from config) or there are no profile steps to seed from.
    pub(super) fn seed_software_dpi_from_profile(&self, profile_steps: &[u16]) {
        let mut steps = self
            .software_dpi_steps
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !steps.is_empty() || profile_steps.is_empty() {
            return;
        }
        *steps = profile_steps.to_vec();
        *self
            .software_dpi_index
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = self
            .dpi_current
            .and_then(|cur| steps.iter().position(|&s| s == cur))
            .unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::v2::rgb::per_key::encode_frame;

    fn rate_option(wire_index: u8, ms: u8, label: &str) -> ReportRateOption {
        ReportRateOption {
            wire_index,
            ms,
            label: label.to_string(),
        }
    }

    #[test]
    fn pk_frame_cache_clear_forces_full_frame() {
        let mut state = LogitechDeviceState::default();
        let keys: Vec<(u8, RgbColor)> = (1u8..=20)
            .map(|id| (id, RgbColor { r: 0, g: 0, b: 255 }))
            .collect();
        let zone = "zone_0";

        let cache = state
            .rgb
            .pk_frame_cache
            .entry(zone.to_string())
            .or_default();
        assert!(!encode_frame(&keys, cache, 0x01, 0x09).is_empty());
        let cache = state
            .rgb
            .pk_frame_cache
            .entry(zone.to_string())
            .or_default();
        assert!(encode_frame(&keys, cache, 0x01, 0x09).is_empty());
        state.rgb.pk_frame_cache.clear();
        let cache = state
            .rgb
            .pk_frame_cache
            .entry(zone.to_string())
            .or_default();
        assert!(!encode_frame(&keys, cache, 0x01, 0x09).is_empty());
    }

    #[test]
    fn host_mode_boolean_value_mapping() {
        assert!(is_host_mode(0x02));
        assert!(!is_host_mode(0x01));

        let host_state = ProfileState {
            onboard_mode: Some(0x02),
            ..ProfileState::default()
        };
        assert!(host_state.is_host(), "mode byte 0x02 must report host mode");

        let onboard_state = ProfileState {
            onboard_mode: Some(0x01),
            ..ProfileState::default()
        };
        assert!(
            !onboard_state.is_host(),
            "mode byte 0x01 must report onboard mode"
        );
    }

    fn dpi_with_steps(steps: Vec<u16>, index: usize, current: Option<u16>) -> DpiCache {
        DpiCache {
            dpi_current: current,
            software_dpi_steps: std::sync::Mutex::new(steps),
            software_dpi_index: std::sync::Mutex::new(index),
            ..DpiCache::default()
        }
    }

    #[test]
    fn host_current_dpi_uses_selected_step_not_polluted_hardware_dpi() {
        let dpi = dpi_with_steps(vec![800, 1200, 1600, 2400, 3200], 4, Some(800));
        assert_eq!(dpi.host_current_dpi(), 3200);
    }

    #[test]
    fn host_current_dpi_falls_back_to_hardware_when_no_step_list() {
        let dpi = DpiCache {
            dpi_current: Some(1600),
            ..DpiCache::default()
        };
        assert_eq!(dpi.host_current_dpi(), 1600);
        assert_eq!(DpiCache::default().host_current_dpi(), 0);
    }

    #[test]
    fn apply_host_steps_stores_list_and_clamps_index() {
        let dpi = dpi_with_steps(vec![800, 1200, 1600, 2400, 3200], 4, None);
        dpi.apply_host_steps(vec![400, 800]);
        assert_eq!(*dpi.software_dpi_steps.lock().unwrap(), vec![400u16, 800]);
        assert_eq!(*dpi.software_dpi_index.lock().unwrap(), 1);
        *dpi.software_dpi_index.lock().unwrap() = 0;
        dpi.apply_host_steps(vec![400, 800, 1600]);
        assert_eq!(*dpi.software_dpi_index.lock().unwrap(), 0);
    }

    #[test]
    fn seed_software_dpi_from_profile_seeds_only_when_empty() {
        let profile_steps = vec![800u16, 1200, 1600, 2400, 3200];
        let dpi = DpiCache {
            dpi_current: Some(1600),
            ..DpiCache::default()
        };
        dpi.seed_software_dpi_from_profile(&profile_steps);
        assert_eq!(
            *dpi.software_dpi_steps.lock().unwrap(),
            vec![800u16, 1200, 1600, 2400, 3200]
        );
        assert_eq!(*dpi.software_dpi_index.lock().unwrap(), 2);

        let profile_steps2 = vec![800u16, 1200, 1600];
        let dpi2 = dpi_with_steps(vec![400, 800], 1, None);
        dpi2.seed_software_dpi_from_profile(&profile_steps2);
        assert_eq!(*dpi2.software_dpi_steps.lock().unwrap(), vec![400u16, 800]);
        assert_eq!(*dpi2.software_dpi_index.lock().unwrap(), 1);
    }

    #[test]
    fn profile_state_is_host_reflects_onboard_mode() {
        assert!(ProfileState {
            onboard_mode: Some(0x02),
            ..ProfileState::default()
        }
        .is_host());
        assert!(!ProfileState {
            onboard_mode: Some(0x01),
            ..ProfileState::default()
        }
        .is_host());
        assert!(!ProfileState {
            onboard_mode: None,
            ..ProfileState::default()
        }
        .is_host());
    }

    #[test]
    fn apply_dpi_step_event_updates_current_on_in_range_index() {
        let profile_steps = vec![800u16, 1600, 3200];
        let mut dpi = DpiCache {
            dpi_current: Some(800),
            ..DpiCache::default()
        };
        assert_eq!(
            dpi.apply_dpi_step_event(&profile_steps, &[2, 0, 0]),
            Some((2, 3200))
        );
        assert_eq!(dpi.dpi_current, Some(3200));
    }

    #[test]
    fn apply_dpi_step_event_ignores_out_of_range_or_empty() {
        let profile_steps = vec![800u16, 1600];
        let mut dpi = DpiCache {
            dpi_current: Some(800),
            ..DpiCache::default()
        };
        assert_eq!(dpi.apply_dpi_step_event(&profile_steps, &[5]), None);
        assert_eq!(dpi.dpi_current, Some(800));
        let mut empty = DpiCache {
            dpi_current: Some(1200),
            ..DpiCache::default()
        };
        assert_eq!(empty.apply_dpi_step_event(&[], &[0]), None);
        assert_eq!(empty.dpi_current, Some(1200));
    }

    #[test]
    fn to_choice_none_when_no_options() {
        assert!(ReportRateState::default().to_choice().is_none());
    }

    #[test]
    fn to_choice_selected_indexes_options_by_wire_index() {
        let rr = ReportRateState {
            current: Some(2),
            options: vec![
                rate_option(8, 8, "8ms"),
                rate_option(4, 4, "4ms"),
                rate_option(2, 2, "2ms"),
                rate_option(1, 1, "1ms"),
            ],
            ext: false,
        };
        let c = rr.to_choice().expect("options present");
        assert_eq!(
            c.options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            ["8ms", "4ms", "2ms", "1ms"]
        );
        assert_eq!(c.selected, 2);
    }

    #[test]
    fn to_choice_ext_selected_matches_wire_index_not_position_guess() {
        let rr = ReportRateState {
            current: Some(3),
            options: vec![
                rate_option(3, 1, "1ms"),
                rate_option(4, 0, "500µs"),
                rate_option(5, 0, "250µs"),
                rate_option(6, 0, "125µs"),
            ],
            ext: true,
        };
        let c = rr.to_choice().expect("options present");
        assert_eq!(c.options.len(), 4);
        assert_eq!(
            c.options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            ["1ms", "500µs", "250µs", "125µs"]
        );
        assert_eq!(c.selected, 0);
    }

    // ── key_remap_requires_host_mode ──────────────────────────────────────

    fn make_state(features: Vec<(u16, u8)>) -> LogitechDeviceState {
        LogitechDeviceState {
            features: Arc::new(features.into_iter().collect()),
            ..LogitechDeviceState::default()
        }
    }

    #[test]
    fn remap_requires_host_mode_with_reprog_controls_only() {
        let s = make_state(vec![(feature::REPROG_CONTROLS_V4, 1)]);
        assert!(s.key_remap_requires_host_mode());
    }

    #[test]
    fn remap_does_not_require_host_mode_with_gkey() {
        let s = make_state(vec![(feature::GKEY, 1)]);
        assert!(!s.key_remap_requires_host_mode());
    }

    #[test]
    fn remap_does_not_require_host_mode_with_mouse_button_spy() {
        let s = make_state(vec![(feature::MOUSE_BUTTON_SPY, 1)]);
        assert!(!s.key_remap_requires_host_mode());
    }

    #[test]
    fn remap_requires_host_mode_with_empty_feature_map() {
        let s = make_state(vec![]);
        assert!(s.key_remap_requires_host_mode());
    }

    #[test]
    fn remap_requires_host_mode_even_with_unrelated_features() {
        let s = make_state(vec![
            (feature::ONBOARD_PROFILES, 1),
            (feature::ADJUSTABLE_DPI, 1),
        ]);
        assert!(s.key_remap_requires_host_mode());
    }
}
