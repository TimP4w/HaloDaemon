//! RGB lighting for `LogitechDevice` — the `RgbCapability` implementation and
//! its static / native-effect / per-LED write helpers.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;

use crate::drivers::vendors::logitech::devices::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::pk_frame;
use crate::drivers::vendors::logitech::protocols::hidpp::{
    feature,
    rgb_effects::{
        encode_per_key_batch, encode_per_key_commit, encode_per_key_set_range,
        encode_set_effect_static, find_native_effect,
    },
};
use crate::drivers::{RgbCapability, RgbStateSlot};
use halod_protocol::types::{
    EffectParamValue, RgbColor, RgbDescriptor, RgbState,
};
use halod_protocol::zone_transform::build_permutation;

impl LogitechDevice {
    // ── RGB write helpers ─────────────────────────────────────────────────────

    pub(super) async fn rgb_set_static(&self, color: RgbColor) -> Result<()> {
        let (rgb_idx, pk_idx, zone_count, static_slots, use_pk) = {
            let state = self.state.lock().await;
            let rgb = state.features.get(&feature::RGB_EFFECTS).copied();
            let pk = state.features.get(&feature::PER_KEY_LIGHTING_V2).copied();
            (rgb, pk, state.rgb.rgb_zones.len(), state.rgb.rgb_static_slots.clone(), state.rgb.rgb_use_pk_lighting)
        };
        let (msg, devnum) = self.transport_snapshot().await;

        log::debug!("[{}] rgb_set_static use_pk={use_pk} zones={zone_count} color={:02x}{:02x}{:02x}", self.id, color.r, color.g, color.b);

        if use_pk {
            let idx = pk_idx.ok_or_else(|| anyhow::anyhow!("No PER_KEY_LIGHTING feature"))?;
            // SET_RANGE (func=0x50): [start_key=0x00, end_key=0xFF, r, g, b]
            msg.feature_request(devnum, idx, 0x50, &encode_per_key_set_range(color)).await
                .map_err(|e| { log::warn!("[{}] PER_KEY SET_RANGE failed: {e}", self.id); e })?;
            // COMMIT (func=0x70)
            msg.feature_request(devnum, idx, 0x70, &[0x00]).await
                .map_err(|e| { log::warn!("[{}] PER_KEY COMMIT failed: {e}", self.id); e })?;
        } else {
            let idx = rgb_idx.ok_or_else(|| anyhow::anyhow!("No RGB_EFFECTS feature"))?;
            let mut last_err = None;
            let mut ok_count = 0u8;
            for z in 0..zone_count as u8 {
                let slot = static_slots.get(z as usize).copied().unwrap_or(0);
                // SET_EFFECT (func=0x10): [zone_idx, slot_idx, r, g, b, period_hi, period_lo, ...]
                let params = encode_set_effect_static(z, slot, color);
                match msg.feature_request(devnum, idx, 0x10, &params).await {
                    Ok(r) => {
                        log::debug!("[{}] RGB SetEffect zone={z} slot={slot} ok: {:02x?}", self.id, r);
                        ok_count += 1;
                    }
                    Err(e) => {
                        log::warn!("[{}] RGB SetEffect zone={z} slot={slot} failed: {e}", self.id);
                        last_err = Some(e);
                    }
                }
            }
            // If every zone failed, surface the error so the UI shows a toast.
            // A partial success is still reported as success (best effort).
            if ok_count == 0 {
                if let Some(e) = last_err {
                    return Err(e.context("RGB SetEffect failed for all zones"));
                }
            }
        }
        Ok(())
    }

    /// Apply a native firmware effect via the RGB_EFFECTS `SetEffect` (func
    /// 0x10) call. The byte block is the effect's `base` from `NATIVE_EFFECTS`
    /// with the user's `values` overlaid.
    pub(super) async fn rgb_set_native_effect(
        &self,
        id: &str,
        values: &HashMap<String, EffectParamValue>,
    ) -> Result<()> {
        let rgb_idx = {
            let state = self.state.lock().await;
            state
                .features
                .get(&feature::RGB_EFFECTS)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("No RGB feature"))?
        };
        let block = find_native_effect(id)
            .map(|e| e.encode(values))
            .ok_or_else(|| anyhow::anyhow!("Unknown native effect: {id}"))?;
        // SetEffect (func 0x10); block[0] = 0xFF addresses all zones at once.
        // The device's error reply (e.g. code 0x05) must surface, not be
        // swallowed — otherwise a rejected effect looks like a silent no-op.
        let (msg, devnum) = self.transport_snapshot().await;
        msg.feature_request(devnum, rgb_idx, 0x10, &block)
            .await
            .map_err(|e| {
                log::warn!("[{}] RGB SetEffect ({id}) failed: {e}", self.id);
                e.context(format!("native effect '{id}' rejected by device"))
            })?;
        Ok(())
    }

    /// Apply each zone's LED-content transform to a per-LED colour map.
    /// Physical LED `zone.leds[i]` takes the colour assigned to `zone.leds[perm[i]]`.
    fn transform_perled_map(
        &self,
        zones_map: &HashMap<String, HashMap<String, RgbColor>>,
    ) -> HashMap<String, HashMap<String, RgbColor>> {
        let Some(descriptor) = self.rgb_descriptor.get() else {
            return zones_map.clone();
        };
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
            .collect()
    }

    /// Whole-zone RGB_EFFECTS fallback for a per-LED write: paint each zone
    /// with its first LED's colour via `SetEffect` (func 0x10). Used when the
    /// device has no per-key feature, or a mouse reports no per-key LED IDs.
    async fn rgb_set_per_led_via_effects(
        &self,
        rgb_idx: u8,
        zones: &HashMap<String, HashMap<String, RgbColor>>,
        zone_map: &HashMap<String, usize>,
    ) {
        let (msg, devnum) = self.transport_snapshot().await;
        let static_slots = {
            let st = self.state.lock().await;
            st.rgb.rgb_static_slots.clone()
        };
        for (zone_id, led_colors) in zones {
            let Some(&zone_idx) = zone_map.get(zone_id) else { continue };
            if let Some(&color) = led_colors.values().next() {
                let slot = static_slots.get(zone_idx).copied().unwrap_or(0);
                let params = encode_set_effect_static(zone_idx as u8, slot, color);
                if let Err(e) = msg.feature_request(devnum, rgb_idx, 0x10, &params).await {
                    log::warn!("[{}] RGB_EFFECTS zone={zone_idx} failed: {e}", self.id);
                }
            }
        }
    }

    pub(super) async fn rgb_set_per_led(&self, zones_map: &std::collections::HashMap<String, std::collections::HashMap<String, RgbColor>>) -> Result<()> {
        let transformed = self.transform_perled_map(zones_map);
        let zones = &transformed;
        let (msg, devnum) = self.transport_snapshot().await;
        let state = self.state.lock().await;
        let per_key_idx = state
            .features
            .get(&feature::PER_KEY_LIGHTING_V2)
            .copied();
        let rgb_idx = state
            .features
            .get(&feature::RGB_EFFECTS)
            .copied();
        let zone_map: HashMap<String, usize> = state
            .rgb.rgb_zones
            .iter()
            .enumerate()
            .map(|(i, z)| (z.id.clone(), i))
            .collect();
        drop(state);

        if let Some(pk_idx) = per_key_idx {
            log::debug!("[{}] rgb_set_per_led via PER_KEY_LIGHTING idx={pk_idx}", self.id);
            if self.is_keyboard() {
                // Keyboard: batch 4 key-color pairs per SET_INDIVIDUAL call, then COMMIT.
                // Padding incomplete final batch with the last valid pair avoids zero-keying
                // key 0 (which corrupts the lighting on every individual-key call).
                // All packets are accumulated and sent in one fire-and-forget burst.
                let mut batch: Vec<(u8, u8, u8, u8)> = Vec::new();
                let mut packets: Vec<Vec<u8>> = Vec::new();

                for (zone_id, led_colors) in zones {
                    if !zone_map.contains_key(zone_id) { continue };
                    for (led_id_str, &color) in led_colors {
                        let Ok(led_id) = led_id_str.parse::<u32>() else { continue };
                        let key_id = (led_id & 0xFF) as u8;
                        batch.push((key_id, color.r, color.g, color.b));
                        if batch.len() == 4 {
                            packets.push(encode_per_key_batch(&batch, devnum, pk_idx));
                            batch.clear();
                        }
                    }
                }
                // Flush remaining keys, pad to 4 with last valid pair
                if !batch.is_empty() {
                    let last = *batch.last().unwrap();
                    while batch.len() < 4 { batch.push(last); }
                    packets.push(encode_per_key_batch(&batch, devnum, pk_idx));
                }
                // COMMIT (func=0x70) to apply queued changes
                packets.push(encode_per_key_commit(devnum, pk_idx));
                if let Err(e) = msg.feature_send_many_fire(packets).await {
                    log::warn!("[{}] PER_KEY_LIGHTING burst failed: {e}", self.id);
                }
            } else {
                // Non-keyboard mouse: use PER_KEY_LIGHTING with bitmap-discovered IDs.
                let pk_led_ids = {
                    let st = self.state.lock().await;
                    st.rgb.pk_led_ids.clone()
                };
                if let (Some(pk), true) = (per_key_idx, !pk_led_ids.is_empty()) {
                    log::debug!("[{}] rgb_set_per_led: mouse via PER_KEY_LIGHTING ids={:?}", self.id, pk_led_ids);
                    let mut pairs: Vec<(u8, u8, u8, u8)> = Vec::new();
                    for (zone_id, led_colors) in zones {
                        if !zone_map.contains_key(zone_id) { continue };
                        for (led_id_str, &color) in led_colors {
                            let Ok(led_id) = led_id_str.parse::<u32>() else { continue };
                            let id = (led_id & 0xFF) as u8;
                            if pk_led_ids.contains(&id) {
                                pairs.push((id, color.r, color.g, color.b));
                            }
                        }
                    }
                    let mut packets: Vec<Vec<u8>> = Vec::with_capacity(pairs.chunks(4).count() + 1);
                    for chunk in pairs.chunks(4) {
                        // Pad the (possibly short) final chunk to 4 entries with
                        // the last valid pair, mirroring the keyboard path.
                        let last = *chunk.last().unwrap();
                        let mut batch = chunk.to_vec();
                        while batch.len() < 4 { batch.push(last); }
                        packets.push(encode_per_key_batch(&batch, devnum, pk));
                    }
                    packets.push(encode_per_key_commit(devnum, pk));
                    let _ = msg.feature_send_many_fire(packets).await;
                    return Ok(());
                }

                // No per-key IDs — fall back to setting whole zone via RGB_EFFECTS
                if let Some(r_idx) = rgb_idx {
                    log::info!("[{}] rgb_set_per_led: mouse has no pk_led_ids, whole-zone RGB_EFFECTS fallback", self.id);
                    self.rgb_set_per_led_via_effects(r_idx, zones, &zone_map).await;
                }
                return Ok(());
            }
        } else if let Some(r_idx) = rgb_idx {
            log::debug!("[{}] rgb_set_per_led via RGB_EFFECTS idx={r_idx}", self.id);
            self.rgb_set_per_led_via_effects(r_idx, zones, &zone_map).await;
        } else {
            log::warn!("[{}] rgb_set_per_led: no per-key or RGB_EFFECTS feature available", self.id);
        }
        Ok(())
    }

    /// Canonical "restore RGB control" sequence. Several operations
    /// (profile-sector flash, report-rate change, host-mode switch, wireless
    /// reconnect) cause the firmware to reclaim LED control. After any of them,
    /// re-enable software LED control via the RGB feature's func 0x50
    /// `[0x01, 0x01]` request and re-apply the last cached RGB state.
    ///
    /// The RGB feature index is resolved from the `RGB_EFFECTS` feature. If the
    /// device lacks that feature the SW-control toggle is skipped; the
    /// cached-state re-apply is skipped when no RGB state has been recorded yet.
    pub(super) async fn restore_rgb_control(&self) {
        let (rgb_idx, current_rgb) = {
            let st = self.state.lock().await;
            let idx = st.features.get(&feature::RGB_EFFECTS).copied();
            (idx, st.rgb.rgb_state.clone())
        };
        if let Some(r_idx) = rgb_idx {
            let (msg, devnum) = self.transport_snapshot().await;
            let _ = msg.feature_request(devnum, r_idx, 0x50, &[0x01, 0x01]).await;
        }
        if let Some(rgb_state) = current_rgb {
            let _ = self.apply(rgb_state).await;
        }
    }
}

// ── RgbCapability ─────────────────────────────────────────────────────────────

#[async_trait]
impl RgbCapability for LogitechDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        static EMPTY: std::sync::OnceLock<RgbDescriptor> = std::sync::OnceLock::new();
        self.rgb_descriptor.get().unwrap_or_else(|| {
            EMPTY.get_or_init(|| RgbDescriptor { zones: vec![], native_effects: vec![] })
        })
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb
    }

    fn save_state(&self) -> serde_json::Value {
        let canvas = self.canvas_zones();
        let transforms = self.zone_transforms();
        let mut obj = serde_json::Map::new();
        if let Some(s) = self.current_state() {
            obj.insert("state".into(), serde_json::to_value(s).unwrap_or_default());
        }
        if !canvas.is_empty() {
            obj.insert("canvas_zones".into(), serde_json::to_value(canvas).unwrap_or_default());
        }
        if !transforms.is_empty() {
            obj.insert("rgb_transforms".into(), serde_json::to_value(transforms).unwrap_or_default());
        }
        if obj.is_empty() { serde_json::Value::Null } else { obj.into() }
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        // Restore rgb_state first, before host-mode is set, so restore_rgb_control
        // re-applies the correct profile colour.
        if let Some(s) = v.get("state") {
            if let Ok(state) = serde_json::from_value(s.clone()) {
                // Store the state without pushing to hardware — hardware write
                // happens when host_mode is restored via BooleanCapability.
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
        if !matches!(new_state, RgbState::Engine) {
            self.state.lock().await.rgb.pk_frame_cache.clear();
        }
        let write_result = match &new_state {
            RgbState::Static { color } => self.rgb_set_static(*color).await,
            RgbState::PerLed { zones } => self.rgb_set_per_led(zones).await,
            RgbState::NativeEffect { id, params } => {
                self.rgb_set_native_effect(id, params).await
            }
            RgbState::Engine => Ok(()),
        };
        // Record the requested state even when the hardware write failed: the
        // device is logically in `new_state` now. Leaving it unrecorded traps
        // the canvas engine's mode reconciler (`reconcile_engine_mode`) in a
        // per-tick retry loop — it keeps seeing the device as `Engine` and
        // re-issuing the failing write — and keeps the UI showing the device
        // as still canvas-controlled. The write error is still returned so
        // callers can surface a toast.
        self.state.lock().await.rgb.rgb_state = Some(new_state);
        write_result
    }

    fn current_state(&self) -> Option<RgbState> {
        // Synchronous — use try_lock to avoid blocking; returns None if state is locked
        self.state.try_lock().ok().and_then(|s| s.rgb.rgb_state.clone())
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        let state = self.state.lock().await;
        let zone_idx = state
            .rgb.rgb_zones
            .iter()
            .position(|z| z.id == zone_id)
            .unwrap_or(0) as u8;
        let per_key_idx = state
            .features
            .get(&feature::PER_KEY_LIGHTING_V2)
            .copied();
        let rgb_idx = state.features.get(&feature::RGB_EFFECTS).copied();
        let leds = state
            .rgb.rgb_zones
            .iter()
            .find(|z| z.id == zone_id)
            .map(|z| z.leds.clone())
            .unwrap_or_default();
        drop(state);

        let (msg, devnum) = self.transport_snapshot().await;
        let is_wired = self.transport.lock().await.is_wired;

        if let Some(pk_idx) = per_key_idx {
            let keys: Vec<(u8, RgbColor)> = leds
                .iter()
                .zip(colors.iter())
                .map(|(lp, &c)| ((lp.id & 0xFF) as u8, c))
                .collect();

            // Diff against the last streamed frame and run-length-encode it.
            // An unchanged frame yields no packets, so the bus write is skipped.
            let packets = {
                let mut state = self.state.lock().await;
                let cache = state
                    .rgb.pk_frame_cache
                    .entry(zone_id.to_string())
                    .or_default();
                pk_frame::encode_frame(&keys, cache, devnum, pk_idx)
            };
            if packets.is_empty() {
                return Ok(());
            }

            // Wired devices bypass the per-receiver coordinator (they have their
            // own HID path) and write directly. Wireless devices post through the
            // coordinator so all devices on the same receiver flush together.
            if !is_wired {
                if let Some(coord) = self.pk_coordinator.get() {
                    coord.post(devnum, packets).await;
                    return Ok(());
                }
            }
            let _ = msg.feature_send_many_fire(packets).await;
        } else if let Some(r_idx) = rgb_idx {
            // Use first color as zone static via SET_EFFECT (func=0x10)
            if let Some(&c) = colors.first() {
                let static_slot = {
                    let st = self.state.lock().await;
                    st.rgb.rgb_static_slots.get(zone_idx as usize).copied().unwrap_or(0)
                };
                let params = encode_set_effect_static(zone_idx, static_slot, c);
                let _ = msg
                    .feature_request(devnum, r_idx, 0x10, &params)
                    .await;
            }
        }
        Ok(())
    }
}
