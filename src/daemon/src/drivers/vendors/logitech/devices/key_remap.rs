//! Key remapping for `LogitechDevice` — the `KeyRemapCapability` impl, the
//! notification-path button-event handlers, and the reconnect/online state
//! machinery shared with the receiver.

use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::drivers::vendors::logitech::devices::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::onboard::reconcile_onboard_profile;
use crate::drivers::vendors::logitech::devices::state::{is_host_mode, LogitechDeviceState};
use crate::drivers::vendors::logitech::protocols::hidpp::{
    feature,
    controls::{button_bitmap_to_cids, encode_set_cid_reporting, parse_diverted_buttons_event},
    dpi::encode_set_dpi,
};
use crate::drivers::{Device, KeyRemapCapability};
use halod_protocol::types::{ButtonAction, ButtonMapping, KeyRemapStatus};

impl LogitechDevice {
    /// Handle an unsolicited HID++ 2.0 feature notification. Returns true if
    /// device state changed and callers should broadcast the new state.
    pub async fn handle_feature_notification(&self, sub_id: u8, address: u8, data: &[u8]) -> bool {
        if let Some(app) = self.app_ref.get().and_then(|w| w.upgrade()) {
            if dispatch_button_notification(sub_id, address, data, &self.state, &app, &self.id).await {
                return false; // forwarded to KeyRemapEngine; no state broadcast
            }
        }

        let (op_idx, sector_size) = {
            let state = self.state.lock().await;
            (
                state.features.get(&feature::ONBOARD_PROFILES).copied(),
                state.profile.profile_sector_size,
            )
        };
        let Some(op) = op_idx else { return false };
        if sub_id != op {
            return false;
        }

        let mut changed = false;
        if sector_size >= 16 {
            let (msg, devnum) = self.transport_snapshot().await;
            if reconcile_onboard_profile(&msg, devnum, op, sector_size, &self.state).await {
                log::trace!("[{}] active onboard profile changed", self.id);
                changed = true;
            }
        }
        if address == 0x10 {
            if let Some((idx, dpi)) = {
                let mut st = self.state.lock().await;
                let profile_steps = st.profile.profile_steps.clone();
                st.dpi.apply_dpi_step_event(&profile_steps, data)
            } {
                log::trace!("[{}] DPI step changed: idx={idx} dpi={dpi}", self.id);
                changed = true;
            }
        }
        changed
    }

    pub async fn set_online(&self, online: bool) -> bool {
        let (changed, held) = {
            let mut state = self.state.lock().await;
            let changed = state.online != online;
            state.online = online;
            // A sleeping device loses its LED state; drop the diff cache so the next
            // streamed frame after wake is sent in full.
            if !online {
                state.rgb.pk_frame_cache.clear();
            }
            // Flush any diverted buttons still held at disconnect so the key remap
            // engine can release them (e.g. prevents Layer Shift staying active forever).
            let held = if !online {
                std::mem::take(&mut state.remap.prev_diverted_cids)
            } else {
                vec![]
            };
            (changed, held)
        };
        if !held.is_empty() {
            if let Some(app) = self.app_ref.get().and_then(|w| w.upgrade()) {
                let _ = app.button_event_tx.send(crate::state::ButtonEvent {
                    device_id: self.id.clone(),
                    pressed: vec![],
                    released: held,
                });
            }
        }
        changed
    }

    /// Called when a wireless device comes back online. Refreshes hardware state
    /// (features, DPI, battery) and re-applies the last user-set RGB to the device.
    pub async fn reinitialize_and_reapply(&self, app: Arc<crate::state::AppState>) {
        let saved_rgb = self.state.lock().await.rgb.rgb_state.clone();
        // The device lost its LED state while offline — the first frame after
        // reconnect must be sent in full.
        self.state.lock().await.rgb.pk_frame_cache.clear();

        // A wireless device that just powered on does not always answer HID++
        // feature queries on the first attempt — the radio link needs a moment
        // to settle after the receiver's 0x41 connection notification. Retry a
        // few times before giving up; a single failed attempt used to leave the
        // device stuck "connected" with an empty capability set, which the UI
        // shows as "no controls available for this device".
        let mut ready = false;
        for attempt in 1..=6u8 {
            // Another notification may have taken the device offline again
            // mid-retry — stop chasing a device that is no longer there.
            if !self.state.lock().await.online {
                log::debug!("[{}] Reconnect aborted — device went offline again", self.id);
                return;
            }
            match self.initialize().await {
                Ok(true) => {
                    ready = true;
                    break;
                }
                Ok(false) => log::debug!(
                    "[{}] Not ready yet after reconnect (attempt {attempt}/6)",
                    self.id
                ),
                Err(e) => log::warn!("[{}] Reconnect reinit error: {e}", self.id),
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
        if !ready {
            crate::notify::warn(
                &app,
                "Device did not reconnect",
                format!("{} did not become ready after 6 reconnect attempts.", self.id),
            )
            .await;
            return;
        }

        // Re-apply persisted settings (host mode, button mappings, …). initialize()
        // re-reads hardware state and may see a firmware reset; load_state restores intent.
        {
            let saved_state = {
                let cfg = app.config.read().await;
                cfg.active_profile_data().device_states.get(&self.id).cloned()
            };
            if let Some(state) = saved_state {
                self.load_state(&state).await;
            }
        }

        if let Some(rgb_state) = saved_rgb {
            // `initialize()` resets the cached RGB state to the device default
            // (blue static); put the user's pre-reconnect state back so the
            // shared restore path re-applies it instead of the default.
            self.state.lock().await.rgb.rgb_state = Some(rgb_state);
            self.restore_rgb_control().await;
        }

        // Re-apply button diversions — device loses them on power-cycle.
        self.apply_reprog_mappings().await;

        // Restore software DPI if we had one.
        {
            let (dpi_idx, dpi, online) = {
                let st = self.state.lock().await;
                let dpi = if st.dpi.software_dpi_index < st.dpi.software_dpi_steps.len() {
                    st.dpi.software_dpi_steps.get(st.dpi.software_dpi_index).copied()
                } else {
                    None
                };
                (st.features.get(&feature::ADJUSTABLE_DPI).copied(), dpi, st.online)
            };
            if online {
                if let (Some(idx), Some(d)) = (dpi_idx, dpi) {
                    let (msg, devnum) = self.transport_snapshot().await;
                    let _ = msg.feature_request(devnum, idx, 0x30, &encode_set_dpi(d)).await;
                }
            }
        }

        crate::ipc::broadcast_state(app).await;
    }

    /// Three backends, depending on which feature the device carries:
    /// - `REPROG_CONTROLS_V4` — per-control divert via setCidReporting (func 0x30).
    /// - `GKEY` — global software-control toggle (func 0x20); enabled whenever
    ///   any G-key has a non-Native mapping.
    /// - `MOUSE_BUTTON_SPY` — global spy (func 0x10) for press events, plus a
    ///   per-button divert (func 0x40) that suppresses the native action of
    ///   each remapped button. The action itself is injected host-side, so a
    ///   diverted-but-unhandled button is simply suppressed (which is what
    ///   happens when the host software is not running).
    pub(super) async fn apply_reprog_mappings(&self) {
        let is_native = |m: &ButtonMapping| {
            matches!((&m.base, &m.shifted), (ButtonAction::Native, ButtonAction::Native))
        };

        let (rc_idx, gkey_idx, spy_idx, mappings) = {
            let state = self.state.lock().await;
            (
                state.features.get(&feature::REPROG_CONTROLS_V4).copied(),
                state.features.get(&feature::GKEY).copied(),
                state.features.get(&feature::MOUSE_BUTTON_SPY).copied(),
                state.remap.button_mappings.clone(),
            )
        };
        let any_mapped = mappings.iter().any(|m| !is_native(m));

        // GKEY backend — single global software-control toggle.
        if let Some(idx) = gkey_idx {
            let (msg, devnum) = self.transport_snapshot().await;
            // GKEY func 0x20 = enableSoftwareControl(enabled).
            match msg.feature_request(devnum, idx, 0x20, &[u8::from(any_mapped)]).await {
                Ok(_) => log::debug!(
                    "[{}] GKEY software control {}",
                    self.id, if any_mapped { "enabled" } else { "disabled" }
                ),
                Err(e) => log::warn!("[{}] GKEY enableSoftwareControl failed: {e}", self.id),
            }
            return;
        }

        // MOUSE_BUTTON_SPY backend — global spy for host-side press events.
        //
        // Per-button divert (func 0x40) is intentionally NOT sent: its
        // button-id byte is not the bitmap bit index (G HUB addresses
        // right-click — cid 10 — as id 5), so addressing it by bit index
        // diverted the wrong buttons, and un-mapping never restored them.
        // Until that id encoding is reverse-engineered, a remapped button also
        // fires its native action (double-fire). See docs/protocols/hidpp.md.
        if let Some(idx) = spy_idx {
            let (msg, devnum) = self.transport_snapshot().await;
            // func 0x10 = setSpyState(enabled) — host-side press reporting.
            match msg.feature_request(devnum, idx, 0x10, &[u8::from(any_mapped)]).await {
                Ok(_) => log::debug!(
                    "[{}] MOUSE_BUTTON_SPY spy {}",
                    self.id, if any_mapped { "on" } else { "off" }
                ),
                Err(e) => log::warn!("[{}] MOUSE_BUTTON_SPY setSpyState failed: {e}", self.id),
            }
            return;
        }

        // REPROG_CONTROLS_V4 backend.
        let Some(rc_idx) = rc_idx else { return };
        let cids = {
            let state = self.state.lock().await;
            if state.profile.onboard_mode.map_or(false, |m| !is_host_mode(m)) { return }
            state.remap.reprog_cids.clone()
        };
        let (msg, devnum) = self.transport_snapshot().await;
        for desc in &cids {
            if !desc.divertable { continue }
            let diverted = mappings.iter().any(|m| m.cid == desc.cid && !is_native(m));
            // setCidReporting (func 0x30): [cid_hi, cid_lo, flags, 0, remap_hi, remap_lo, 0, ...]
            // flags bit 0 = divert
            if let Err(e) = msg
                .feature_request(devnum, rc_idx, 0x30, &encode_set_cid_reporting(desc.cid, diverted))
                .await
            {
                log::warn!("[{}] setCidReporting cid={:#06x} divert={diverted} failed: {e}", self.id, desc.cid);
            }
        }
        log::debug!("[{}] reprog mappings applied ({} CIDs checked)", self.id, cids.len());
    }
}

// ── Button notification dispatch ─────────────────────────────────────────────

/// Decode and dispatch a button press/release notification from
/// REPROG_CONTROLS_V4, GKEY, or MOUSE_BUTTON_SPY. Returns true when the
/// notification was a button event; callers should skip further processing.
///
/// Used by both the wireless receiver path (`handle_feature_notification`) and
/// the wired DPI watcher (`start_dpi_watcher`).
pub(super) async fn dispatch_button_notification(
    sub_id: u8,
    address: u8,
    data: &[u8],
    state: &Arc<Mutex<LogitechDeviceState>>,
    app: &Arc<crate::state::AppState>,
    id: &str,
) -> bool {
    if address != 0x00 {
        return false;
    }
    let (rc_idx, gkey_idx, spy_idx) = {
        let st = state.lock().await;
        (
            st.features.get(&feature::REPROG_CONTROLS_V4).copied(),
            st.features.get(&feature::GKEY).copied(),
            st.features.get(&feature::MOUSE_BUTTON_SPY).copied(),
        )
    };
    let current = if Some(sub_id) == rc_idx {
        parse_diverted_buttons_event(data)
    } else if Some(sub_id) == gkey_idx || Some(sub_id) == spy_idx {
        let bitmap = u16::from_le_bytes([
            data.first().copied().unwrap_or(0),
            data.get(1).copied().unwrap_or(0),
        ]);
        button_bitmap_to_cids(bitmap)
    } else {
        return false;
    };

    let (pressed, released) = {
        let mut st = state.lock().await;
        let prev = st.remap.prev_diverted_cids.clone();
        let pressed: Vec<u16> = current.iter().filter(|c| !prev.contains(c)).copied().collect();
        let released: Vec<u16> = prev.iter().filter(|c| !current.contains(c)).copied().collect();
        st.remap.prev_diverted_cids = current;
        (pressed, released)
    };
    if !pressed.is_empty() || !released.is_empty() {
        log::trace!("[{id}] button event pressed={:?} released={:?}", pressed, released);
        let _ = app.button_event_tx.send(crate::state::ButtonEvent {
            device_id: id.to_string(),
            pressed,
            released,
        });
    }
    true
}

// ── KeyRemapCapability ────────────────────────────────────────────────────────

#[async_trait]
impl KeyRemapCapability for LogitechDevice {
    async fn get_key_remap_status(&self) -> KeyRemapStatus {
        let state = self.state.lock().await;
        KeyRemapStatus {
            buttons: state.remap.reprog_cids.clone(),
            mappings: state.remap.button_mappings.clone(),
            requires_host_mode: state.key_remap_requires_host_mode(),
            host_mode_active: state.profile.onboard_mode.map(is_host_mode).unwrap_or(false),
        }
    }

    async fn set_button_mapping(&self, mapping: ButtonMapping) -> Result<()> {
        log::info!(
            "[{}] set_button_mapping cid={} base={:?} shifted={:?}",
            self.id, mapping.cid, mapping.base, mapping.shifted
        );
        {
            let state = self.state.lock().await;
            if state.key_remap_requires_host_mode()
                && !state.profile.onboard_mode.map(is_host_mode).unwrap_or(false)
            {
                bail!("key remapping requires host mode");
            }
        }
        {
            let mut state = self.state.lock().await;
            if let Some(pos) = state.remap.button_mappings.iter().position(|m| m.cid == mapping.cid) {
                if matches!((&mapping.base, &mapping.shifted), (ButtonAction::Native, ButtonAction::Native)) {
                    state.remap.button_mappings.remove(pos);
                } else {
                    state.remap.button_mappings[pos] = mapping;
                }
            } else if !matches!((&mapping.base, &mapping.shifted), (ButtonAction::Native, ButtonAction::Native)) {
                state.remap.button_mappings.push(mapping);
            }
        }
        let stored = self.state.lock().await.remap.button_mappings.len();
        log::info!("[{}] set_button_mapping stored — {stored} mapping(s) total", self.id);
        self.apply_reprog_mappings().await;
        Ok(())
    }

    async fn reset_button_mapping(&self, cid: u16) -> Result<()> {
        self.state.lock().await.remap.button_mappings.retain(|m| m.cid != cid);
        self.apply_reprog_mappings().await;
        Ok(())
    }

    async fn reset_all_button_mappings(&self) -> Result<()> {
        self.state.lock().await.remap.button_mappings.clear();
        self.apply_reprog_mappings().await;
        Ok(())
    }

    fn state_key(&self) -> &'static str { "keyremap" }

    fn save_state(&self) -> serde_json::Value {
        // try_lock: button_mappings are only written on user-initiated calls so
        // contention here is negligible.
        match self.state.try_lock() {
            Ok(st) => {
                if st.remap.button_mappings.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::to_value(&st.remap.button_mappings).unwrap_or(serde_json::Value::Null)
                }
            }
            Err(_) => serde_json::Value::Null,
        }
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        if let Ok(mappings) = serde_json::from_value::<Vec<ButtonMapping>>(v.clone()) {
            {
                let mut st = self.state.lock().await;
                st.remap.button_mappings = mappings;
            }
            // Push the restored mappings to the device.
            self.apply_reprog_mappings().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::vendors::logitech::devices::state::LogitechDeviceState;
    use crate::state::AppState;
    use std::collections::HashMap;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    fn make_state_with_feature(feat_key: u16, feat_idx: u8) -> Arc<Mutex<LogitechDeviceState>> {
        let mut features = HashMap::new();
        features.insert(feat_key, feat_idx);
        Arc::new(Mutex::new(LogitechDeviceState { features, ..LogitechDeviceState::default() }))
    }

    // ── dispatch_button_notification ─────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_reprog_button_press() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        let app = make_app();
        let mut btn_rx = app.button_event_tx.subscribe();

        // Two big-endian CID entries: 0x00D7 and 0x00C9 held
        let handled = dispatch_button_notification(
            0x09, 0x00, &[0x00, 0xD7, 0x00, 0xC9, 0x00, 0x00, 0x00, 0x00],
            &state, &app, "d",
        ).await;

        assert!(handled);
        let evt = btn_rx.try_recv().unwrap();
        assert!(evt.pressed.contains(&0x00D7));
        assert!(evt.pressed.contains(&0x00C9));
        assert!(evt.released.is_empty());
    }

    #[tokio::test]
    async fn dispatch_reprog_button_release() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        state.lock().await.remap.prev_diverted_cids = vec![0x00D7];
        let app = make_app();
        let mut btn_rx = app.button_event_tx.subscribe();

        let handled = dispatch_button_notification(
            0x09, 0x00, &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state, &app, "d",
        ).await;

        assert!(handled);
        let evt = btn_rx.try_recv().unwrap();
        assert!(evt.pressed.is_empty());
        assert!(evt.released.contains(&0x00D7));
    }

    #[tokio::test]
    async fn dispatch_mouse_button_spy_bitmap() {
        let state = make_state_with_feature(feature::MOUSE_BUTTON_SPY, 0x0a);
        let app = make_app();
        let mut btn_rx = app.button_event_tx.subscribe();

        // Bitmap 0x0005 = bits 0 and 2 set → synthetic CIDs 1 and 3
        let handled = dispatch_button_notification(
            0x0a, 0x00, &[0x05, 0x00, 0x00, 0x00],
            &state, &app, "d",
        ).await;

        assert!(handled);
        let evt = btn_rx.try_recv().unwrap();
        assert!(evt.pressed.contains(&1));
        assert!(evt.pressed.contains(&3));
    }

    #[tokio::test]
    async fn dispatch_returns_false_for_wrong_address() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        let app = make_app();
        let mut btn_rx = app.button_event_tx.subscribe();

        let handled = dispatch_button_notification(
            0x09, 0x10, &[0x00, 0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state, &app, "d",
        ).await;

        assert!(!handled);
        assert!(btn_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_returns_false_for_unknown_sub_id() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        let app = make_app();
        let mut btn_rx = app.button_event_tx.subscribe();

        let handled = dispatch_button_notification(
            0x0f, 0x00, &[0x00, 0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state, &app, "d",
        ).await;

        assert!(!handled);
        assert!(btn_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_no_event_when_state_unchanged() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        state.lock().await.remap.prev_diverted_cids = vec![0x00D7];
        let app = make_app();
        let mut btn_rx = app.button_event_tx.subscribe();

        // Same CID as prev — no change, no event
        let handled = dispatch_button_notification(
            0x09, 0x00, &[0x00, 0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state, &app, "d",
        ).await;

        assert!(handled);
        assert!(btn_rx.try_recv().is_err());
    }

    // ── Key remapper unit tests ───────────────────────────────────────────────

    fn compute_deltas(prev: &[u16], current: &[u16]) -> (Vec<u16>, Vec<u16>) {
        let pressed: Vec<u16> = current.iter().filter(|cid| !prev.contains(cid)).copied().collect();
        let released: Vec<u16> = prev.iter().filter(|cid| !current.contains(cid)).copied().collect();
        (pressed, released)
    }

    #[test]
    fn reprog_event_delta_press() {
        let prev = vec![];
        let current = vec![0x0050, 0x0051];
        let (pressed, released) = compute_deltas(&prev, &current);
        assert_eq!(pressed, vec![0x0050, 0x0051]);
        assert!(released.is_empty());
    }

    #[test]
    fn reprog_event_delta_release() {
        let prev = vec![0x0050, 0x0051];
        let current = vec![0x0051];
        let (pressed, released) = compute_deltas(&prev, &current);
        assert!(pressed.is_empty());
        assert_eq!(released, vec![0x0050]);
    }

    #[test]
    fn reprog_event_delta_swap() {
        let prev = vec![0x0050];
        let current = vec![0x0051];
        let (pressed, released) = compute_deltas(&prev, &current);
        assert_eq!(pressed, vec![0x0051]);
        assert_eq!(released, vec![0x0050]);
    }

    #[test]
    fn reprog_event_delta_no_change() {
        let prev = vec![0x0050];
        let current = vec![0x0050];
        let (pressed, released) = compute_deltas(&prev, &current);
        assert!(pressed.is_empty());
        assert!(released.is_empty());
    }

    #[test]
    fn button_mapping_serde_roundtrip() {
        use halod_protocol::types::{ButtonAction, ButtonMapping, CycleDir, MouseBtn};
        let m = ButtonMapping {
            cid: 0x0051,
            base: ButtonAction::MouseButton { btn: MouseBtn::Middle },
            shifted: ButtonAction::DpiCycle { direction: CycleDir::Up },
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ButtonMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cid, m.cid);
        assert_eq!(back.base, m.base);
        assert_eq!(back.shifted, m.shifted);
    }

    // ── apply_reprog_mappings guard tests ─────────────────────────────────────
    //
    // Only Some(0x02) host-mode should allow setCidReporting; Some(0x01) onboard
    // blocks it; None (no ONBOARD_PROFILES feature) has no mode restriction.

    fn reprog_guard_should_skip(onboard_mode: Option<u8>) -> bool {
        // Mirrors the guard at apply_reprog_mappings: return early only when
        // onboard_mode is known-and-not-host-mode.
        onboard_mode.map_or(false, |m| m != 0x02)
    }

    #[test]
    fn reprog_guard_none_proceeds() {
        // None = device has no ONBOARD_PROFILES feature; no mode restriction needed.
        assert!(!reprog_guard_should_skip(None));
    }

    #[test]
    fn reprog_guard_host_mode_proceeds() {
        // 0x02 = host mode — setCidReporting is allowed.
        assert!(!reprog_guard_should_skip(Some(0x02)));
    }

    #[test]
    fn reprog_guard_onboard_mode_skips() {
        // 0x01 = onboard mode — diverting buttons must be deferred until host mode.
        assert!(reprog_guard_should_skip(Some(0x01)));
    }

    #[test]
    fn reprog_guard_unknown_mode_skips() {
        // Any unrecognised non-0x02 value is treated conservatively as "not host".
        assert!(reprog_guard_should_skip(Some(0x00)));
        assert!(reprog_guard_should_skip(Some(0x03)));
    }
}
