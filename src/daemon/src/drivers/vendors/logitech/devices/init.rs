//! Device initialisation — the `init_*` methods that query the hardware once
//! at startup and populate the cached `LogitechDeviceState`.

use std::collections::HashMap;

use anyhow::Result;

use crate::drivers::vendors::logitech::devices::device::LogitechDevice;
use crate::drivers::vendors::generic::devices::common::override_keyboard_layout;
use crate::drivers::vendors::logitech::devices::led_positions::leds_for_zone_info;
use crate::drivers::vendors::logitech::devices::state::LogitechDeviceState;
use crate::drivers::vendors::logitech::protocols::hidpp::{
    feature,
    controls::{cid_label, parse_cid_info},
    onboard::{parse_dpi_steps_from_sector, parse_profile_directory},
    rgb_effects::{find_native_effect, parse_pk_led_bitmap, parse_rgb_effect_table_entry, parse_rgb_zone_count},
    parse_current_dpi, parse_dpi_list, HidppMessenger,
};
use halod_protocol::types::{
    ButtonDescriptor, KeyboardLayout, LedPosition, NativeEffect, RgbColor, RgbDescriptor, RgbState,
    RgbZone, ZoneTopology,
};

impl LogitechDevice {
    // ── Initialisation steps ──────────────────────────────────────────────────

    pub(super) async fn init_features(&self) -> Result<HashMap<u16, u8>> {
        let (msg, devnum) = self.transport_snapshot().await;
        let table = msg.enumerate_features(devnum).await?;
        log::debug!(
            "[{}] Features: {:?}",
            self.id,
            table.keys().collect::<Vec<_>>()
        );
        Ok(table)
    }

    pub(super) async fn init_name(&self, features: &HashMap<u16, u8>) -> String {
        let Some(&idx) = features.get(&feature::DEVICE_NAME) else {
            return format!("Logitech Device");
        };
        let (msg, devnum) = self.transport_snapshot().await;
        // func=0x00 returns name length; func=0x10 + offset returns up to 16 chars
        let len = msg
            .feature_request(devnum, idx, 0x00, &[])
            .await
            .ok()
            .and_then(|r| r.first().copied())
            .unwrap_or(0) as usize;

        let mut name_bytes = Vec::with_capacity(len);
        let mut offset: u8 = 0;
        while name_bytes.len() < len {
            let chunk = msg
                .feature_request(devnum, idx, 0x10, &[offset])
                .await;
            match chunk {
                Ok(r) => {
                    let available = (len - name_bytes.len()).min(r.len());
                    name_bytes.extend_from_slice(&r[..available]);
                    offset += available as u8;
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&name_bytes).trim_end_matches('\0').to_string()
    }

    pub(super) async fn init_battery(&self, features: &HashMap<u16, u8>, state: &mut LogitechDeviceState) {
        let (msg, devnum) = self.transport_snapshot().await;
        // Try UNIFIED_BATTERY first (0x1004)
        if let Some(&idx) = features.get(&feature::UNIFIED_BATTERY) {
            match msg.feature_request(devnum, idx, 0x10, &[]).await {
                Ok(reply) if !reply.is_empty() => {
                    log::debug!("[{}] UNIFIED_BATTERY raw: {:02x?}", self.id, reply);
                    state.battery.battery_level = Some(reply[0]);
                    state.battery.battery_charging = reply.get(2).copied().unwrap_or(0) != 0;
                    return;
                }
                Ok(_) => log::warn!("[{}] UNIFIED_BATTERY returned empty reply", self.id),
                Err(e) => log::warn!("[{}] UNIFIED_BATTERY failed: {e}", self.id),
            }
        }
    }

    pub(super) async fn init_report_rate(&self, features: &HashMap<u16, u8>, state: &mut LogitechDeviceState) {
        let (msg, devnum) = self.transport_snapshot().await;
        // Try EXT_REPORT_RATE (0x8061) first
        if let Some(&idx) = features.get(&feature::EXT_REPORT_RATE) {
            let rates_reply = msg.feature_request(devnum, idx, 0x10, &[]).await;
            let cur_reply = msg.feature_request(devnum, idx, 0x20, &[]).await;
            match (&rates_reply, &cur_reply) {
                (Err(e), _) => log::warn!("[{}] EXT_REPORT_RATE rates failed: {e}", self.id),
                (_, Err(e)) => log::warn!("[{}] EXT_REPORT_RATE current failed: {e}", self.id),
                (Ok(r), Ok(c)) => log::debug!("[{}] EXT_REPORT_RATE rates={:02x?} cur={:02x?}", self.id, r, c),
            }
            if let (Ok(rates_data), Ok(cur_data)) = (rates_reply, cur_reply) {
                let rate_labels = ["8ms", "4ms", "2ms", "1ms", "500µs", "250µs", "125µs"];
                let rate_ms_map: [u8; 7] = [8, 4, 2, 1, 0, 0, 0]; // 0 = sub-ms, handled separately
                let flags = if rates_data.len() >= 2 {
                    ((rates_data[0] as u16) << 8) | rates_data[1] as u16
                } else {
                    rates_data.first().copied().unwrap_or(0) as u16
                };
                let mut options = Vec::new();
                for i in 0..rate_labels.len().min(7) {
                    if flags & (1 << i) != 0 {
                        options.push(rate_ms_map[i]);
                    }
                }
                let cur_idx = cur_data.first().copied().unwrap_or(3) as usize;
                state.report_rate.report_rate_ms = Some(rate_ms_map.get(cur_idx).copied().unwrap_or(1));
                state.report_rate.report_rate_options = options;
                state.report_rate.report_rate_ext = true;
                return;
            }
        }

        // Fallback: REPORT_RATE (0x8060)
        if let Some(&idx) = features.get(&feature::REPORT_RATE) {
            log::info!("[{}] report rate via REPORT_RATE fallback (0x8060)", self.id);
            let rates_reply = msg.feature_request(devnum, idx, 0x00, &[]).await;
            let cur_reply = msg.feature_request(devnum, idx, 0x10, &[]).await;
            match (&rates_reply, &cur_reply) {
                (Err(e), _) => log::warn!("[{}] REPORT_RATE rates failed: {e}", self.id),
                (_, Err(e)) => log::warn!("[{}] REPORT_RATE current failed: {e}", self.id),
                (Ok(r), Ok(c)) => log::debug!("[{}] REPORT_RATE rates={:02x?} cur={:02x?}", self.id, r, c),
            }
            if let (Ok(rates_data), Ok(cur_data)) = (rates_reply, cur_reply) {
                let flags = rates_data.first().copied().unwrap_or(0);
                let mut options = Vec::new();
                for i in 0..8u8 {
                    if (flags >> i) & 1 != 0 {
                        options.push(i + 1);
                    }
                }
                state.report_rate.report_rate_ms = cur_data.first().copied();
                state.report_rate.report_rate_options = options;
                state.report_rate.report_rate_ext = false;
            }
        }
    }

    pub(super) async fn init_dpi(&self, features: &HashMap<u16, u8>, state: &mut LogitechDeviceState) {
        let Some(&dpi_idx) = features.get(&feature::ADJUSTABLE_DPI) else {
            log::debug!("[{}] ADJUSTABLE_DPI not present", self.id);
            return;
        };
        log::debug!("[{}] ADJUSTABLE_DPI at index {dpi_idx}", self.id);
        let (msg, devnum) = self.transport_snapshot().await;

        // Fetch DPI list in chunks (func=0x10, param: sensor=0, dir=0, chunk_idx)
        let mut raw_dpi = Vec::new();
        for chunk_idx in 0u8..=0xFF {
            match msg
                .feature_request(devnum, dpi_idx, 0x10, &[0x00, 0x00, chunk_idx])
                .await
            {
                Ok(chunk) => {
                    // First byte is sensor index echo, rest is DPI data
                    let payload = if chunk.len() > 1 { &chunk[1..] } else { &chunk };
                    raw_dpi.extend_from_slice(payload);
                    // Stop when we see a 0x0000 terminator in the payload
                    if payload.windows(2).any(|w| w == [0, 0]) {
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("[{}] DPI list chunk {chunk_idx} failed: {e}", self.id);
                    break;
                }
            }
        }
        state.dpi.dpi_list = parse_dpi_list(&raw_dpi);

        // Current DPI via func=0x20
        match msg.feature_request(devnum, dpi_idx, 0x20, &[]).await {
            Ok(reply) => {
                log::debug!("[{}] ADJUSTABLE_DPI current raw: {:02x?}", self.id, reply);
                if let Some(dpi) = parse_current_dpi(&reply) {
                    state.dpi.dpi_current = Some(dpi);
                }
            }
            Err(e) => log::warn!("[{}] ADJUSTABLE_DPI GetCurrentDpi failed: {e}", self.id),
        }

        // DPI steps live in the active onboard-profile sector. `init_onboard`
        // runs first, so profile_sector / profile_sector_size are already set.
        if let Some(&op_idx) = features.get(&feature::ONBOARD_PROFILES) {
            self.init_profile_dpi_steps(op_idx, state).await;
        }

        // Seed the host-mode software DPI list from profile 1 on first
        // discovery. `load_state()` runs after `initialize()` and overrides
        // this when a list was previously saved to config; the seed only
        // survives when nothing was persisted (genuinely first discovery).
        let profile_steps = state.profile.profile_steps.clone();
        state.dpi.seed_software_dpi_from_profile(&profile_steps);
    }

    /// Read onboard-profile state (host/onboard mode, profile directory, active
    /// sector). Kept separate from `init_dpi` so devices without `ADJUSTABLE_DPI`
    /// — e.g. keyboards — still get host-mode detection. DPI steps are read
    /// separately by `init_dpi` via `init_profile_dpi_steps`.
    pub(super) async fn init_onboard(&self, features: &HashMap<u16, u8>, state: &mut LogitechDeviceState) {
        // Reset cached onboard state first so a re-init that no longer finds the
        // feature (or fails mid-read) can't leave a stale profile directory —
        // that would make the UI offer profile actions the device then rejects.
        state.profile.profile_dir.clear();
        state.profile.profile_steps.clear();
        state.profile.profile_sector = 0;
        state.profile.profile_sector_size = 0;
        state.profile.rom_profile_count = 0;
        state.profile.onboard_mode = None;
        if let Some(&op_idx) = features.get(&feature::ONBOARD_PROFILES) {
            self.init_onboard_profile(op_idx, state).await;
        }
    }

    pub(super) async fn init_onboard_profile(&self, op_idx: u8, state: &mut LogitechDeviceState) {
        let (msg, devnum) = self.transport_snapshot().await;
        // func 0x20 = getOnboardMode; reply[0]: 0x01 = onboard, 0x02 = host.
        // NOTE: func 0x10 is *setOnboardMode* — calling it to "read" the mode
        // (empty params) silently writes mode 0x00 and knocks the device out of
        // host mode. The get/set split mirrors 0x30 setCurrentProfile / 0x40
        // getCurrentProfile.
        match msg.feature_request(devnum, op_idx, 0x20, &[]).await {
            Ok(r) => state.profile.onboard_mode = r.first().copied(),
            Err(e) => log::warn!("[{}] ONBOARD_PROFILES getOnboardMode failed: {e}", self.id),
        }
        // func=0x00 → capability info: memory_type, profile_count, buttons, sectors, size, shift
        let info = match msg
            .feature_request(devnum, op_idx, 0x00, &[])
            .await
        {
            Ok(r) => {
                log::debug!("[{}] ONBOARD_PROFILES caps raw: {:02x?}", self.id, r);
                if r.len() >= 9 { r } else {
                    log::warn!("[{}] ONBOARD_PROFILES caps too short ({}b)", self.id, r.len());
                    return;
                }
            }
            Err(e) => {
                log::warn!("[{}] ONBOARD_PROFILES caps failed: {e}", self.id);
                return;
            }
        };
        // info[3] = profile count (writable RAM slots), info[4] = profile-count
        // OOB (factory ROM profiles). sector_size is at info[7:9] (BE u16).
        state.profile.rom_profile_count = info[4];
        let sector_size = u16::from_be_bytes([info[7], info[8]]) as usize;
        if sector_size == 0 {
            log::warn!("[{}] ONBOARD_PROFILES sector_size=0", self.id);
            return;
        }
        log::debug!(
            "[{}] ONBOARD_PROFILES sector_size={sector_size} profiles={} rom_profiles={}",
            self.id, info[3], info[4]
        );

        // Read the profile directory (sector 0x0000): a list of every profile
        // slot and whether it is enabled. Cached for profile management and as
        // the fallback active sector below.
        state.profile.profile_dir = match self.read_full_sector(op_idx, 0x0000, sector_size).await {
            Some(dir) => parse_profile_directory(&dir),
            None => {
                log::warn!("[{}] could not read profile directory", self.id);
                Vec::new()
            }
        };
        log::debug!("[{}] profile directory: {:?}", self.id, state.profile.profile_dir);

        // Get the active profile sector via func=0x40 (GET_CURRENT_PROFILE).
        // In host mode there is no active onboard profile, so this returns
        // [0xFF,0xFF]/[0x00,0x00] — fall back to the first enabled profile in
        // the directory so DPI steps stay populated regardless of mode.
        let first_enabled = state
            .profile.profile_dir
            .iter()
            .find(|(_, enabled)| *enabled)
            .map(|(s, _)| *s)
            .unwrap_or(0x0001);
        let sector = match msg.feature_request(devnum, op_idx, 0x40, &[]).await {
            Ok(r) if r.len() >= 2 && r[0..2] != [0xFF, 0xFF] && r[0..2] != [0x00, 0x00] => {
                let s = u16::from_be_bytes([r[0], r[1]]);
                log::debug!("[{}] ONBOARD_PROFILES current sector={s:#06x}", self.id);
                s
            }
            Ok(r) => {
                log::info!("[{}] GET_CURRENT_PROFILE has no active profile ({:02x?}); using directory", self.id, r);
                first_enabled
            }
            Err(e) => {
                log::warn!("[{}] GET_CURRENT_PROFILE failed: {e}; using directory", self.id);
                first_enabled
            }
        };

        state.profile.profile_sector = sector;
        state.profile.profile_sector_size = sector_size;
    }

    /// Parse the DPI step list from the active onboard-profile sector. Only
    /// meaningful for devices with `ADJUSTABLE_DPI` (mice) — keyboards have no
    /// DPI and store no steps. Requires `init_onboard_profile` to have already
    /// set `profile_sector` / `profile_sector_size`.
    pub(super) async fn init_profile_dpi_steps(&self, op_idx: u8, state: &mut LogitechDeviceState) {
        if state.profile.profile_sector_size == 0 {
            return;
        }
        match self
            .read_full_sector(op_idx, state.profile.profile_sector, state.profile.profile_sector_size)
            .await
        {
            Some(data) if data.len() >= 13 => {
                state.profile.profile_steps = parse_dpi_steps_from_sector(&data);
            }
            Some(data) => log::warn!("[{}] Profile sector too short ({}b)", self.id, data.len()),
            None => log::warn!("[{}] could not read profile sector for DPI steps", self.id),
        }
    }

    /// Enumerate remappable controls via REPROG_CONTROLS_V4 (0x1b04).
    pub(super) async fn init_reprog_controls(&self, features: &HashMap<u16, u8>, state: &mut LogitechDeviceState) {
        state.remap.reprog_cids.clear();
        let Some(&rc_idx) = features.get(&feature::REPROG_CONTROLS_V4) else { return };
        let (msg, devnum) = self.transport_snapshot().await;
        // getCount (func 0x00) → [count, ...]
        let count = match msg.feature_request(devnum, rc_idx, 0x00, &[]).await {
            Ok(r) => r.first().copied().unwrap_or(0),
            Err(e) => {
                log::warn!("[{}] REPROG_CONTROLS_V4 getCount failed: {e}", self.id);
                return;
            }
        };
        log::debug!("[{}] REPROG_CONTROLS_V4: {count} controls", self.id);
        for i in 0..count {
            // getCidInfo (func 0x10, param: index) → [cid_hi, cid_lo, task_hi, task_lo, flags, pos, group, gmask, ...]
            match msg.feature_request(devnum, rc_idx, 0x10, &[i]).await {
                Ok(r) => match parse_cid_info(&r) {
                    Some(info) => {
                        let divertable = (info.flags & 0x08) != 0;
                        let label = cid_label(info.cid, info.task_id);
                        log::debug!(
                            "[{}] CID {:#06x} task={:#06x} flags={:#04x} group={} divertable={divertable} label={label:?}",
                            self.id, info.cid, info.task_id, info.flags, info.group
                        );
                        state.remap.reprog_cids.push(ButtonDescriptor {
                            cid: info.cid,
                            label,
                            divertable,
                            group: info.group,
                        });
                    }
                    None => log::warn!("[{}] getCidInfo({i}) too short ({}b)", self.id, r.len()),
                },
                Err(e) => log::warn!("[{}] getCidInfo({i}) failed: {e}", self.id),
            }
        }
    }

    /// Enumerate a device's buttons via a bitmap-event backend — GKEY (0x8010)
    /// for gaming keyboards, MOUSE_BUTTON_SPY (0x8110) for gaming mice — and
    /// register them as remappable buttons.
    ///
    /// Both backends are used by devices that do not carry `REPROG_CONTROLS_V4`.
    /// There is no per-control divert — software control is global — so it is
    /// toggled in `apply_reprog_mappings` based on whether any mapping exists.
    /// Events arrive as the same 16-bit bitmap (see `handle_gkey_event` /
    /// `handle_button_bitmap_event`); each bit position is a `cid` (`bit index
    /// + 1`, matching `button_bitmap_to_cids`).
    ///
    /// Labels come from the profile's `bitmap_button_labels` table when present
    /// (the bitmap may be sparse — the G502 X Plus scatters its physical buttons
    /// across the 16 bits, and unmapped CIDs are skipped). When no table applies
    /// the label falls back to `"{bitmap_button_prefix} {n}"`.
    pub(super) async fn init_bitmap_buttons(&self, features: &HashMap<u16, u8>, state: &mut LogitechDeviceState) {
        let labels = self.profile.and_then(|p| p.bitmap_button_labels);
        let prefix = self.profile.map(|p| p.bitmap_button_prefix).unwrap_or("Button");

        // Determine the addressable button count for the active backend.
        // GKEY exposes a getCount call; MOUSE_BUTTON_SPY has no count so all 16
        // bitmap slots are considered.
        let usable: u16 = if let Some(&idx) = features.get(&feature::GKEY) {
            let (msg, devnum) = self.transport_snapshot().await;
            // GKEY func 0x00 = getCount → [count, ...].
            let count = match msg.feature_request(devnum, idx, 0x00, &[]).await {
                Ok(r) => r.first().copied().unwrap_or(0),
                Err(e) => {
                    log::warn!("[{}] GKEY getCount failed: {e}", self.id);
                    return;
                }
            };
            log::debug!("[{}] GKEY: {count} G-key(s)", self.id);
            // The gkeysEvent bitmap is 16-bit, so `button_bitmap_to_cids` only
            // decodes CIDs 1..=16 — any G-key past 16 could never fire. Clamp so
            // we don't register silently-dead buttons.
            let usable = u16::from(count).min(16);
            if u16::from(count) > usable {
                log::warn!(
                    "[{}] GKEY reports {count} G-keys; only the first 16 are addressable",
                    self.id
                );
            }
            usable
        } else if features.contains_key(&feature::MOUSE_BUTTON_SPY) {
            log::debug!(
                "[{}] MOUSE_BUTTON_SPY: registering buttons (table={})",
                self.id,
                labels.is_some()
            );
            16
        } else {
            return;
        };

        for n in 1..=usable {
            let label = match labels {
                Some(table) => match table.iter().find(|(cid, _)| *cid == n) {
                    Some((_, l)) => l.to_string(),
                    None => continue, // sparse bitmap: bit with no physical button
                },
                None => format!("{prefix} {n}"),
            };
            state.remap.reprog_cids.push(ButtonDescriptor {
                cid: n,
                label,
                divertable: true,
                group: 0,
            });
        }
    }

    pub(super) async fn init_keyboard_layout(&self, features: &HashMap<u16, u8>) -> KeyboardLayout {
        let Some(&idx) = features.get(&feature::KEYBOARD_LAYOUT_2) else {
            return KeyboardLayout::Unknown;
        };
        let (msg, devnum) = self.transport_snapshot().await;
        match msg.feature_request(devnum, idx, 0x00, &[]).await {
            Ok(r) if !r.is_empty() => {
                log::debug!("[{}] KEYBOARD_LAYOUT_2 country_code={}", self.id, r[0]);
                match r[0] {
                    1 => KeyboardLayout::US,
                    13 => KeyboardLayout::CH,
                    14 => KeyboardLayout::IT,
                    other => {
                        log::info!("[{}] Unknown keyboard layout country code {other}, treating as Unknown", self.id);
                        KeyboardLayout::Unknown
                    }
                }
            }
            Ok(_) => KeyboardLayout::Unknown,
            Err(e) => {
                log::warn!("[{}] KEYBOARD_LAYOUT_2 failed: {e}", self.id);
                KeyboardLayout::Unknown
            }
        }
    }

    // ── init_rgb helpers ──────────────────────────────────────────────────────

    /// Native effects advertised for this device's RGB zones, resolved from the
    /// `native_effects` id list in the device profile.
    fn default_native_effects(&self) -> Vec<NativeEffect> {
        self.profile
            .map(|p| p.native_effects)
            .unwrap_or(&[])
            .iter()
            .filter_map(|id| find_native_effect(id))
            .map(|e| NativeEffect {
                id: e.id.to_string(),
                name: e.name.to_string(),
                params: e.param_descriptors(),
            })
            .collect()
    }

    /// Commit the final zone list into the `rgb_descriptor` OnceLock and set the
    /// default RGB state (blue static). Idempotent — silently ignores a second call.
    fn commit_rgb_descriptor(&self, zones: Vec<RgbZone>, state: &mut LogitechDeviceState) {
        state.rgb.rgb_zones = zones;
        let _ = self.rgb_descriptor.set(RgbDescriptor {
            zones: state.rgb.rgb_zones.clone(),
            native_effects: self.default_native_effects(),
        });
        state.rgb.rgb_state = Some(RgbState::Static { color: RgbColor { r: 0, g: 0, b: 255 } });
    }

    /// Handle the fallback path: no RGB_EFFECTS feature but PER_KEY_LIGHTING is
    /// available. Builds a single synthetic zone from the device profile and marks
    /// the device as using the PK lighting path.
    fn init_rgb_pk_fallback(
        &self,
        pk_idx: u8,
        keyboard_layout: &KeyboardLayout,
        state: &mut LogitechDeviceState,
    ) {
        let zone_info = self.profile.map(|p| p.zones).unwrap_or(&[]).first();
        let name = zone_info.map(|z| z.name).unwrap_or("Lighting").to_string();
        let topology = override_keyboard_layout(
            zone_info.map(|z| z.topology.clone()).unwrap_or(ZoneTopology::Linear),
            keyboard_layout,
        );
        let key_layout = self.profile.and_then(|p| p.key_layout);
        let leds = zone_info
            .map(|zi| leds_for_zone_info(zi, key_layout))
            .unwrap_or_default();
        log::debug!("[{}] No RGB_EFFECTS, using PER_KEY_LIGHTING idx={pk_idx}", self.id);
        state.rgb.rgb_static_slots = vec![0];
        state.rgb.rgb_use_pk_lighting = true;
        let zones = vec![RgbZone { id: "zone_0".to_string(), name, topology, leds }];
        self.commit_rgb_descriptor(zones, state);
    }

    /// Query the RGB_EFFECTS feature for the total number of lighting zones.
    /// Returns `None` (and logs) if the request fails or the reply is malformed.
    async fn rgb_query_zone_count(
        &self,
        msg: &HidppMessenger,
        devnum: u8,
        idx: u8,
    ) -> Option<u8> {
        match msg.feature_request(devnum, idx, 0x00, &[0xFF, 0xFF, 0x00]).await {
            Ok(r) => {
                log::debug!("[{}] RGB GetInfo(global) raw: {:02x?}", self.id, r);
                match parse_rgb_zone_count(&r) {
                    Some(count) => Some(count),
                    None => {
                        log::warn!("[{}] RGB GetInfo too short ({}b)", self.id, r.len());
                        None
                    }
                }
            }
            Err(e) => {
                log::warn!("[{}] RGB GetInfo(global) failed: {e}", self.id);
                None
            }
        }
    }

    /// Build the `RgbZone` list and the parallel `static_slots` vector for every
    /// zone reported by the firmware. For each zone the static effect slot is
    /// located by scanning the effect table for effect_id 0x0001.
    async fn rgb_build_zones(
        &self,
        msg: &HidppMessenger,
        devnum: u8,
        idx: u8,
        zone_count: u8,
        keyboard_layout: &KeyboardLayout,
    ) -> (Vec<RgbZone>, Vec<u8>) {
        let mut zones = Vec::new();
        let mut static_slots = Vec::new();

        for z in 0..zone_count {
            // GET_INFO(z, 0xFF, 0x00) → effect_count at r[4]
            let effect_count = match msg.feature_request(devnum, idx, 0x00, &[z, 0xFF, 0x00]).await {
                Ok(r) => r.get(4).copied().unwrap_or(0),
                Err(e) => {
                    log::warn!("[{}] RGB GetInfo(zone={z}) failed: {e}", self.id);
                    0u8
                }
            };

            // Scan the whole effect table: log every slot's effect_id (so
            // per-device effect slots can be reverse-engineered) and record the
            // static effect slot (effect_id 0x0001).
            let mut static_slot = 0u8;
            for slot in 0..effect_count {
                match msg.feature_request(devnum, idx, 0x00, &[z, slot, 0x00]).await {
                    Ok(r) => {
                        if let Some(effect_id) = parse_rgb_effect_table_entry(&r) {
                            log::debug!(
                                "[{}] RGB effect table zone={z} slot={slot} effect_id={effect_id:#06x}",
                                self.id
                            );
                            if effect_id == 0x0001 {
                                static_slot = slot;
                            }
                        }
                    }
                    Err(e) => log::warn!("[{}] RGB GetInfo(zone={z}, slot={slot}) failed: {e}", self.id),
                }
            }
            static_slots.push(static_slot);

            let zone_info = self.profile.map(|p| p.zones).unwrap_or(&[]).get(z as usize);
            zones.push(RgbZone {
                id: format!("zone_{z}"),
                name: zone_info.map(|zi| zi.name.to_string()).unwrap_or_else(|| format!("Zone {z}")),
                topology: override_keyboard_layout(
                    zone_info.map(|zi| zi.topology.clone()).unwrap_or(ZoneTopology::Linear),
                    keyboard_layout,
                ),
                leds: zone_info
                    .map(|zi| leds_for_zone_info(zi, self.profile.and_then(|p| p.key_layout)))
                    .unwrap_or_default(),
            });
        }

        (zones, static_slots)
    }

    /// For non-keyboard devices: query the PER_KEY_LIGHTING_V2 bitmap (3 pages ×
    /// 13 bytes) to discover the actual firmware LED IDs (range 1..31). If more
    /// than one ID is found, the first zone's LED positions are rebuilt to match
    /// and the IDs are stored in `state.rgb.pk_led_ids` for use during RGB writes.
    async fn rgb_discover_mouse_pk_leds(
        &self,
        msg: &HidppMessenger,
        devnum: u8,
        pk_idx: u8,
        zones: &mut Vec<RgbZone>,
        state: &mut LogitechDeviceState,
    ) {
        let mut bitmap = vec![0u8; 39]; // 3 pages × 13 bytes
        for page in 0u8..3 {
            if let Ok(r) = msg.feature_request(devnum, pk_idx, 0x00, &[0x00, 0x00, page]).await {
                // response layout: [echo0][echo1][bm0..bm12]
                if r.len() >= 15 {
                    let start = page as usize * 13;
                    bitmap[start..start + 13].copy_from_slice(&r[2..15]);
                }
            }
        }

        let low_ids = parse_pk_led_bitmap(&bitmap);

        log::debug!("[{}] PK bitmap low_ids: {:?}", self.id, low_ids);

        if low_ids.len() > 1 && zones.len() <= 1 {
            let max = (low_ids.len() as f32 - 1.0).max(1.0);
            let leds: Vec<LedPosition> = low_ids.iter().enumerate()
                .map(|(i, &id)| LedPosition { id: id as u32, x: i as f32 / max, y: 0.5 })
                .collect();
            if let Some(zone) = zones.get_mut(0) {
                zone.leds = leds;
            }
            state.rgb.pk_led_ids = low_ids;
        }
    }

    // ── Top-level RGB initialisation ──────────────────────────────────────────

    pub(super) async fn init_rgb(&self, features: &HashMap<u16, u8>, keyboard_layout: &KeyboardLayout, state: &mut LogitechDeviceState) {
        let rgb_idx = features.get(&feature::RGB_EFFECTS).copied();

        let pk_idx = features.get(&feature::PER_KEY_LIGHTING_V2).copied();

        // No RGB_EFFECTS — try PER_KEY_LIGHTING fallback or give up.
        let Some(idx) = rgb_idx else {
            if let Some(pk) = pk_idx {
                self.init_rgb_pk_fallback(pk, keyboard_layout, state);
            } else {
                log::debug!("[{}] No RGB feature found", self.id);
            }
            return;
        };

        log::debug!("[{}] RGB_EFFECTS at index {idx}", self.id);
        let (msg, devnum) = self.transport_snapshot().await;

        let Some(zone_count) = self.rgb_query_zone_count(&msg, devnum, idx).await else {
            return;
        };
        log::debug!("[{}] RGB zone_count={zone_count}", self.id);

        if zone_count == 0 {
            log::warn!("[{}] RGB zone_count=0, skipping", self.id);
            return;
        }

        let (mut zones, static_slots) = self
            .rgb_build_zones(&msg, devnum, idx, zone_count, keyboard_layout)
            .await;

        // For mice: overlay the PK bitmap to get exact per-LED firmware IDs.
        if !self.is_keyboard() {
            if let Some(pk) = pk_idx {
                self.rgb_discover_mouse_pk_leds(&msg, devnum, pk, &mut zones, state).await;
            }
        }

        state.rgb.rgb_static_slots = static_slots;
        state.rgb.rgb_use_pk_lighting = false;
        self.commit_rgb_descriptor(zones, state);
    }
}
