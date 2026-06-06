//! Onboard-profile flash management and DPI capabilities for `LogitechDevice` —
//! sector read/write helpers, the `OnboardProfilesCapability` and `DpiCapability`
//! impls, the active-profile reconciler, and the background DPI watcher.

use std::sync::{Arc, Weak};

use anyhow::{bail, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::drivers::vendors::logitech::devices::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::state::{is_host_mode, LogitechDeviceState};
use crate::drivers::vendors::logitech::protocols::hidpp::{
    feature,
    onboard::{
        parse_dpi_steps_from_sector, parse_profile_directory, patch_profile_sector,
        read_full_sector_via, rom_source_sector, set_sector_crc, write_full_sector_via,
    },
    dpi::encode_set_dpi,
    HidppMessenger,
};
use crate::drivers::{DpiCapability, OnboardProfilesCapability};
use crate::ipc::broadcast_state;
use halod_protocol::types::{DpiMode, DpiStatus};

impl LogitechDevice {
    /// Read a full ONBOARD_PROFILES sector. Thin `&self` wrapper over
    /// [`read_full_sector_via`] using the device's current transport.
    pub(super) async fn read_full_sector(&self, op_idx: u8, sector: u16, size: usize) -> Option<Vec<u8>> {
        let (msg, devnum) = self.transport_snapshot().await;
        read_full_sector_via(&msg, devnum, op_idx, sector, size).await
    }

    // ── Onboard profile flash helpers ─────────────────────────────────────────

    /// Erase, write (in 16-byte chunks) and commit a full flash sector via
    /// ONBOARD_PROFILES funcs 0x60/0x70/0x80. `write_sector` must be a writable
    /// RAM sector (not a read-only 0x01xx ROM address). Thin `&self` wrapper
    /// over [`write_full_sector_via`] using the device's current transport.
    async fn write_full_sector(&self, op_idx: u8, write_sector: u16, bytes: &[u8]) -> Result<()> {
        let (msg, devnum) = self.transport_snapshot().await;
        write_full_sector_via(&msg, devnum, op_idx, write_sector, bytes).await
    }

    async fn write_dpi_steps(&self, new_steps: Vec<u16>) -> Result<()> {
        if new_steps.is_empty() {
            bail!("DPI steps list cannot be empty");
        }
        let (op_idx, sector, sector_size) = {
            let state = self.state.lock().await;
            let Some(&op_idx) = state.features.get(&feature::ONBOARD_PROFILES) else {
                bail!("ONBOARD_PROFILES feature not available");
            };
            (op_idx, state.profile.profile_sector, state.profile.profile_sector_size)
        };

        if sector_size < 16 {
            bail!("Invalid profile sector size: {sector_size}");
        }

        // ROM sectors (>= 0x0100) are read-only; the writable user sector lives at sector & 0xFF.
        let write_sector = if sector >= 0x0100 { sector & 0xFF } else { sector };
        log::debug!("[{}] write_dpi_steps: sector={sector:#06x} write_sector={write_sector:#06x} size={sector_size}", self.id);

        // Read the full sector from write_sector (user-writable address).
        let mut sector_bytes = self.read_full_sector(op_idx, write_sector, sector_size).await
            .ok_or_else(|| anyhow::anyhow!("Failed to read profile sector {write_sector:#06x}"))?;

        patch_profile_sector(&mut sector_bytes, &new_steps, sector_size);
        self.write_full_sector(op_idx, write_sector, &sector_bytes).await?;

        // Update cached steps
        self.state.lock().await.profile.profile_steps = new_steps.clone();
        self.restore_rgb_control().await;

        log::info!("[{}] DPI steps saved: {:?}", self.id, new_steps);
        Ok(())
    }

    // ── Onboard profile management ─────────────────────────────────────────────

    /// Snapshot `(op_idx, sector_size)` from cached state, erroring if the
    /// ONBOARD_PROFILES feature or sector size is unavailable.
    async fn onboard_profile_ctx(&self) -> Result<(u8, usize)> {
        let state = self.state.lock().await;
        let Some(&op_idx) = state.features.get(&feature::ONBOARD_PROFILES) else {
            bail!("ONBOARD_PROFILES feature not available");
        };
        let sector_size = state.profile.profile_sector_size;
        if sector_size < 16 {
            bail!("Invalid profile sector size: {sector_size}");
        }
        Ok((op_idx, sector_size))
    }

    /// Copy a ROM factory profile into the writable RAM sector for `slot`
    /// (1-based). Slots beyond the device's factory profile count have no ROM
    /// default of their own, so they are seeded from ROM profile 1.
    async fn restore_profile_slot(&self, slot: u8) -> Result<()> {
        if slot == 0 {
            bail!("profile slot must be 1-based");
        }
        let (op_idx, sector_size) = self.onboard_profile_ctx().await?;
        let rom_count = self.state.lock().await.profile.rom_profile_count;
        let rom_sector = rom_source_sector(slot, rom_count);
        let ram_sector = slot as u16;
        if slot > rom_count {
            log::info!("[{}] restore_profile_slot: slot {slot} has no factory ROM default (rom_count={rom_count}); seeding from ROM profile 1", self.id);
        }
        log::debug!("[{}] restore_profile_slot: rom={rom_sector:#06x} -> ram={ram_sector:#06x}", self.id);

        let mut rom_bytes = self.read_full_sector(op_idx, rom_sector, sector_size).await
            .ok_or_else(|| anyhow::anyhow!("Failed to read ROM profile sector {rom_sector:#06x}"))?;
        // ROM sectors carry a placeholder trailing CRC (0xFFFF); the device
        // validates the CRC on commit, so recompute it over the content before
        // writing — otherwise endWrite fails with a hardware error (code 0x04).
        set_sector_crc(&mut rom_bytes, sector_size);
        self.write_full_sector(op_idx, ram_sector, &rom_bytes).await?;
        Ok(())
    }

    /// Read the directory sector, flip the enabled flag for `slot` (1-based),
    /// recompute its CRC16 and write it back.
    async fn write_directory_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
        if slot == 0 {
            bail!("profile slot must be 1-based");
        }
        let (op_idx, sector_size) = self.onboard_profile_ctx().await?;
        let mut dir = self.read_full_sector(op_idx, 0x0000, sector_size).await
            .ok_or_else(|| anyhow::anyhow!("Failed to read profile directory"))?;

        // Locate the directory entry whose sector address matches this slot.
        let entries = parse_profile_directory(&dir);
        let entry_idx = entries
            .iter()
            .position(|(sector, _)| *sector == slot as u16)
            .ok_or_else(|| anyhow::anyhow!("profile slot {slot} not present in directory"))?;
        dir[entry_idx * 4 + 2] = u8::from(enabled);
        set_sector_crc(&mut dir, sector_size);

        self.write_full_sector(op_idx, 0x0000, &dir).await?;
        Ok(())
    }

    /// Re-read the directory and active profile sector into cached state after a
    /// profile-management write. Re-reads DPI steps too, but only for devices
    /// that have `ADJUSTABLE_DPI` — switching profiles changes the active steps.
    async fn refresh_onboard_profile(&self) {
        let (op_idx, has_dpi) = {
            let st = self.state.lock().await;
            (
                st.features.get(&feature::ONBOARD_PROFILES).copied(),
                st.features.contains_key(&feature::ADJUSTABLE_DPI),
            )
        };
        if let Some(op_idx) = op_idx {
            let mut state = self.state.lock().await;
            self.init_onboard_profile(op_idx, &mut state).await;
            if has_dpi {
                self.init_profile_dpi_steps(op_idx, &mut state).await;
            }
        }
    }
}

/// Re-read the device's onboard mode and refresh cached `dpi.onboard_mode`.
/// Func 0x20 (get); 0x10 is *set* and would clobber the device's mode.
pub(super) async fn reconcile_onboard_mode(
    msg: &HidppMessenger,
    devnum: u8,
    op_idx: u8,
    state: &Arc<Mutex<LogitechDeviceState>>,
) -> bool {
    let reply = match msg.feature_request(devnum, op_idx, 0x20, &[]).await {
        Ok(r) => r,
        Err(_) => return false,
    };
    let mut st = state.lock().await;
    apply_onboard_mode_reply(&reply, &mut st.profile.onboard_mode)
}

pub(super) fn apply_onboard_mode_reply(reply: &[u8], current: &mut Option<u8>) -> bool {
    let Some(&mode) = reply.first() else { return false };
    if *current == Some(mode) {
        return false;
    }
    *current = Some(mode);
    true
}

/// Re-read the device's active profile and refresh cached state if it changed
/// (e.g. the user pressed the on-mouse profile button). Returns true when the
/// active profile changed and callers should broadcast.
pub(super) async fn reconcile_onboard_profile(
    msg: &HidppMessenger,
    devnum: u8,
    op_idx: u8,
    sector_size: usize,
    state: &Arc<Mutex<LogitechDeviceState>>,
) -> bool {
    // getCurrentProfile (func 0x40)
    let reply = match msg.feature_request(devnum, op_idx, 0x40, &[]).await {
        Ok(r) if r.len() >= 2 => r,
        _ => return false,
    };
    if reply[0..2] == [0xFF, 0xFF] || reply[0..2] == [0x00, 0x00] {
        return false; // no active onboard profile (host mode)
    }
    let sector = u16::from_be_bytes([reply[0], reply[1]]);
    if state.lock().await.profile.profile_sector == sector {
        return false; // unchanged
    }

    // Active profile changed — re-read the sector (DPI steps + active step) and
    // the directory so the UI reflects the new profile.
    let sector_data = read_full_sector_via(msg, devnum, op_idx, sector, sector_size).await;
    let dir = read_full_sector_via(msg, devnum, op_idx, 0x0000, sector_size)
        .await
        .map(|d| parse_profile_directory(&d));

    let mut st = state.lock().await;
    st.profile.profile_sector = sector;
    if let Some(data) = &sector_data {
        let steps = parse_dpi_steps_from_sector(data);
        let step_idx = data.get(1).copied().unwrap_or(0) as usize;
        st.dpi.dpi_current = steps.get(step_idx).copied().or(st.dpi.dpi_current);
        st.profile.profile_steps = steps;
    }
    if let Some(dir) = dir {
        st.profile.profile_dir = dir;
    }
    true
}

// ── DPI watcher ───────────────────────────────────────────────────────────────

pub(super) fn start_dpi_watcher(
    notify_rx: tokio::sync::broadcast::Receiver<crate::drivers::vendors::logitech::protocols::hidpp::HidppNotification>,
    transport: Option<(Weak<HidppMessenger>, u8)>,
    state: Arc<Mutex<LogitechDeviceState>>,
    id: String,
    app: Arc<crate::state::AppState>,
) {
    let mut rx = notify_rx;
    tokio::spawn(async move {
        // Safety-net poll: the device can leave host mode silently (no notif).
        let mut mode_ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        mode_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        mode_ticker.tick().await;
        loop {
            tokio::select! {
                _ = mode_ticker.tick() => {
                    let (op_idx, online) = {
                        let st = state.lock().await;
                        (st.features.get(&feature::ONBOARD_PROFILES).copied(), st.online)
                    };
                    if !online { continue }
                    let Some(op) = op_idx else { continue };
                    let Some((weak_msg, devnum)) = transport.as_ref() else { continue };
                    // If the wired messenger was dropped (transport reverted), stop the watcher.
                    let Some(msg) = weak_msg.upgrade() else { break };
                    if reconcile_onboard_mode(&msg, *devnum, op, &state).await {
                        log::info!("[{id}] onboard mode drift detected, cache refreshed");
                        broadcast_state(Arc::clone(&app)).await;
                    }
                }
                result = rx.recv() => match result {
                    Ok(notif) => {
                        if super::key_remap::dispatch_button_notification(
                            notif.sub_id, notif.address, &notif.data, &state, &app, &id,
                        ).await {
                            continue;
                        }

                        let (op_idx, sector_size) = {
                            let st = state.lock().await;
                            (st.features.get(&feature::ONBOARD_PROFILES).copied(), st.profile.profile_sector_size)
                        };
                        let Some(op) = op_idx else { continue };
                        if notif.sub_id != op {
                            continue;
                        }

                        let mut changed = false;
                        if sector_size >= 16 {
                            if let Some((weak_msg, devnum)) = &transport {
                                if let Some(msg) = weak_msg.upgrade() {
                                    changed = reconcile_onboard_profile(
                                        &msg, *devnum, op, sector_size, &state,
                                    ).await;
                                }
                            }
                        }

                        if notif.address == 0x10 {
                            if let Some((idx, dpi)) = {
                                let mut st = state.lock().await;
                                let profile_steps = st.profile.profile_steps.clone();
                                st.dpi.apply_dpi_step_event(&profile_steps, &notif.data)
                            } {
                                log::trace!("[{id}] DPI step changed: idx={idx} dpi={dpi}");
                                changed = true;
                            }
                        } else if let Some((weak_msg, devnum)) = &transport {
                            if let Some(msg) = weak_msg.upgrade() {
                                if reconcile_onboard_mode(&msg, *devnum, op, &state).await {
                                    changed = true;
                                }
                            }
                        }

                        if changed {
                            broadcast_state(Arc::clone(&app)).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        log::trace!("[{id}] DPI watcher lagged {n}");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    });
}

// ── OnboardProfilesCapability ─────────────────────────────────────────────────

#[async_trait]
impl OnboardProfilesCapability for LogitechDevice {
    async fn switch_profile(&self, slot: u8) -> Result<()> {
        if slot == 0 {
            bail!("profile slot must be 1-based");
        }
        let (op_idx, _) = self.onboard_profile_ctx().await?;
        {
            let state = self.state.lock().await;
            if state.profile.onboard_mode == Some(0x02) {
                bail!("device is in host mode; switch to onboard mode first");
            }
            let enabled = state
                .profile.profile_dir
                .iter()
                .any(|&(sector, en)| (sector & 0xFF) as u8 == slot && en);
            if !enabled {
                bail!("profile slot {slot} is not enabled");
            }
        }
        // func=0x30 setCurrentProfile — params [sector_hi, sector_lo, 0x00]
        let (msg, devnum) = self.transport_snapshot().await;
        msg.feature_request(devnum, op_idx, 0x30, &[0x00, slot, 0x00]).await?;
        self.refresh_onboard_profile().await;
        log::info!("[{}] switched to onboard profile {slot}", self.id);
        Ok(())
    }

    async fn restore_profile(&self, slot: u8) -> Result<()> {
        // Restore is a factory reset — only meaningful for slots that have a
        // ROM default of their own. ROM-less slots only support add/remove.
        let rom_count = self.state.lock().await.profile.rom_profile_count;
        if slot > rom_count {
            bail!("profile {slot} has no factory default to restore");
        }
        self.restore_profile_slot(slot).await?;
        self.refresh_onboard_profile().await;
        self.restore_rgb_control().await;
        log::info!("[{}] restored onboard profile {slot} from ROM defaults", self.id);
        Ok(())
    }

    async fn set_profile_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
        // Adding a slot: populate its RAM sector from ROM first so a freshly
        // enabled slot never holds stale or corrupt content.
        if enabled {
            self.restore_profile_slot(slot).await?;
        }
        self.write_directory_enabled(slot, enabled).await?;
        self.refresh_onboard_profile().await;
        self.restore_rgb_control().await;
        log::info!(
            "[{}] onboard profile {slot} {}",
            self.id,
            if enabled { "enabled" } else { "disabled" }
        );
        Ok(())
    }
}

// ── DpiCapability ─────────────────────────────────────────────────────────────
//
// Merges the former `DpiProfileCapability` (onboard-profile flash DPI) and
// `SoftwareDpiCapability` (host-mode software DPI) into one capability. The
// device's `dpi.onboard_mode` selects which layer is live: `is_host_mode`
// (Some(0x02)) → software DPI; otherwise → onboard-profile DPI.

#[async_trait]
impl DpiCapability for LogitechDevice {
    async fn dpi_status(&self) -> DpiStatus {
        let state = self.state.lock().await;
        let host = state.profile.onboard_mode.map(is_host_mode).unwrap_or(false);
        let available_dpis = state.dpi.dpi_list.clone();
        if host {
            // Host mode — the software-DPI step list, a config-only construct.
            DpiStatus {
                steps: state.dpi.software_dpi_steps.clone(),
                current_index: state.dpi.software_dpi_index,
                current_dpi: state.dpi.host_current_dpi(),
                available_dpis,
                mode: DpiMode::Host,
            }
        } else {
            // Onboard mode — from the active onboard profile (old dpi_profile).
            let steps = state.profile.profile_steps.clone();
            let current_dpi = state.dpi.dpi_current.unwrap_or(0);
            let current_index = steps.iter().position(|&s| s == current_dpi).unwrap_or(0);
            DpiStatus {
                steps,
                current_index,
                current_dpi,
                available_dpis,
                mode: DpiMode::Onboard,
            }
        }
    }

    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()> {
        if steps.is_empty() {
            bail!("DPI steps list cannot be empty");
        }
        let (host, avail) = {
            let state = self.state.lock().await;
            (
                state.profile.onboard_mode.map(is_host_mode).unwrap_or(false),
                state.dpi.dpi_list.clone(),
            )
        };
        for &s in &steps {
            if !avail.is_empty() && !avail.contains(&s) {
                bail!("DPI value {s} not in supported list");
            }
        }
        if host {
            // Host-mode DPI steps are a config-only list — never persisted to
            // the device. Just store them; the active hardware DPI changes only
            // via an explicit `set_dpi_index` / `set_dpi_direct`.
            log::info!(
                "[{}] set_dpi_steps: host mode — software DPI list saved (config only, no device write)",
                self.id
            );
            self.state.lock().await.dpi.apply_host_steps(steps.clone());
            *self.dpi_steps_cache.lock().unwrap() = steps;
            Ok(())
        } else {
            if steps.len() > 5 {
                bail!("onboard DPI steps must have 1–5 entries, got {}", steps.len());
            }
            log::info!("[{}] set_dpi_steps: onboard mode — flashing active profile", self.id);
            self.write_dpi_steps(steps).await
        }
    }

    async fn set_dpi_index(&self, index: usize) -> Result<()> {
        let dpi = {
            let mut state = self.state.lock().await;
            // Software DPI cycling only applies in host mode. In onboard mode
            // the device's own profiles govern DPI — must not change the
            // cached index (the UI would show a phantom switch).
            if !state.profile.is_host() {
                bail!("DPI step selection is host-mode only");
            }
            let len = state.dpi.software_dpi_steps.len();
            if len == 0 { bail!("software DPI steps not configured"); }
            let idx = index.min(len - 1);
            state.dpi.software_dpi_index = idx;
            *self.dpi_index_cache.lock().unwrap() = idx;
            state.dpi.software_dpi_steps[idx]
        };
        self.set_dpi_direct(dpi).await
    }

    fn state_key(&self) -> &'static str { "dpi" }

    fn save_state(&self) -> serde_json::Value {
        let steps = self.dpi_steps_cache.lock().unwrap().clone();
        let index = *self.dpi_index_cache.lock().unwrap();
        if steps.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::json!({ "steps": steps, "index": index })
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(steps) = v.get("steps")
            .and_then(|s| serde_json::from_value::<Vec<u16>>(s.clone()).ok())
        {
            if !steps.is_empty() {
                // Apply into the async state cache so the profile-1 seed is overridden.
                let mut st = self.state.lock().await;
                let idx = v.get("index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as usize;
                let idx = idx.min(steps.len() - 1);
                st.dpi.software_dpi_index = idx;
                st.dpi.software_dpi_steps = steps.clone();
                drop(st);
                // Keep the sync cache in sync.
                *self.dpi_steps_cache.lock().unwrap() = steps;
                *self.dpi_index_cache.lock().unwrap() = idx;
            }
        }
    }

    async fn set_dpi_direct(&self, dpi: u16) -> Result<()> {
        let dpi_idx = {
            let state = self.state.lock().await;
            // Host-mode only — see `set_dpi_index`. Gates momentary DPI too.
            if !state.profile.is_host() {
                bail!("software DPI is host-mode only");
            }
            *state.features.get(&feature::ADJUSTABLE_DPI)
                .ok_or_else(|| anyhow::anyhow!("ADJUSTABLE_DPI not available"))?
        };
        let (msg, devnum) = self.transport_snapshot().await;
        // setSensorDPI (func 0x30): [sensor=0, dpi_hi, dpi_lo]
        msg.feature_request(devnum, dpi_idx, 0x30, &encode_set_dpi(dpi)).await?;
        self.state.lock().await.dpi.dpi_current = Some(dpi);
        log::debug!("[{}] software DPI set to {dpi}", self.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::vendors::logitech::protocols::hidpp::HidppNotification;
    use crate::state::AppState;
    use std::collections::HashMap;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    fn make_state_with_dpi(op_idx: u8, steps: Vec<u16>, current: u16) -> Arc<Mutex<LogitechDeviceState>> {
        let mut features = HashMap::new();
        features.insert(feature::ONBOARD_PROFILES, op_idx);
        use crate::drivers::vendors::logitech::devices::state::{DpiCache, ProfileState};
        Arc::new(Mutex::new(LogitechDeviceState {
            features,
            dpi: DpiCache {
                dpi_current: Some(current),
                ..DpiCache::default()
            },
            profile: ProfileState {
                profile_steps: steps,
                ..ProfileState::default()
            },
            ..LogitechDeviceState::default()
        }))
    }

    #[tokio::test]
    async fn dpi_watcher_updates_current_dpi_on_step_change() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![800, 1600, 3200], 800);
        let app = make_app();

        start_dpi_watcher(rx, None, Arc::clone(&state), "test_device".to_string(), app);

        tx.send(HidppNotification {
            devnum: 0xff, sub_id: 0x0b, address: 0x10, data: vec![2, 0, 0, 0, 0, 0, 0, 0],
        }).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(3200));
    }

    #[tokio::test]
    async fn dpi_watcher_ignores_wrong_sub_id() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![800, 1600], 800);
        let app = make_app();

        start_dpi_watcher(rx, None, Arc::clone(&state), "test_device".to_string(), app);

        tx.send(HidppNotification {
            devnum: 0xff, sub_id: 0x0c, address: 0x10, data: vec![1, 0, 0, 0, 0, 0, 0, 0],
        }).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(800));
    }

    #[tokio::test]
    async fn dpi_watcher_ignores_wrong_address() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![800, 1600], 800);
        let app = make_app();

        start_dpi_watcher(rx, None, Arc::clone(&state), "test_device".to_string(), app);

        tx.send(HidppNotification {
            devnum: 0xff, sub_id: 0x0b, address: 0x20, data: vec![1, 0, 0, 0, 0, 0, 0, 0],
        }).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(800));
    }

    #[tokio::test]
    async fn dpi_watcher_noop_when_profile_steps_empty() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![], 1200);
        let app = make_app();

        start_dpi_watcher(rx, None, Arc::clone(&state), "test_device".to_string(), app);

        tx.send(HidppNotification {
            devnum: 0xff, sub_id: 0x0b, address: 0x10, data: vec![0, 0, 0, 0, 0, 0, 0, 0],
        }).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(1200));
    }

    #[tokio::test]
    async fn dpi_watcher_out_of_range_step_index_is_noop() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![800, 1600], 800);
        let app = make_app();

        start_dpi_watcher(rx, None, Arc::clone(&state), "test_device".to_string(), app);

        tx.send(HidppNotification {
            devnum: 0xff, sub_id: 0x0b, address: 0x10, data: vec![5, 0, 0, 0, 0, 0, 0, 0],
        }).unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(800));
    }

    #[test]
    fn apply_onboard_mode_reply_updates_when_drifted() {
        let mut current = Some(0x02);
        assert!(apply_onboard_mode_reply(&[0x01], &mut current));
        assert_eq!(current, Some(0x01));
    }

    #[test]
    fn apply_onboard_mode_reply_noop_when_unchanged() {
        let mut current = Some(0x02);
        assert!(!apply_onboard_mode_reply(&[0x02], &mut current));
        assert_eq!(current, Some(0x02));
    }

    #[test]
    fn apply_onboard_mode_reply_seeds_from_none() {
        let mut current: Option<u8> = None;
        assert!(apply_onboard_mode_reply(&[0x02], &mut current));
        assert_eq!(current, Some(0x02));
    }

    #[test]
    fn apply_onboard_mode_reply_ignores_empty_reply() {
        let mut current = Some(0x02);
        assert!(!apply_onboard_mode_reply(&[], &mut current));
        assert_eq!(current, Some(0x02));
    }
}
