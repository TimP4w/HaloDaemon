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
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::rgb::find_native_effect;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::Hidpp20;
use crate::drivers::{RgbCapability, RgbStateSlot};
use halod_shared::types::{
    DeviceCapability, EffectParamValue, KeyboardLayout, NativeEffect, RgbColor, RgbDescriptor,
    RgbState, RgbZone, ZoneTopology,
};
use halod_shared::zone_transform::build_permutation;

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
        let (zone_count, static_slots, use_pk) = {
            let state = self.state.lock().await;
            (
                state.rgb.rgb_zones.len(),
                state.rgb.rgb_static_slots.clone(),
                state.rgb.rgb_use_pk_lighting,
            )
        };
        let hidpp = self.hidpp2().await;

        log::debug!(
            "[{}] rgb_set_static use_pk={use_pk} zones={zone_count} color={:02x}{:02x}{:02x}",
            self.id,
            color.r,
            color.g,
            color.b
        );

        if use_pk {
            hidpp.per_key_set_all(color).await.map_err(|e| {
                log::warn!("[{}] PER_KEY set-all failed: {e}", self.id);
                e
            })?;
        } else {
            let mut last_err = None;
            let mut ok_count = 0u8;
            for z in 0..zone_count as u8 {
                let slot = static_slots.get(z as usize).copied().unwrap_or(0);
                match hidpp.rgb_set_static_effect(z, slot, color).await {
                    Ok(()) => ok_count += 1,
                    Err(e) => {
                        log::warn!(
                            "[{}] RGB SetEffect zone={z} slot={slot} failed: {e}",
                            self.id
                        );
                        last_err = Some(e);
                    }
                }
            }
            // If every zone failed, surface the error; a partial success is
            // still reported as success (best effort).
            if ok_count == 0 {
                if let Some(e) = last_err {
                    return Err(e.context("RGB SetEffect failed for all zones"));
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

    /// Whole-zone RGB_EFFECTS fallback for a per-LED write: paint each zone with
    /// its first LED's colour via `SetEffect`. Used when the device has no
    /// per-key feature, or a mouse reports no per-key LED IDs.
    async fn rgb_set_per_led_via_effects(
        &self,
        hidpp: &Hidpp20,
        zones: &HashMap<String, HashMap<String, RgbColor>>,
        zone_map: &HashMap<String, usize>,
    ) {
        let static_slots = {
            let st = self.state.lock().await;
            st.rgb.rgb_static_slots.clone()
        };
        for (zone_id, led_colors) in zones {
            let Some(&zone_idx) = zone_map.get(zone_id) else {
                continue;
            };
            if let Some(&color) = led_colors.values().next() {
                let slot = static_slots.get(zone_idx).copied().unwrap_or(0);
                if let Err(e) = hidpp
                    .rgb_set_static_effect(zone_idx as u8, slot, color)
                    .await
                {
                    log::warn!("[{}] RGB_EFFECTS zone={zone_idx} failed: {e}", self.id);
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
        let state = self.state.lock().await;
        let has_pk = state.features.contains_key(&feature::PER_KEY_LIGHTING_V2);
        let has_rgb = state.features.contains_key(&feature::RGB_EFFECTS);
        let zone_map: HashMap<String, usize> = state
            .rgb
            .rgb_zones
            .iter()
            .enumerate()
            .map(|(i, z)| (z.id.clone(), i))
            .collect();
        drop(state);

        if has_pk {
            if self.is_keyboard() {
                // Keyboard: stream every key via PER_KEY setIndividual + commit.
                let pairs = collect_pairs(zones, &zone_map, None);
                if let Err(e) = hidpp.write_per_key_pairs(&pairs).await {
                    log::warn!("[{}] PER_KEY_LIGHTING burst failed: {e}", self.id);
                }
            } else {
                // Mouse: only the firmware-reported per-key LED ids.
                let pk_led_ids = {
                    let st = self.state.lock().await;
                    st.rgb.pk_led_ids.clone()
                };
                if !pk_led_ids.is_empty() {
                    let pairs = collect_pairs(zones, &zone_map, Some(&pk_led_ids));
                    if let Err(e) = hidpp.write_per_key_pairs(&pairs).await {
                        log::warn!("[{}] PER_KEY_LIGHTING burst failed: {e}", self.id);
                    }
                } else if has_rgb {
                    log::info!(
                        "[{}] rgb_set_per_led: mouse has no pk_led_ids, whole-zone RGB_EFFECTS fallback",
                        self.id
                    );
                    self.rgb_set_per_led_via_effects(&hidpp, zones, &zone_map)
                        .await;
                }
            }
        } else if has_rgb {
            self.rgb_set_per_led_via_effects(&hidpp, zones, &zone_map)
                .await;
        } else {
            log::warn!(
                "[{}] rgb_set_per_led: no per-key or RGB_EFFECTS feature available",
                self.id
            );
        }
        Ok(())
    }

    /// Canonical "restore RGB control" sequence. Several operations
    /// (profile-sector flash, report-rate change, host-mode switch, wireless
    /// reconnect) cause the firmware to reclaim LED control. After any of them,
    /// re-enable software LED control and re-apply the last cached RGB state.
    pub(super) async fn restore_rgb_control(&self) {
        self.hidpp2().await.rgb_enable_sw_control().await;
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
        state.rgb.rgb_use_pk_lighting = true;
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
        let low_ids = hidpp.read_pk_led_ids().await;
        log::debug!("[{}] PK bitmap low_ids: {:?}", self.id, low_ids);

        if low_ids.len() > 1 && zones.len() <= 1 {
            let leds = super::led_positions::led_strip_from_ids(&low_ids);
            if let Some(zone) = zones.get_mut(0) {
                zone.leds = leds;
            }
            state.rgb.pk_led_ids = low_ids;
        }
    }

    pub(super) async fn init_rgb(
        &self,
        features: &HashMap<u16, u8>,
        keyboard_layout: &KeyboardLayout,
        state: &mut LogitechDeviceState,
    ) {
        let has_rgb = features.contains_key(&feature::RGB_EFFECTS);
        let pk_idx = features.get(&feature::PER_KEY_LIGHTING_V2).copied();

        // No RGB_EFFECTS — try PER_KEY_LIGHTING fallback or give up.
        if !has_rgb {
            if let Some(pk) = pk_idx {
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
        state.rgb.rgb_use_pk_lighting = false;
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
            self.hidpp2().await.rgb_enable_sw_control().await;
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
        let (zone_idx, has_pk, has_rgb, leds) = {
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
                state.features.contains_key(&feature::PER_KEY_LIGHTING_V2),
                state.features.contains_key(&feature::RGB_EFFECTS),
                leds,
            )
        };

        let hidpp = self.hidpp2().await;

        // Read is_wired and coordinator together from one lock acquisition.
        let (is_wired, coordinator) = {
            let t = self.transport.lock().await;
            (t.is_wired, t.coordinator.clone())
        };

        if has_pk {
            let keys: Vec<(u8, RgbColor)> = leds
                .iter()
                .zip(colors.iter())
                .map(|(lp, &c)| ((lp.id & 0xFF) as u8, c))
                .collect();

            // Diff against the last streamed frame and run-length-encode it.
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

            // Wired devices bypass the per-receiver coordinator (their own HID
            // path) and write directly. Wireless devices post through the
            // coordinator so all devices on the same receiver flush together.
            if !is_wired {
                if let Some(coord) = &coordinator {
                    coord.post(hidpp.devnum(), packets).await;
                    return Ok(());
                }
            }
            if let Err(e) = hidpp.send_packets(packets).await {
                log::warn!("[{}] write_frame: send_packets failed: {e}", self.id);
            }
        } else if has_rgb {
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
        Ok(())
    }
}
