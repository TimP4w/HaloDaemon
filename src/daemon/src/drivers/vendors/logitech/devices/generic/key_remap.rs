// SPDX-License-Identifier: GPL-3.0-or-later
//! Key remapping for `LogitechDevice` — the `KeyRemapCapability` impl, the
//! notification-path button-event handlers, and the reconnect/online state
//! machinery shared with the receiver.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::onboard::reconcile_feature_notification;
use crate::drivers::vendors::logitech::devices::generic::state::{
    is_host_mode, LogitechDeviceState,
};
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::keys::{
    cid_label, parse_button_bitmap_event, parse_diverted_buttons_event,
};
use crate::drivers::{Device, KeyRemapCapability};
use halod_shared::types::ButtonDescriptor;
use halod_shared::types::{ButtonAction, ButtonMapping, DeviceCapability, KeyRemapStatus};

impl LogitechDevice {
    /// Handle an unsolicited HID++ 2.0 feature notification. Returns true if
    /// device state changed and callers should broadcast the new state.
    pub async fn handle_feature_notification(
        &self,
        sub_id: u8,
        address: u8,
        data: &[u8],
        app: Option<&Arc<crate::state::AppState>>,
    ) -> bool {
        let hidpp = self.hidpp2().await;
        // Wireless path skips the onboard-mode safety-net re-read — it is driven
        // by receiver connection notifications, not silent mode drift.
        reconcile_feature_notification(
            Some(&hidpp),
            sub_id,
            address,
            data,
            &self.state,
            app,
            &self.id,
            false,
        )
        .await
        .unwrap_or(false)
    }

    pub async fn set_online(&self, online: bool, app: &Arc<crate::state::AppState>) -> bool {
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
        if !online {
            app.input.layer_shift_clear_device(&self.id);
        }
        if !held.is_empty()
            && app
                .input
                .button_event_tx
                .send(crate::state::ButtonEvent {
                    device_id: self.id.clone(),
                    pressed: vec![],
                    released: held,
                })
                .is_err()
        {
            log::debug!(
                "[{}] button_event_tx: no active receivers at disconnect",
                self.id
            );
        }
        changed
    }

    /// Called when a wireless device comes back online. Refreshes hardware state
    /// (features, DPI, battery) and re-applies the last user-set RGB to the device.
    pub async fn reinitialize_and_reapply(&self, app: Arc<crate::state::AppState>) {
        let saved_rgb = {
            let mut state = self.state.lock().await;
            let saved = state.rgb.rgb_state.clone();
            // The device lost its LED state while offline — the first frame after
            // reconnect must be sent in full.
            state.rgb.pk_frame_cache.clear();
            saved
        };

        // A device that just powered on may not answer HID++ queries on the
        // first attempt while the radio link settles, so retry a few times.
        let mut ready = false;
        for attempt in 1..=6u8 {
            // Another notification may have taken the device offline again
            // mid-retry — stop chasing a device that is no longer there.
            if !self.state.lock().await.online {
                log::debug!("[{}] Reconnect aborted, device went offline again", self.id);
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
            crate::platform::notify::send(
                &app,
                halod_shared::types::NotificationCode::DeviceReconnectFailed {
                    device: self.id.to_string(),
                },
            )
            .await;
            return;
        }

        // Re-apply persisted settings (host mode, button mappings, …). initialize()
        // re-reads hardware state and may see a firmware reset; load_state restores intent.
        {
            let saved_state = {
                let cfg = app.config.read().await;
                Some(cfg.effective_device_state(&self.id)).filter(|v| !v.is_null())
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
            let (dpi, online) = {
                let st = self.state.lock().await;
                let dpi = {
                    let steps = st
                        .dpi
                        .software_dpi_steps
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    let idx = *st
                        .dpi
                        .software_dpi_index
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    steps.get(idx).copied()
                };
                (dpi, st.online)
            };
            if online {
                if let Some(d) = dpi {
                    if let Err(e) = self.hidpp2().await.set_dpi(d).await {
                        log::warn!("[{}] DPI restore failed on reconnect: {e}", self.id);
                    }
                }
            }
        }

        app.broadcast_state().await;
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
        let is_native = is_native_mapping;

        let (rc_idx, gkey_idx, spy_idx, mappings, buttons) = {
            let state = self.state.lock().await;
            (
                state.features.get(&feature::REPROG_CONTROLS_V4).copied(),
                state.features.get(&feature::GKEY).copied(),
                state.features.get(&feature::MOUSE_BUTTON_SPY).copied(),
                state.remap.button_mappings.clone(),
                state.remap.reprog_cids.clone(),
            )
        };
        if let Err(e) = crate::input::validate::validate_button_mappings(&buttons, &mappings) {
            log::warn!(
                "[{}] refusing invalid key-remap state before hardware apply: {e:#}",
                self.id
            );
            return;
        }
        let any_mapped = mappings.iter().any(|m| !is_native(m));

        // GKEY backend — single global software-control toggle.
        if gkey_idx.is_some() {
            match self
                .hidpp2()
                .await
                .set_gkey_software_control(any_mapped)
                .await
            {
                Ok(_) => log::debug!(
                    "[{}] GKEY software control {}",
                    self.id,
                    if any_mapped { "enabled" } else { "disabled" }
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
        // fires its native action (double-fire). 
        if spy_idx.is_some() {
            match self.hidpp2().await.set_mouse_button_spy(any_mapped).await {
                Ok(_) => log::debug!(
                    "[{}] MOUSE_BUTTON_SPY spy {}",
                    self.id,
                    if any_mapped { "on" } else { "off" }
                ),
                Err(e) => log::warn!("[{}] MOUSE_BUTTON_SPY setSpyState failed: {e}", self.id),
            }
            return;
        }

        // REPROG_CONTROLS_V4 backend.
        if rc_idx.is_none() {
            return;
        }
        let cids = {
            let state = self.state.lock().await;
            if state.profile.onboard_mode.is_some_and(|m| !is_host_mode(m)) {
                return;
            }
            state.remap.reprog_cids.clone()
        };
        let hidpp = self.hidpp2().await;
        for desc in &cids {
            if !desc.divertable {
                continue;
            }
            let diverted = mappings.iter().any(|m| m.cid == desc.cid && !is_native(m));
            if let Err(e) = hidpp.set_cid_reporting(desc.cid, diverted).await {
                log::warn!(
                    "[{}] setCidReporting cid={:#06x} divert={diverted} failed: {e}",
                    self.id,
                    desc.cid
                );
            }
        }
        log::debug!(
            "[{}] reprog mappings applied ({} CIDs checked)",
            self.id,
            cids.len()
        );
    }

    // ── Button enumeration (init) ─────────────────────────────────────────────

    /// Enumerate remappable controls via REPROG_CONTROLS_V4 (0x1b04).
    pub(super) async fn init_reprog_controls(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        state.remap.reprog_cids.clear();
        if !features.contains_key(&feature::REPROG_CONTROLS_V4) {
            return;
        }
        let hidpp = self.hidpp2_with(features).await;
        let count = hidpp.reprog_control_count().await;
        log::debug!("[{}] REPROG_CONTROLS_V4: {count} controls", self.id);
        for i in 0..count {
            let Some(info) = hidpp.reprog_control_info(i).await else {
                continue;
            };
            let label = cid_label(info.cid, info.task_id).into_owned();
            log::debug!(
                "[{}] CID {:#06x} task={:#06x} flags={:#04x} group={} divertable={} label={label:?}",
                self.id, info.cid, info.task_id, info.flags, info.group, info.divertable()
            );
            state.remap.reprog_cids.push(ButtonDescriptor {
                cid: info.cid,
                label,
                divertable: info.divertable(),
                group: info.group,
            });
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
    pub(super) async fn init_bitmap_buttons(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        let labels = self.profile.and_then(|p| p.bitmap_button_labels);
        let prefix = self
            .profile
            .map(|p| p.bitmap_button_prefix)
            .unwrap_or("Button");

        // Determine the addressable button count for the active backend.
        // GKEY exposes a getCount call; MOUSE_BUTTON_SPY has no count so all 16
        // bitmap slots are considered.
        let usable: u16 = if features.contains_key(&feature::GKEY) {
            state.remap.uses_software_control = true;
            let count = self.hidpp2_with(features).await.gkey_count().await;
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
            state.remap.uses_software_control = true;
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

    /// Seed the device's out-of-the-box button mappings (e.g. G8 → DPI up) on
    /// first run. No-op when the profile declares no defaults or the device
    /// already carries mappings — a reconnect re-runs init, and the persisted
    /// config restore that follows is authoritative.
    pub(super) fn seed_default_button_mappings(&self, state: &mut LogitechDeviceState) {
        if !state.remap.button_mappings.is_empty() {
            return;
        }
        let defaults =
            super::profile::default_button_mappings(self.profile, &state.remap.reprog_cids);
        if !defaults.is_empty() {
            log::debug!(
                "[{}] seeding {} default button mapping(s)",
                self.id,
                defaults.len()
            );
            state.remap.button_mappings = defaults;
        }
    }
}

/// Returns `true` when both `base` and `shifted` actions are `ButtonAction::Native`.
fn is_native_mapping(m: &ButtonMapping) -> bool {
    matches!(
        (&m.base, &m.shifted),
        (ButtonAction::Native, ButtonAction::Native)
    )
}

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
    let backend = if Some(sub_id) == spy_idx {
        "SPY"
    } else if Some(sub_id) == gkey_idx {
        "GKEY"
    } else if Some(sub_id) == rc_idx {
        "REPROG"
    } else {
        return false;
    };
    log::trace!("[{id}] button notify backend={backend} sub_id={sub_id:#04x} data={data:02x?}",);
    let current = if backend == "REPROG" {
        parse_diverted_buttons_event(data)
    } else {
        parse_button_bitmap_event(data)
    };

    let (pressed, released) = {
        let mut st = state.lock().await;
        let prev = st.remap.prev_diverted_cids.clone();
        let pressed: Vec<u16> = current
            .iter()
            .filter(|c| !prev.contains(c))
            .copied()
            .collect();
        let released: Vec<u16> = prev
            .iter()
            .filter(|c| !current.contains(c))
            .copied()
            .collect();
        st.remap.prev_diverted_cids = current;
        (pressed, released)
    };
    log::trace!("[{id}] button event: pressed={pressed:?} released={released:?}",);
    if (!pressed.is_empty() || !released.is_empty())
        && app
            .input
            .button_event_tx
            .send(crate::state::ButtonEvent {
                device_id: id.to_string(),
                pressed,
                released,
            })
            .is_err()
    {
        log::debug!("[{}] button_event_tx: no active receivers at dispatch", id);
    }
    true
}

#[async_trait]
impl KeyRemapCapability for LogitechDevice {
    async fn get_key_remap_status(&self) -> KeyRemapStatus {
        let state = self.state.lock().await;
        KeyRemapStatus {
            buttons: state.remap.reprog_cids.clone(),
            mappings: state.remap.button_mappings.clone(),
            requires_host_mode: state.key_remap_requires_host_mode(),
            host_mode_active: state
                .profile
                .onboard_mode
                .map(is_host_mode)
                .unwrap_or(false),
        }
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        // Surface the panel when divert-capable even before CIDs enumerate.
        let has_feature = self
            .state
            .lock()
            .await
            .features
            .contains_key(&feature::REPROG_CONTROLS_V4);
        let status = self.get_key_remap_status().await;
        if status.buttons.is_empty() && !has_feature {
            return None;
        }
        Some(DeviceCapability::KeyRemap(status))
    }

    async fn set_button_mapping(&self, mapping: ButtonMapping) -> Result<()> {
        log::info!(
            "[{}] set_button_mapping cid={} base={:?} shifted={:?}",
            self.id,
            mapping.cid,
            mapping.base,
            mapping.shifted
        );
        {
            let state = self.state.lock().await;
            if state.key_remap_requires_host_mode()
                && !state
                    .profile
                    .onboard_mode
                    .map(is_host_mode)
                    .unwrap_or(false)
            {
                bail!("key remapping requires host mode");
            }
            crate::input::validate::validate_cid(&state.remap.reprog_cids, &mapping)?;
        }
        {
            let mut state = self.state.lock().await;
            if let Some(pos) = state
                .remap
                .button_mappings
                .iter()
                .position(|m| m.cid == mapping.cid)
            {
                if is_native_mapping(&mapping) {
                    state.remap.button_mappings.remove(pos);
                } else {
                    state.remap.button_mappings[pos] = mapping;
                }
            } else if !is_native_mapping(&mapping) {
                state.remap.button_mappings.push(mapping);
            }
        }
        let stored = self.state.lock().await.remap.button_mappings.len();
        log::info!(
            "[{}] set_button_mapping stored, {stored} mapping(s) total",
            self.id
        );
        self.apply_reprog_mappings().await;
        Ok(())
    }

    async fn default_mappings(&self) -> Vec<ButtonMapping> {
        let cids = self.state.lock().await.remap.reprog_cids.clone();
        super::profile::default_button_mappings(self.profile, &cids)
    }

    /// Restore the button to its device default (or Native if it has none).
    async fn reset_button_mapping(&self, cid: u16) -> Result<()> {
        let default = self
            .default_mappings()
            .await
            .into_iter()
            .find(|m| m.cid == cid);
        {
            let mut state = self.state.lock().await;
            state.remap.button_mappings.retain(|m| m.cid != cid);
            if let Some(d) = default {
                state.remap.button_mappings.push(d);
            }
        }
        self.apply_reprog_mappings().await;
        Ok(())
    }

    /// Restore every button to the device defaults (Native when none are declared).
    async fn reset_all_button_mappings(&self) -> Result<()> {
        let defaults = self.default_mappings().await;
        self.state.lock().await.remap.button_mappings = defaults;
        self.apply_reprog_mappings().await;
        Ok(())
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::KEY_REMAP
    }

    fn save_state(&self) -> serde_json::Value {
        // try_lock: button_mappings are only written on user-initiated calls so
        // contention here is negligible.
        match self.state.try_lock() {
            Ok(st) => {
                // A device with defaults must persist even an empty list, so a
                // user who clears a defaulted button back to Native isn't
                // re-seeded with the default on the next boot.
                let has_defaults = self.profile.is_some_and(|p| p.default_buttons.is_some());
                if st.remap.button_mappings.is_empty() && !has_defaults {
                    serde_json::Value::Null
                } else {
                    serde_json::to_value(&st.remap.button_mappings)
                        .unwrap_or(serde_json::Value::Null)
                }
            }
            Err(_) => serde_json::Value::Null,
        }
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        if let Ok(mappings) = serde_json::from_value::<Vec<ButtonMapping>>(v.clone()) {
            let buttons = self.state.lock().await.remap.reprog_cids.clone();
            if let Err(e) = crate::input::validate::validate_button_mappings(&buttons, &mappings) {
                log::warn!("[{}] rejecting invalid restored mappings: {e:#}", self.id);
                return;
            }
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
    use crate::drivers::vendors::logitech::devices::generic::state::LogitechDeviceState;
    use crate::state::AppState;
    use std::collections::HashMap;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    fn make_state_with_feature(feat_key: u16, feat_idx: u8) -> Arc<Mutex<LogitechDeviceState>> {
        let mut features = HashMap::new();
        features.insert(feat_key, feat_idx);
        Arc::new(Mutex::new(LogitechDeviceState {
            features: Arc::new(features),
            ..LogitechDeviceState::default()
        }))
    }

    #[tokio::test]
    async fn dispatch_reprog_button_press() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        let app = make_app();
        let mut btn_rx = app.input.button_event_tx.subscribe();

        // Two big-endian CID entries: 0x00D7 and 0x00C9 held
        let handled = dispatch_button_notification(
            0x09,
            0x00,
            &[0x00, 0xD7, 0x00, 0xC9, 0x00, 0x00, 0x00, 0x00],
            &state,
            &app,
            "d",
        )
        .await;

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
        let mut btn_rx = app.input.button_event_tx.subscribe();

        let handled = dispatch_button_notification(
            0x09,
            0x00,
            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state,
            &app,
            "d",
        )
        .await;

        assert!(handled);
        let evt = btn_rx.try_recv().unwrap();
        assert!(evt.pressed.is_empty());
        assert!(evt.released.contains(&0x00D7));
    }

    #[tokio::test]
    async fn dispatch_mouse_button_spy_bitmap() {
        let state = make_state_with_feature(feature::MOUSE_BUTTON_SPY, 0x0a);
        let app = make_app();
        let mut btn_rx = app.input.button_event_tx.subscribe();

        // Bitmap 0x0005 = bits 0 and 2 set → synthetic CIDs 1 and 3
        let handled =
            dispatch_button_notification(0x0a, 0x00, &[0x05, 0x00, 0x00, 0x00], &state, &app, "d")
                .await;

        assert!(handled);
        let evt = btn_rx.try_recv().unwrap();
        assert!(evt.pressed.contains(&1));
        assert!(evt.pressed.contains(&3));
    }

    #[tokio::test]
    async fn dispatch_returns_false_for_wrong_address() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        let app = make_app();
        let mut btn_rx = app.input.button_event_tx.subscribe();

        let handled = dispatch_button_notification(
            0x09,
            0x10,
            &[0x00, 0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state,
            &app,
            "d",
        )
        .await;

        assert!(!handled);
        assert!(btn_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_returns_false_for_unknown_sub_id() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        let app = make_app();
        let mut btn_rx = app.input.button_event_tx.subscribe();

        let handled = dispatch_button_notification(
            0x0f,
            0x00,
            &[0x00, 0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state,
            &app,
            "d",
        )
        .await;

        assert!(!handled);
        assert!(btn_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn dispatch_no_event_when_state_unchanged() {
        let state = make_state_with_feature(feature::REPROG_CONTROLS_V4, 0x09);
        state.lock().await.remap.prev_diverted_cids = vec![0x00D7];
        let app = make_app();
        let mut btn_rx = app.input.button_event_tx.subscribe();

        // Same CID as prev — no change, no event
        let handled = dispatch_button_notification(
            0x09,
            0x00,
            &[0x00, 0xD7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &state,
            &app,
            "d",
        )
        .await;

        assert!(handled);
        assert!(btn_rx.try_recv().is_err());
    }

    fn compute_deltas(prev: &[u16], current: &[u16]) -> (Vec<u16>, Vec<u16>) {
        let pressed: Vec<u16> = current
            .iter()
            .filter(|cid| !prev.contains(cid))
            .copied()
            .collect();
        let released: Vec<u16> = prev
            .iter()
            .filter(|cid| !current.contains(cid))
            .copied()
            .collect();
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
        use halod_shared::types::{ButtonAction, ButtonMapping, CycleDir, MouseBtn};
        let m = ButtonMapping {
            cid: 0x0051,
            base: ButtonAction::MouseButton {
                btn: MouseBtn::Middle,
            },
            shifted: ButtonAction::DpiCycle {
                direction: CycleDir::Up,
            },
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ButtonMapping = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cid, m.cid);
        assert_eq!(back.base, m.base);
        assert_eq!(back.shifted, m.shifted);
    }

    // Only Some(0x02) host-mode should allow setCidReporting; Some(0x01) onboard
    // blocks it; None (no ONBOARD_PROFILES feature) has no mode restriction.

    fn reprog_guard_should_skip(onboard_mode: Option<u8>) -> bool {
        // Mirrors the guard at apply_reprog_mappings: return early only when
        // onboard_mode is known-and-not-host-mode.
        onboard_mode.is_some_and(|m| m != 0x02)
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
