// SPDX-License-Identifier: GPL-3.0-or-later
//! RGB lighting for `LogitechDevice` — the `RgbCapability` implementation and
//! its static / native-effect / per-LED write helpers. All wire work is done
//! through the protocol's typed RGB operations ([`Hidpp20`]); this file owns the
//! zone-descriptor assembly, LED-position layout, and frame caching.

use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;

use crate::drivers::vendors::generic::devices::common::override_keyboard_layout;
use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::led_positions::leds_for_zone_info;
use crate::drivers::vendors::logitech::devices::generic::state::LogitechDeviceState;
use crate::drivers::vendors::logitech::devices::generic::state::RgbWire;
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::rgb::find_native_effect;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::Hidpp20;
use crate::drivers::{RgbCapability, RgbStateSlot};
use halod_shared::types::{
    DeviceCapability, EffectParamValue, KeyboardLayout, NativeEffect, RgbColor, RgbDescriptor,
    RgbState, RgbZone, ZoneTopology,
};
use halod_shared::zone_transform::build_permutation;

/// Pick the wire for a device that reports RGB_EFFECTS. Per-LED streaming
/// (`PerKey`) needs the per-key feature AND zone LEDs that carry real firmware
/// IDs — keyboards get those from their static layout, mice only once PK
/// discovery has populated `pk_led_ids`. Anything else stays on whole-zone
/// `RgbEffects`, which `write_frame` collapses to `colors.first()`.
fn rgb_effects_wire(is_keyboard: bool, has_per_key: bool, pk_leds_discovered: bool) -> RgbWire {
    if has_per_key && (is_keyboard || pk_leds_discovered) {
        RgbWire::PerKey
    } else {
        RgbWire::RgbEffects
    }
}

fn collect_pairs(
    zones: &HashMap<String, HashMap<String, RgbColor>>,
    zone_map: &HashMap<String, usize>,
    filter: Option<&[u8]>,
) -> Vec<(u8, u8, u8, u8)> {
    let mut pairs = Vec::new();
    for (zone_id, led_colors) in zones {
        if !zone_map.contains_key(zone_id) {
            continue;
        }
        for (led_id_str, &color) in led_colors {
            let Ok(led_id) = led_id_str.parse::<u32>() else {
                continue;
            };
            let id = (led_id & 0xFF) as u8;
            if filter.is_none_or(|ids| ids.contains(&id)) {
                pairs.push((id, color.r, color.g, color.b));
            }
        }
    }
    pairs
}

impl LogitechDevice {
    // ── RGB write helpers ─────────────────────────────────────────────────────

    pub(super) async fn rgb_set_static(&self, color: RgbColor) -> Result<()> {
        let (zones, static_slots, wire) = {
            let state = self.state.lock().await;
            (
                state.rgb.rgb_zones.clone(),
                state.rgb.rgb_static_slots.clone(),
                state.rgb.rgb_wire,
            )
        };
        let hidpp = self.hidpp2().await;

        log::debug!(
            "[{}] rgb_set_static wire={wire:?} zones={} color={:02x}{:02x}{:02x}",
            self.id,
            zones.len(),
            color.r,
            color.g,
            color.b
        );

        match wire {
            RgbWire::PerKey => {
                hidpp.per_key_set_all(color).await.map_err(|e| {
                    log::warn!("[{}] PER_KEY set-all failed: {e}", self.id);
                    e
                })?;
            }
            RgbWire::ColorLedEffects => {
                let mut last_err = None;
                let mut ok_count = 0u8;
                for (i, zone) in zones.iter().enumerate() {
                    let zone_idx = i as u8;
                    let slot = static_slots.get(i).copied().unwrap_or(0);
                    match hidpp
                        .color_led_set_static_effect(zone_idx, slot, color)
                        .await
                    {
                        Ok(()) => ok_count += 1,
                        Err(e) => {
                            log::warn!(
                                "[{}] COLOR_LED SetEffect zone={} zone_idx={zone_idx} slot={slot} failed: {e}",
                                self.id, zone.id
                            );
                            last_err = Some(e);
                        }
                    }
                }
                if ok_count == 0 {
                    if let Some(e) = last_err {
                        return Err(e.context("COLOR_LED SetEffect failed for all zones"));
                    }
                }
            }
            RgbWire::RgbEffects => {
                let mut last_err = None;
                let mut ok_count = 0u8;
                for (i, zone) in zones.iter().enumerate() {
                    let slot = static_slots.get(i).copied().unwrap_or(0);
                    let zone_idx = i as u8;
                    match hidpp.rgb_set_static_effect(zone_idx, slot, color).await {
                        Ok(()) => ok_count += 1,
                        Err(e) => {
                            log::warn!(
                                "[{}] RGB SetEffect zone={} zone_idx={zone_idx} failed: {e}",
                                self.id,
                                zone.id
                            );
                            last_err = Some(e);
                        }
                    }
                }
                if ok_count == 0 {
                    if let Some(e) = last_err {
                        return Err(e.context("RGB SetEffect failed for all zones"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Apply a native firmware effect via the RGB_EFFECTS `SetEffect` call.
    pub(super) async fn rgb_set_native_effect(
        &self,
        id: &str,
        values: &HashMap<String, EffectParamValue>,
    ) -> Result<()> {
        self.hidpp2().await.rgb_set_native_effect(id, values).await
    }

    /// Apply each zone's LED-content transform to a per-LED colour map.
    /// Physical LED `zone.leds[i]` takes the colour assigned to `zone.leds[perm[i]]`.
    fn transform_perled_map<'a>(
        &self,
        zones_map: &'a HashMap<String, HashMap<String, RgbColor>>,
    ) -> Cow<'a, HashMap<String, HashMap<String, RgbColor>>> {
        let Some(descriptor) = self.rgb_descriptor.get() else {
            return Cow::Borrowed(zones_map);
        };
        Cow::Owned(
            zones_map
                .iter()
                .map(|(zid, led_map)| {
                    let transform = self.rgb.transform_for(zid);
                    let zone = descriptor.zones.iter().find(|z| &z.id == zid);
                    match zone {
                        Some(zone) if !transform.is_identity() => {
                            let perm = build_permutation(zone, &transform);
                            let new_map: HashMap<String, RgbColor> = (0..zone.leds.len())
                                .filter_map(|i| {
                                    let src = zone.leds[perm[i]].id.to_string();
                                    led_map.get(&src).map(|c| (zone.leds[i].id.to_string(), *c))
                                })
                                .collect();
                            (zid.clone(), new_map)
                        }
                        _ => (zid.clone(), led_map.clone()),
                    }
                })
                .collect(),
        )
    }

    /// Whole-zone fallback for a per-LED write: paint each zone with
    /// its first LED's colour via `SetEffect`. Dispatches to the correct
    /// protocol based on `wire`.
    async fn rgb_set_per_led_via_effects(
        &self,
        hidpp: &Hidpp20,
        zones: &HashMap<String, HashMap<String, RgbColor>>,
        zone_map: &HashMap<String, usize>,
        wire: RgbWire,
    ) {
        let static_slots = {
            let st = self.state.lock().await;
            st.rgb.rgb_static_slots.clone()
        };
        for (zone_id, led_colors) in zones {
            let Some(&seq_idx) = zone_map.get(zone_id) else {
                continue;
            };
            if let Some(&color) = led_colors.values().next() {
                let slot = static_slots.get(seq_idx).copied().unwrap_or(0);
                let result = match wire {
                    RgbWire::ColorLedEffects => {
                        hidpp
                            .color_led_set_static_effect(seq_idx as u8, slot, color)
                            .await
                    }
                    _ => {
                        hidpp
                            .rgb_set_static_effect(seq_idx as u8, slot, color)
                            .await
                    }
                };
                if let Err(e) = result {
                    let label = match wire {
                        RgbWire::ColorLedEffects => "COLOR_LED",
                        _ => "RGB_EFFECTS",
                    };
                    log::warn!("[{}] {label} zone={zone_id} failed: {e}", self.id);
                }
            }
        }
    }

    pub(super) async fn rgb_set_per_led(
        &self,
        zones_map: &HashMap<String, HashMap<String, RgbColor>>,
    ) -> Result<()> {
        let transformed = self.transform_perled_map(zones_map);
        let zones = &*transformed;
        let hidpp = self.hidpp2().await;
        let (wire, zone_map, has_pk) = {
            let state = self.state.lock().await;
            let zone_map: HashMap<String, usize> = state
                .rgb
                .rgb_zones
                .iter()
                .enumerate()
                .map(|(i, z)| (z.id.clone(), i))
                .collect();
            let has_pk = state.features.contains_key(&feature::PER_KEY_LIGHTING_V2);
            (state.rgb.rgb_wire, zone_map, has_pk)
        };

        if has_pk {
            if self.is_keyboard() {
                let pairs = collect_pairs(zones, &zone_map, None);
                if let Err(e) = hidpp.write_per_key_pairs(&pairs).await {
                    log::warn!("[{}] PER_KEY_LIGHTING burst failed: {e}", self.id);
                }
            } else {
                let pk_led_ids = {
                    let st = self.state.lock().await;
                    st.rgb.pk_led_ids.clone()
                };
                if !pk_led_ids.is_empty() {
                    let pairs = collect_pairs(zones, &zone_map, Some(&pk_led_ids));
                    if let Err(e) = hidpp.write_per_key_pairs(&pairs).await {
                        log::warn!("[{}] PER_KEY_LIGHTING burst failed: {e}", self.id);
                    }
                } else {
                    self.rgb_set_per_led_via_effects(&hidpp, zones, &zone_map, wire)
                        .await;
                }
            }
        } else {
            self.rgb_set_per_led_via_effects(&hidpp, zones, &zone_map, wire)
                .await;
        }
        Ok(())
    }

    /// Canonical "restore RGB control" sequence. Several operations
    /// (profile-sector flash, report-rate change, host-mode switch, wireless
    /// reconnect) cause the firmware to reclaim LED control. After any of them,
    /// re-enable software LED control and re-apply the last cached RGB state.
    pub(super) async fn restore_rgb_control(&self) {
        let wire = self.state.lock().await.rgb.rgb_wire;
        let hidpp = self.hidpp2().await;
        match wire {
            RgbWire::ColorLedEffects => hidpp.color_led_enable_sw_control().await,
            RgbWire::RgbEffects | RgbWire::PerKey => hidpp.rgb_enable_sw_control().await,
        }
        let current_rgb = self.state.lock().await.rgb.rgb_state.clone();
        if let Some(rgb_state) = current_rgb {
            if let Err(e) = self.apply(rgb_state).await {
                log::warn!(
                    "[{}] restore_rgb_control: failed to re-apply RGB state: {e}",
                    self.id
                );
            }
        }
    }

    // ── RGB initialisation ────────────────────────────────────────────────────

    pub(super) async fn init_keyboard_layout(&self, features: &HashMap<u16, u8>) -> KeyboardLayout {
        self.hidpp2_with(features)
            .await
            .read_keyboard_layout()
            .await
    }

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
    /// default RGB state (blue static). Idempotent.
    fn commit_rgb_descriptor(&self, zones: Vec<RgbZone>, state: &mut LogitechDeviceState) {
        state.rgb.rgb_zones = zones;
        let _ = self.rgb_descriptor.set(RgbDescriptor {
            zones: state.rgb.rgb_zones.clone(),
            native_effects: self.default_native_effects(),
        });
        state.rgb.rgb_state = Some(RgbState::Static {
            color: RgbColor { r: 0, g: 0, b: 255 },
        });
    }

    /// No RGB_EFFECTS but PER_KEY_LIGHTING available: build a single synthetic
    /// zone from the device profile and mark the device as using the PK path.
    fn init_rgb_pk_fallback(
        &self,
        pk_idx: u8,
        keyboard_layout: &KeyboardLayout,
        state: &mut LogitechDeviceState,
    ) {
        let zone_info = self.profile.map(|p| p.zones).unwrap_or(&[]).first();
        let name = zone_info.map(|z| z.name).unwrap_or("Lighting").to_string();
        let topology = override_keyboard_layout(
            zone_info
                .map(|z| z.topology.clone())
                .unwrap_or(ZoneTopology::Linear),
            keyboard_layout,
        );
        let key_layout = self.profile.and_then(|p| p.key_layout);
        let leds = zone_info
            .map(|zi| leds_for_zone_info(zi, key_layout))
            .unwrap_or_default();
        log::debug!(
            "[{}] No RGB_EFFECTS, using PER_KEY_LIGHTING idx={pk_idx}",
            self.id
        );
        state.rgb.rgb_static_slots = vec![0];
        state.rgb.rgb_wire = RgbWire::PerKey;
        let zones = vec![RgbZone {
            id: "zone_0".to_string(),
            name,
            topology,
            leds,
        }];
        self.commit_rgb_descriptor(zones, state);
    }

    /// Build the `RgbZone` list and the parallel `static_slots` vector for every
    /// zone reported by the firmware. The static effect slot is located by
    /// scanning the effect table for effect_id 0x0001.
    async fn rgb_build_zones(
        &self,
        hidpp: &Hidpp20,
        zone_count: u8,
        keyboard_layout: &KeyboardLayout,
    ) -> (Vec<RgbZone>, Vec<u8>) {
        let mut zones = Vec::new();
        let mut static_slots = Vec::new();

        for z in 0..zone_count {
            let effect_count = hidpp.rgb_zone_effect_count(z).await;

            // Scan the whole effect table: log every slot's effect_id and record
            // the static effect slot (effect_id 0x0001).
            let mut static_slot = 0u8;
            for slot in 0..effect_count {
                if let Some(effect_id) = hidpp.rgb_effect_id(z, slot).await {
                    log::debug!(
                        "[{}] RGB effect table zone={z} slot={slot} effect_id={effect_id:#06x}",
                        self.id
                    );
                    if effect_id == 0x0001 {
                        static_slot = slot;
                    }
                }
            }
            static_slots.push(static_slot);

            let zone_info = self.profile.map(|p| p.zones).unwrap_or(&[]).get(z as usize);
            zones.push(RgbZone {
                id: format!("zone_{z}"),
                name: zone_info
                    .map(|zi| zi.name.to_string())
                    .unwrap_or_else(|| format!("Zone {z}")),
                topology: override_keyboard_layout(
                    zone_info
                        .map(|zi| zi.topology.clone())
                        .unwrap_or(ZoneTopology::Linear),
                    keyboard_layout,
                ),
                leds: zone_info
                    .map(|zi| leds_for_zone_info(zi, self.profile.and_then(|p| p.key_layout)))
                    .unwrap_or_default(),
            });
        }

        (zones, static_slots)
    }

    /// For non-keyboard devices: discover the firmware LED IDs from the
    /// PER_KEY_LIGHTING bitmap. If more than one ID is found, the first zone's
    /// LED positions are rebuilt to match and the IDs stored for RGB writes.
    async fn rgb_discover_mouse_pk_leds(
        &self,
        hidpp: &Hidpp20,
        zones: &mut [RgbZone],
        state: &mut LogitechDeviceState,
    ) {
        let mut low_ids = hidpp.read_pk_led_ids().await;
        log::debug!("[{}] PK bitmap low_ids: {:?}", self.id, low_ids);

        if let Some(layout) = self.profile.and_then(|p| p.key_layout) {
            let ordered: Vec<u8> = layout
                .cid_map
                .iter()
                .filter_map(|(fid, _)| {
                    let id = *fid as u8;
                    low_ids.contains(&id).then_some(id)
                })
                .collect();
            if ordered.len() == low_ids.len() {
                low_ids = ordered;
                log::debug!(
                    "[{}] PK bitmap reordered via key_layout: {:?}",
                    self.id,
                    low_ids
                );
            }
        }

        if low_ids.len() > 1 && zones.len() <= 1 {
            let leds = super::led_positions::led_strip_from_ids(&low_ids);
            if let Some(zone) = zones.get_mut(0) {
                zone.leds = leds;
            }
            state.rgb.pk_led_ids = low_ids;
        }
    }

    /// Build zones via COLOR_LED_EFFECTS (0x8070) — same shape as
    /// `rgb_build_zones` but uses the 0x8070 function codes and derives
    /// zone names / LED positions from the firmware-reported location.
    async fn color_led_build_zones(
        &self,
        hidpp: &Hidpp20,
        zone_count: u8,
        keyboard_layout: &KeyboardLayout,
    ) -> (Vec<RgbZone>, Vec<u8>) {
        use crate::drivers::vendors::logitech::protocols::hidpp::v2::rgb::color_led::color_led_location_name;

        let mut zones = Vec::new();
        let mut static_slots = Vec::new();

        for z in 0..zone_count {
            let (_, location, effect_count) =
                hidpp.color_led_zone_info(z).await.unwrap_or((z, 0, 0));
            log::debug!(
                "[{}] COLOR_LED zone={z} location={location} effect_count={effect_count}",
                self.id
            );

            let mut static_slot = 0u8;
            for slot in 0..effect_count {
                if let Some(effect_id) = hidpp.color_led_effect_id(z, slot).await {
                    log::debug!(
                        "[{}] COLOR_LED effect table zone={z} slot={slot} effect_id={effect_id:#06x}",
                        self.id
                    );
                    if effect_id == 0x0001 {
                        static_slot = slot;
                    }
                }
            }
            static_slots.push(static_slot);

            let zone_info = self.profile.map(|p| p.zones).unwrap_or(&[]).get(z as usize);
            zones.push(RgbZone {
                id: format!("zone_{z}"),
                name: zone_info
                    .map(|zi| zi.name.to_string())
                    .unwrap_or_else(|| color_led_location_name(location).to_string()),
                topology: override_keyboard_layout(
                    zone_info
                        .map(|zi| zi.topology.clone())
                        .unwrap_or(ZoneTopology::Linear),
                    keyboard_layout,
                ),
                leds: zone_info
                    .map(|zi| leds_for_zone_info(zi, self.profile.and_then(|p| p.key_layout)))
                    .unwrap_or_else(|| super::led_positions::mouse_led_positions(1)),
            });
        }

        (zones, static_slots)
    }

    pub(super) async fn init_rgb(
        &self,
        features: &HashMap<u16, u8>,
        keyboard_layout: &KeyboardLayout,
        state: &mut LogitechDeviceState,
    ) {
        let has_rgb = features.contains_key(&feature::RGB_EFFECTS);
        let has_color_led = features.contains_key(&feature::COLOR_LED_EFFECTS);
        let pk_idx = features.get(&feature::PER_KEY_LIGHTING_V2).copied();

        // No RGB_EFFECTS — try COLOR_LED_EFFECTS, then PER_KEY_LIGHTING, or give up.
        if !has_rgb {
            if has_color_led {
                let hidpp = self.hidpp2_with(features).await;
                let Some(zone_count) = hidpp.color_led_zone_count().await else {
                    return;
                };
                log::debug!("[{}] COLOR_LED zone_count={zone_count}", self.id);

                if zone_count == 0 {
                    log::warn!("[{}] COLOR_LED zone_count=0, skipping", self.id);
                    return;
                }

                let (zones, static_slots) = self
                    .color_led_build_zones(&hidpp, zone_count, keyboard_layout)
                    .await;

                state.rgb.rgb_static_slots = static_slots;
                state.rgb.rgb_wire = RgbWire::ColorLedEffects;
                self.commit_rgb_descriptor(zones, state);
            } else if let Some(pk) = pk_idx {
                self.init_rgb_pk_fallback(pk, keyboard_layout, state);
            } else {
                log::debug!("[{}] No RGB feature found", self.id);
            }
            return;
        }

        let hidpp = self.hidpp2_with(features).await;
        let Some(zone_count) = hidpp.rgb_zone_count().await else {
            return;
        };
        log::debug!("[{}] RGB zone_count={zone_count}", self.id);

        if zone_count == 0 {
            log::warn!("[{}] RGB zone_count=0, skipping", self.id);
            return;
        }

        let (mut zones, static_slots) = self
            .rgb_build_zones(&hidpp, zone_count, keyboard_layout)
            .await;

        // For mice: overlay the PK bitmap to get exact per-LED firmware IDs.
        if !self.is_keyboard() && pk_idx.is_some() {
            self.rgb_discover_mouse_pk_leds(&hidpp, &mut zones, state)
                .await;
        }

        state.rgb.rgb_static_slots = static_slots;
        let pk_leds_discovered = !state.rgb.pk_led_ids.is_empty();
        state.rgb.rgb_wire =
            rgb_effects_wire(self.is_keyboard(), pk_idx.is_some(), pk_leds_discovered);
        self.commit_rgb_descriptor(zones, state);
    }
}

// ── RgbCapability ─────────────────────────────────────────────────────────────

#[async_trait]
impl RgbCapability for LogitechDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        static EMPTY: std::sync::OnceLock<RgbDescriptor> = std::sync::OnceLock::new();
        self.rgb_descriptor.get().unwrap_or_else(|| {
            EMPTY.get_or_init(|| RgbDescriptor {
                zones: vec![],
                native_effects: vec![],
            })
        })
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        // Only devices with discovered zones and a built descriptor expose RGB.
        if self.state.lock().await.rgb.rgb_zones.is_empty() || self.rgb_descriptor.get().is_none() {
            return None;
        }
        Some(DeviceCapability::Rgb(self.serialize_rgb()))
    }

    fn save_state(&self) -> serde_json::Value {
        let canvas = self.canvas_zones();
        let transforms = self.zone_transforms();
        let mut obj = serde_json::Map::new();
        if let Some(s) = self.current_state() {
            obj.insert("state".into(), serde_json::to_value(s).unwrap_or_default());
        }
        if !canvas.is_empty() {
            obj.insert(
                "canvas_zones".into(),
                serde_json::to_value(canvas).unwrap_or_default(),
            );
        }
        if !transforms.is_empty() {
            obj.insert(
                "rgb_transforms".into(),
                serde_json::to_value(transforms).unwrap_or_default(),
            );
        }
        if obj.is_empty() {
            serde_json::Value::Null
        } else {
            obj.into()
        }
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        // Restore rgb_state first, before host-mode is set, so restore_rgb_control
        // re-applies the correct profile colour.
        if let Some(s) = v.get("state") {
            if let Ok(state) = serde_json::from_value(s.clone()) {
                self.state.lock().await.rgb.rgb_state = Some(state);
            }
        }
        if let Some(z) = v.get("canvas_zones") {
            if let Ok(zones) = serde_json::from_value(z.clone()) {
                self.set_canvas_zones(zones);
            }
        }
        if let Some(t) = v.get("rgb_transforms") {
            if let Ok(transforms) = serde_json::from_value(t.clone()) {
                self.set_zone_transforms(transforms);
            }
        }
    }

    async fn apply(&self, new_state: RgbState) -> Result<()> {
        // Static/PerLed/NativeEffect overwrite the hardware LEDs directly, so the
        // next streamed frame must be sent in full rather than diffed against a
        // now-stale cache.
        if !matches!(new_state, RgbState::Engine | RgbState::DirectEffect { .. }) {
            self.state.lock().await.rgb.pk_frame_cache.clear();
        }
        // Re-acquire software LED control before writing. When the device is in
        // onboard mode the firmware owns the LEDs and will ignore host commands;
        // RGB_SET_SW_CONTROL tells it to hand control back so our SetEffect /
        // PER_KEY writes take effect.
        if !matches!(new_state, RgbState::Engine | RgbState::DirectEffect { .. }) {
            let hidpp = self.hidpp2().await;
            match self.state.lock().await.rgb.rgb_wire {
                RgbWire::ColorLedEffects => hidpp.color_led_enable_sw_control().await,
                _ => hidpp.rgb_enable_sw_control().await,
            }
        }
        let write_result = match &new_state {
            RgbState::Static { color } => self.rgb_set_static(*color).await,
            RgbState::PerLed { zones } => self.rgb_set_per_led(zones).await,
            RgbState::NativeEffect { id, params } => self.rgb_set_native_effect(id, params).await,
            RgbState::Engine | RgbState::DirectEffect { .. } => Ok(()),
        };
        // Record the requested state even when the hardware write failed.
        self.state.lock().await.rgb.rgb_state = Some(new_state);
        write_result
    }

    fn current_state(&self) -> Option<RgbState> {
        self.state
            .try_lock()
            .ok()
            .and_then(|s| s.rgb.rgb_state.clone())
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        let (zone_idx, wire, pk_idx, leds) = {
            let state = self.state.lock().await;
            let (zone_idx, leds) = state
                .rgb
                .rgb_zones
                .iter()
                .enumerate()
                .find(|(_, z)| z.id == zone_id)
                .map(|(i, z)| (i as u8, z.leds.clone()))
                .unwrap_or((0, vec![]));
            (
                Some(zone_idx),
                state.rgb.rgb_wire,
                state.features.get(&feature::PER_KEY_LIGHTING_V2).copied(),
                leds,
            )
        };

        let hidpp = self.hidpp2().await;

        let (is_wired, coordinator) = {
            let t = self.transport.lock().await;
            (t.is_wired, t.coordinator.clone())
        };

        match wire {
            RgbWire::PerKey if pk_idx.is_some() => {
                let keys: Vec<(u8, RgbColor)> = leds
                    .iter()
                    .zip(colors.iter())
                    .map(|(lp, &c)| ((lp.id & 0xFF) as u8, c))
                    .collect();

                let packets = {
                    let mut state = self.state.lock().await;
                    let cache = state
                        .rgb
                        .pk_frame_cache
                        .entry(zone_id.to_string())
                        .or_default();
                    hidpp.encode_per_key_frame(&keys, cache)
                };
                if packets.is_empty() {
                    return Ok(());
                }

                if !is_wired {
                    if let Some(coord) = &coordinator {
                        coord.post(hidpp.devnum(), packets).await;
                        return Ok(());
                    }
                }
                if let Err(e) = hidpp.send_packets(packets).await {
                    log::warn!("[{}] write_frame: send_packets failed: {e}", self.id);
                }
            }
            RgbWire::ColorLedEffects => {
                let Some(zone_idx) = zone_idx else {
                    return Ok(());
                };
                if let Some(&c) = colors.first() {
                    let static_slot = {
                        let st = self.state.lock().await;
                        st.rgb
                            .rgb_static_slots
                            .get(zone_idx as usize)
                            .copied()
                            .unwrap_or(0)
                    };
                    if let Err(e) = hidpp
                        .color_led_set_static_effect(zone_idx, static_slot, c)
                        .await
                    {
                        log::warn!(
                            "[{}] write_frame: color_led_set_static_effect failed: {e}",
                            self.id
                        );
                    }
                }
            }
            RgbWire::RgbEffects | RgbWire::PerKey => {
                let Some(zone_idx) = zone_idx else {
                    return Ok(());
                };
                if let Some(&c) = colors.first() {
                    let static_slot = {
                        let st = self.state.lock().await;
                        st.rgb
                            .rgb_static_slots
                            .get(zone_idx as usize)
                            .copied()
                            .unwrap_or(0)
                    };
                    if let Err(e) = hidpp.rgb_set_static_effect(zone_idx, static_slot, c).await {
                        log::warn!(
                            "[{}] write_frame: rgb_set_static_effect failed: {e}",
                            self.id
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_with_per_key_streams_per_led() {
        // Keyboard LEDs always carry real firmware IDs, so discovery is moot.
        assert_eq!(rgb_effects_wire(true, true, false), RgbWire::PerKey);
        assert_eq!(rgb_effects_wire(true, true, true), RgbWire::PerKey);
    }

    #[test]
    fn keyboard_without_per_key_stays_rgb_effects() {
        assert_eq!(rgb_effects_wire(true, false, false), RgbWire::RgbEffects);
    }

    #[test]
    fn mouse_streams_per_led_only_once_leds_are_discovered() {
        // Discovered PK LED IDs are real firmware IDs — safe to stream per-LED.
        assert_eq!(rgb_effects_wire(false, true, true), RgbWire::PerKey);
        // Not discovered: LEDs carry synthetic IDs, so stay whole-zone.
        assert_eq!(rgb_effects_wire(false, true, false), RgbWire::RgbEffects);
    }

    #[test]
    fn no_per_key_feature_always_stays_rgb_effects() {
        assert_eq!(rgb_effects_wire(false, false, true), RgbWire::RgbEffects);
        assert_eq!(rgb_effects_wire(true, false, true), RgbWire::RgbEffects);
    }
}
