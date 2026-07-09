// SPDX-License-Identifier: GPL-3.0-or-later
//! Onboard-profile flash management and DPI capabilities for `LogitechDevice` —
//! the `OnboardProfilesCapability` and `DpiCapability` impls, the active-profile
//! reconciler, and the background DPI watcher. Sector I/O, mode reads and DPI
//! reads all go through the protocol handle ([`Hidpp20`]); this file owns the
//! profile state machine and the policy around it.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use anyhow::{bail, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::{
    is_host_mode, LogitechDeviceState,
};
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::settings::{
    build_onboard_profiles, parse_dpi_steps_from_sector, parse_profile_directory,
    patch_profile_sector, rom_source_sector, set_sector_crc,
};
use crate::drivers::vendors::logitech::protocols::hidpp::v2::Hidpp20;
use crate::drivers::vendors::logitech::protocols::hidpp::{HidppChannel, HidppNotification};
use crate::drivers::{DpiCapability, OnboardProfilesCapability};
use halod_shared::types::{DeviceCapability, DpiMode, DpiStatus};

impl LogitechDevice {
    // ── Onboard profile flash helpers ─────────────────────────────────────────

    async fn write_dpi_steps(&self, new_steps: Vec<u16>) -> Result<()> {
        if new_steps.is_empty() {
            bail!("DPI steps list cannot be empty");
        }
        let sector_size = self.onboard_sector_size().await?;
        let sector = self.state.lock().await.profile.profile_sector;

        // ROM sectors (>= 0x0100) are read-only; the writable user sector lives at sector & 0xFF.
        let write_sector = if sector >= 0x0100 {
            sector & 0xFF
        } else {
            sector
        };
        log::debug!(
            "[{}] write_dpi_steps: sector={sector:#06x} write_sector={write_sector:#06x} size={sector_size}",
            self.id
        );

        let hidpp = self.hidpp2().await;
        let mut sector_bytes = hidpp
            .read_profile_sector(write_sector, sector_size)
            .await
            .ok_or_else(|| anyhow::anyhow!("Failed to read profile sector {write_sector:#06x}"))?;

        patch_profile_sector(&mut sector_bytes, &new_steps, sector_size)?;
        hidpp
            .write_profile_sector(write_sector, &sector_bytes)
            .await?;

        self.state.lock().await.profile.profile_steps = new_steps.clone();
        self.restore_rgb_control().await;

        log::info!("[{}] DPI steps saved: {:?}", self.id, new_steps);
        Ok(())
    }

    // ── Onboard profile management ─────────────────────────────────────────────

    /// Snapshot the sector size from cached state, erroring if the
    /// ONBOARD_PROFILES feature or sector size is unavailable.
    async fn onboard_sector_size(&self) -> Result<usize> {
        let state = self.state.lock().await;
        if !state.features.contains_key(&feature::ONBOARD_PROFILES) {
            bail!("ONBOARD_PROFILES feature not available");
        }
        let sector_size = state.profile.profile_sector_size;
        if sector_size < 16 {
            bail!("Invalid profile sector size: {sector_size}");
        }
        Ok(sector_size)
    }

    /// Copy a ROM factory profile into the writable RAM sector for `slot`
    /// (1-based). Slots beyond the device's factory profile count are seeded
    /// from ROM profile 1.
    async fn restore_profile_slot(&self, slot: u8) -> Result<()> {
        if slot == 0 {
            bail!("profile slot must be 1-based");
        }
        let sector_size = self.onboard_sector_size().await?;
        let rom_count = self.state.lock().await.profile.rom_profile_count;
        let rom_sector = rom_source_sector(slot, rom_count);
        let ram_sector = slot as u16;
        if slot > rom_count {
            log::info!("[{}] restore_profile_slot: slot {slot} has no factory ROM default (rom_count={rom_count}); seeding from ROM profile 1", self.id);
        }
        log::debug!(
            "[{}] restore_profile_slot: rom={rom_sector:#06x} -> ram={ram_sector:#06x}",
            self.id
        );

        let hidpp = self.hidpp2().await;
        let mut rom_bytes = hidpp
            .read_profile_sector(rom_sector, sector_size)
            .await
            .ok_or_else(|| {
                anyhow::anyhow!("Failed to read ROM profile sector {rom_sector:#06x}")
            })?;
        // ROM sectors carry a placeholder trailing CRC; recompute it before
        // writing or endWrite fails with a hardware error.
        set_sector_crc(&mut rom_bytes, sector_size)?;
        hidpp.write_profile_sector(ram_sector, &rom_bytes).await?;
        Ok(())
    }

    /// Read the directory sector, flip the enabled flag for `slot` (1-based),
    /// recompute its CRC16 and write it back.
    async fn write_directory_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
        if slot == 0 {
            bail!("profile slot must be 1-based");
        }
        let sector_size = self.onboard_sector_size().await?;
        let hidpp = self.hidpp2().await;
        let mut dir = hidpp
            .read_profile_sector(0x0000, sector_size)
            .await
            .ok_or_else(|| anyhow::anyhow!("Failed to read profile directory"))?;

        let entries = parse_profile_directory(&dir);
        let entry_idx = entries
            .iter()
            .position(|(sector, _)| *sector == slot as u16)
            .ok_or_else(|| anyhow::anyhow!("profile slot {slot} not present in directory"))?;
        dir[entry_idx * 4 + 2] = u8::from(enabled);
        set_sector_crc(&mut dir, sector_size)?;

        hidpp.write_profile_sector(0x0000, &dir).await?;
        Ok(())
    }

    /// Re-read the directory and active profile sector into cached state after a
    /// profile-management write. Re-reads DPI steps too for ADJUSTABLE_DPI devices.
    async fn refresh_onboard_profile(&self) {
        let (has_onboard, has_dpi) = {
            let st = self.state.lock().await;
            let has_onboard = st.features.contains_key(&feature::ONBOARD_PROFILES);
            let has_dpi = has_onboard && st.features.contains_key(&feature::ADJUSTABLE_DPI);
            (has_onboard, has_dpi)
        };
        if has_onboard {
            let mut state = self.state.lock().await;
            self.init_onboard_profile(&mut state).await;
            if has_dpi {
                self.init_profile_dpi_steps(&mut state).await;
            }
        }
    }
}

// ── Initialisation ────────────────────────────────────────────────────────────

impl LogitechDevice {
    pub(super) async fn init_dpi(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        if !features.contains_key(&feature::ADJUSTABLE_DPI) {
            log::debug!("[{}] ADJUSTABLE_DPI not present", self.id);
            return;
        }
        let hidpp = self.hidpp2_with(features).await;
        state.dpi.dpi_list = hidpp.read_dpi_list().await;
        if let Some(dpi) = hidpp.read_current_dpi().await {
            state.dpi.dpi_current = Some(dpi);
        }

        // DPI steps live in the active onboard-profile sector. `init_onboard`
        // runs first, so profile_sector / profile_sector_size are already set.
        if features.contains_key(&feature::ONBOARD_PROFILES) {
            self.init_profile_dpi_steps(state).await;
        }

        // Seed the host-mode software DPI list from profile 1 on first discovery.
        let profile_steps = state.profile.profile_steps.clone();
        state.dpi.seed_software_dpi_from_profile(&profile_steps);
    }

    /// Read onboard-profile state (host/onboard mode, profile directory, active
    /// sector). Kept separate from `init_dpi` so devices without `ADJUSTABLE_DPI`
    /// still get host-mode detection.
    pub(super) async fn init_onboard(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        if features.contains_key(&feature::ONBOARD_PROFILES) {
            self.init_onboard_profile(state).await;
            return;
        }
        // Feature absent — clear any stale onboard state from a prior device so
        // the UI can't offer profile actions this device would reject.
        state.profile.profile_dir.clear();
        state.profile.profile_steps.clear();
        state.profile.profile_sector = 0;
        state.profile.profile_sector_size = 0;
        state.profile.rom_profile_count = 0;
        state.profile.onboard_mode = None;
    }

    /// Read host/onboard mode, capabilities and the active profile, committing to
    /// `state` only once every read succeeds — a transient failure leaves the
    /// previously cached values intact rather than dropping the device out of
    /// host mode.
    pub(super) async fn init_onboard_profile(&self, state: &mut LogitechDeviceState) {
        let hidpp = self.hidpp2_with(state.features.as_ref()).await;

        let Some(mode) = hidpp.read_onboard_mode().await else {
            log::warn!("[{}] ONBOARD_PROFILES onboard mode unavailable", self.id);
            return;
        };
        let Some(caps) = hidpp.read_onboard_capabilities().await else {
            log::warn!("[{}] ONBOARD_PROFILES capabilities unavailable", self.id);
            return;
        };
        let sector_size = caps.sector_size;
        log::debug!(
            "[{}] ONBOARD_PROFILES sector_size={sector_size} rom_profiles={}",
            self.id,
            caps.rom_profile_count
        );

        // Mode + capability reads succeeded — commit them. These are cheap
        // single-frame reads and drive the host-mode toggle, so refresh them
        // even if the (multi-frame, flakier) directory read below fails.
        state.profile.onboard_mode = Some(mode);
        state.profile.rom_profile_count = caps.rom_profile_count;
        state.profile.profile_sector_size = sector_size;

        // Read the profile directory (sector 0x0000). This multi-chunk flash
        // read transiently fails on wireless (INVALID_ARGUMENT while the bus is
        // busy). Preserve the last-known-good directory + active sector rather
        // than blanking them: an empty directory makes the onboard capability
        // vanish from the UI and collapses the DPI list on every dropped read.
        let Some(dir_bytes) = hidpp.read_profile_sector(0x0000, sector_size).await else {
            log::debug!(
                "[{}] profile directory read failed; keeping last-known-good onboard state",
                self.id
            );
            return;
        };
        let profile_dir = parse_profile_directory(&dir_bytes);
        log::debug!("[{}] profile directory: {:?}", self.id, profile_dir);

        // Active profile sector; in host mode there is none, so fall back to the
        // first enabled profile so DPI steps stay populated regardless of mode.
        let first_enabled = profile_dir
            .iter()
            .find(|(_, enabled)| *enabled)
            .map(|(s, _)| *s)
            .unwrap_or(0x0001);
        let sector = hidpp
            .read_active_profile_sector()
            .await
            .unwrap_or(first_enabled);

        state.profile.profile_dir = profile_dir;
        state.profile.profile_sector = sector;
    }

    /// Parse the DPI step list from the active onboard-profile sector. Only
    /// meaningful for ADJUSTABLE_DPI devices (mice). Requires
    /// `init_onboard_profile` to have set `profile_sector` / `profile_sector_size`.
    pub(super) async fn init_profile_dpi_steps(&self, state: &mut LogitechDeviceState) {
        if state.profile.profile_sector_size == 0 {
            return;
        }
        let hidpp = self.hidpp2_with(state.features.as_ref()).await;
        match hidpp
            .read_profile_sector(
                state.profile.profile_sector,
                state.profile.profile_sector_size,
            )
            .await
        {
            Some(data) if data.len() >= 13 => {
                state.profile.profile_steps = parse_dpi_steps_from_sector(&data);
            }
            Some(data) => log::warn!("[{}] Profile sector too short ({}b)", self.id, data.len()),
            None => log::warn!("[{}] could not read profile sector for DPI steps", self.id),
        }
    }
}

/// Re-read the device's onboard mode and refresh cached `onboard_mode`.
pub(super) async fn reconcile_onboard_mode(
    hidpp: &Hidpp20,
    state: &Arc<Mutex<LogitechDeviceState>>,
) -> bool {
    let Some(mode) = hidpp.read_onboard_mode().await else {
        return false;
    };
    let mut st = state.lock().await;
    apply_onboard_mode_reply(mode, &mut st.profile.onboard_mode)
}

pub(super) fn apply_onboard_mode_reply(mode: u8, current: &mut Option<u8>) -> bool {
    if *current == Some(mode) {
        return false;
    }
    *current = Some(mode);
    true
}

/// Re-read the device's active profile and refresh cached state if it changed.
/// Returns true when the active profile changed and callers should broadcast.
pub(super) async fn reconcile_onboard_profile(
    hidpp: &Hidpp20,
    sector_size: usize,
    state: &Arc<Mutex<LogitechDeviceState>>,
) -> bool {
    let Some(sector) = hidpp.read_active_profile_sector().await else {
        return false; // no active onboard profile (host mode)
    };
    if state.lock().await.profile.profile_sector == sector {
        return false; // unchanged
    }

    let sector_data = hidpp.read_profile_sector(sector, sector_size).await;
    let dir = hidpp
        .read_profile_sector(0x0000, sector_size)
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

/// Shared HID++ feature-notification reconcile for the wired watcher and wireless
/// path. `None` = button event forwarded to key-remap (no broadcast); `Some(changed)`
/// otherwise. `hidpp: None` skips transport reconciles but still applies DPI steps;
/// `reconcile_mode_on_other_address` is the wired-only onboard-mode safety net.
pub(super) async fn reconcile_feature_notification(
    hidpp: Option<&Hidpp20>,
    sub_id: u8,
    address: u8,
    data: &[u8],
    state: &Arc<Mutex<LogitechDeviceState>>,
    app: Option<&Arc<crate::state::AppState>>,
    id: &str,
    reconcile_mode_on_other_address: bool,
) -> Option<bool> {
    if let Some(app) = app {
        if super::key_remap::dispatch_button_notification(sub_id, address, data, state, app, id)
            .await
        {
            return None;
        }
    }

    let (op_idx, sector_size) = {
        let st = state.lock().await;
        (
            st.features.get(&feature::ONBOARD_PROFILES).copied(),
            st.profile.profile_sector_size,
        )
    };
    let Some(op) = op_idx else { return Some(false) };
    if sub_id != op {
        return Some(false);
    }

    let mut changed = false;
    if let Some(hidpp) = hidpp {
        if sector_size >= 16 && reconcile_onboard_profile(hidpp, sector_size, state).await {
            log::trace!("[{id}] active onboard profile changed");
            changed = true;
        }
    }

    if address == 0x10 {
        if let Some((idx, dpi)) = {
            let mut st = state.lock().await;
            let profile_steps = st.profile.profile_steps.clone();
            st.dpi.apply_dpi_step_event(&profile_steps, data)
        } {
            log::trace!("[{id}] DPI step changed: idx={idx} dpi={dpi}");
            changed = true;
        }
    } else if reconcile_mode_on_other_address {
        if let Some(hidpp) = hidpp {
            if reconcile_onboard_mode(hidpp, state).await {
                changed = true;
            }
        }
    }

    Some(changed)
}

// ── DPI watcher ───────────────────────────────────────────────────────────────

pub(super) fn start_dpi_watcher(
    notify_rx: tokio::sync::broadcast::Receiver<HidppNotification>,
    transport: Option<(Weak<dyn HidppChannel>, u8)>,
    state: Arc<Mutex<LogitechDeviceState>>,
    id: String,
    app: Option<Arc<crate::state::AppState>>,
) {
    let id_log = id.clone();
    let mut rx = notify_rx;
    let poll = tokio::spawn(async move {
        // Safety-net poll: the device can leave host mode silently (no notif).
        let mut mode_ticker = tokio::time::interval(std::time::Duration::from_secs(5));
        mode_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        mode_ticker.tick().await;
        loop {
            tokio::select! {
                _ = mode_ticker.tick() => {
                    let (has_op, online, features) = {
                        let st = state.lock().await;
                        (st.features.contains_key(&feature::ONBOARD_PROFILES), st.online, Arc::clone(&st.features))
                    };
                    if !online { continue }
                    if !has_op { continue }
                    let Some((weak_msg, devnum)) = transport.as_ref() else { continue };
                    // If the wired messenger was dropped (transport reverted), stop the watcher.
                    let Some(msg) = weak_msg.upgrade() else { break };
                    let hidpp = Hidpp20::new(msg, *devnum, (*features).clone());
                    if reconcile_onboard_mode(&hidpp, &state).await {
                        log::info!("[{id}] onboard mode drift detected, cache refreshed");
                        if let Some(a) = app.as_ref() {
                            a.broadcast_state().await;
                        }
                    }
                }
                result = rx.recv() => match result {
                    Ok(notif) => {
                        // Release the state lock before the (potentially slow)
                        // HashMap clone for Hidpp20::new.
                        let features = {
                            let st = state.lock().await;
                            Arc::clone(&st.features)
                        };
                        // Live messenger if any; a dropped one still passes DPI/button events.
                        let hidpp = transport.as_ref()
                            .and_then(|(w, d)| w.upgrade().map(|m| Hidpp20::new(m, *d, (*features).clone())));
                        if let Some(true) = reconcile_feature_notification(
                            hidpp.as_ref(),
                            notif.sub_id, notif.address, &notif.data,
                            &state, app.as_ref(), &id, true,
                        ).await {
                            if let Some(a) = app.as_ref() {
                                a.broadcast_state().await;
                            }
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

    // Log if the task panics rather than exiting normally.
    tokio::spawn(async move {
        if let Err(e) = poll.await {
            if !e.is_cancelled() {
                log::error!("[{id_log}] DPI watcher task exited unexpectedly: {e}");
            }
        }
    });
}

// ── OnboardProfilesCapability ─────────────────────────────────────────────────

#[async_trait]
impl OnboardProfilesCapability for LogitechDevice {
    async fn to_wire(&self) -> Option<DeviceCapability> {
        let state = self.state.lock().await;
        if !state.features.contains_key(&feature::ONBOARD_PROFILES) {
            return None;
        }
        let mut profiles = build_onboard_profiles(
            &state.profile.profile_dir,
            state.profile.profile_sector,
            state.profile.rom_profile_count,
        )?;
        // Cached profile_sector points at the fallback "first enabled" slot in
        // host mode; zero it here so active_slot==0 ⇒ host-mode holds.
        let host = state
            .profile
            .onboard_mode
            .map(is_host_mode)
            .unwrap_or(false);
        super::device::clear_active_slot_in_host_mode(&mut profiles, host);
        Some(DeviceCapability::OnboardProfiles(profiles))
    }

    async fn switch_profile(&self, slot: u8) -> Result<()> {
        if slot == 0 {
            bail!("profile slot must be 1-based");
        }
        self.onboard_sector_size().await?;
        {
            let state = self.state.lock().await;
            if state.profile.onboard_mode == Some(0x02) {
                bail!("device is in host mode; switch to onboard mode first");
            }
            let enabled = state
                .profile
                .profile_dir
                .iter()
                .any(|&(sector, en)| (sector & 0xFF) as u8 == slot && en);
            if !enabled {
                bail!("profile slot {slot} is not enabled");
            }
        }
        self.hidpp2().await.set_current_profile(slot).await?;
        self.refresh_onboard_profile().await;
        log::info!("[{}] switched to onboard profile {slot}", self.id);
        Ok(())
    }

    async fn restore_profile(&self, slot: u8) -> Result<()> {
        // Restore is a factory reset — only meaningful for slots with a ROM
        // default of their own.
        let rom_count = self.state.lock().await.profile.rom_profile_count;
        if slot > rom_count {
            bail!("profile {slot} has no factory default to restore");
        }
        self.restore_profile_slot(slot).await?;
        self.refresh_onboard_profile().await;
        self.restore_rgb_control().await;
        log::info!(
            "[{}] restored onboard profile {slot} from ROM defaults",
            self.id
        );
        Ok(())
    }

    async fn set_profile_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
        // Adding a slot: populate its RAM sector from ROM first.
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

#[async_trait]
impl DpiCapability for LogitechDevice {
    async fn dpi_status(&self) -> DpiStatus {
        let state = self.state.lock().await;
        let host = state
            .profile
            .onboard_mode
            .map(is_host_mode)
            .unwrap_or(false);
        let available_dpis = state.dpi.dpi_list.clone();
        if host {
            let steps = state
                .dpi
                .software_dpi_steps
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            let current_index = *state
                .dpi
                .software_dpi_index
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            DpiStatus {
                current_dpi: state.dpi.host_current_dpi(),
                steps,
                current_index,
                available_dpis,
                mode: DpiMode::Host,
            }
        } else {
            let steps = if !state.profile.profile_steps.is_empty() {
                state.profile.profile_steps.clone()
            } else {
                state.dpi.dpi_current.map(|d| vec![d]).unwrap_or_default()
            };
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

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let has_dpi = {
            let state = self.state.lock().await;
            !state.profile.profile_steps.is_empty()
                || state.dpi.dpi_current.is_some()
                || state.features.contains_key(&feature::ADJUSTABLE_DPI)
        };
        if !has_dpi {
            return None;
        }
        Some(DeviceCapability::Dpi(self.dpi_status().await))
    }

    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()> {
        if steps.is_empty() {
            bail!("DPI steps list cannot be empty");
        }
        let (host, avail) = {
            let state = self.state.lock().await;
            (
                state
                    .profile
                    .onboard_mode
                    .map(is_host_mode)
                    .unwrap_or(false),
                state.dpi.dpi_list.clone(),
            )
        };
        for &s in &steps {
            if !avail.is_empty() && !avail.contains(&s) {
                bail!("DPI value {s} not in supported list");
            }
        }
        if host {
            log::info!(
                "[{}] set_dpi_steps: host mode, software DPI list saved (config only, no device write)",
                self.id
            );
            self.state.lock().await.dpi.apply_host_steps(steps);
            Ok(())
        } else {
            if steps.len() > 5 {
                bail!(
                    "onboard DPI steps must have 1–5 entries, got {}",
                    steps.len()
                );
            }
            log::info!(
                "[{}] set_dpi_steps: onboard mode, flashing active profile",
                self.id
            );
            self.write_dpi_steps(steps).await
        }
    }

    async fn set_dpi_index(&self, index: usize) -> Result<()> {
        let dpi = {
            let state = self.state.lock().await;
            if !state.profile.is_host() {
                bail!("DPI step selection is host-mode only");
            }
            let steps = state
                .dpi
                .software_dpi_steps
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let len = steps.len();
            if len == 0 {
                bail!("software DPI steps not configured");
            }
            let idx = index.min(len - 1);
            *state
                .dpi
                .software_dpi_index
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = idx;
            steps[idx]
        };
        self.set_dpi_direct(dpi).await
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::DPI
    }

    fn save_state(&self) -> serde_json::Value {
        let Ok(state) = self.state.try_lock() else {
            return serde_json::Value::Null;
        };
        let steps = state
            .dpi
            .software_dpi_steps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let index = *state
            .dpi
            .software_dpi_index
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if steps.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::json!({ "steps": steps, "index": index })
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(steps) = v
            .get("steps")
            .and_then(|s| serde_json::from_value::<Vec<u16>>(s.clone()).ok())
        {
            if !steps.is_empty() {
                let st = self.state.lock().await;
                let idx = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                let idx = idx.min(steps.len() - 1);
                *st.dpi
                    .software_dpi_index
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = idx;
                *st.dpi
                    .software_dpi_steps
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = steps;
            }
        }
    }

    async fn set_dpi_direct(&self, dpi: u16) -> Result<()> {
        {
            let state = self.state.lock().await;
            if !state.profile.is_host() {
                bail!("software DPI is host-mode only");
            }
            if !state.features.contains_key(&feature::ADJUSTABLE_DPI) {
                bail!("ADJUSTABLE_DPI not available");
            }
        }
        self.hidpp2().await.set_dpi(dpi).await?;
        self.state.lock().await.dpi.dpi_current = Some(dpi);
        log::debug!("[{}] software DPI set to {dpi}", self.id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use std::collections::HashMap;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    fn make_state_with_dpi(
        op_idx: u8,
        steps: Vec<u16>,
        current: u16,
    ) -> Arc<Mutex<LogitechDeviceState>> {
        let mut features = HashMap::new();
        features.insert(feature::ONBOARD_PROFILES, op_idx);
        use crate::drivers::vendors::logitech::devices::generic::state::{DpiCache, ProfileState};
        Arc::new(Mutex::new(LogitechDeviceState {
            features: Arc::new(features),
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

        start_dpi_watcher(
            rx,
            None,
            Arc::clone(&state),
            "test_device".to_string(),
            Some(app),
        );

        tx.send(HidppNotification {
            devnum: 0xff,
            sub_id: 0x0b,
            address: 0x10,
            data: vec![2, 0, 0, 0, 0, 0, 0, 0],
        })
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(3200));
    }

    #[tokio::test]
    async fn dpi_watcher_ignores_wrong_sub_id() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![800, 1600], 800);
        let app = make_app();

        start_dpi_watcher(
            rx,
            None,
            Arc::clone(&state),
            "test_device".to_string(),
            Some(app),
        );

        tx.send(HidppNotification {
            devnum: 0xff,
            sub_id: 0x0c,
            address: 0x10,
            data: vec![1, 0, 0, 0, 0, 0, 0, 0],
        })
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(800));
    }

    #[tokio::test]
    async fn dpi_watcher_ignores_wrong_address() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![800, 1600], 800);
        let app = make_app();

        start_dpi_watcher(
            rx,
            None,
            Arc::clone(&state),
            "test_device".to_string(),
            Some(app),
        );

        tx.send(HidppNotification {
            devnum: 0xff,
            sub_id: 0x0b,
            address: 0x20,
            data: vec![1, 0, 0, 0, 0, 0, 0, 0],
        })
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(800));
    }

    #[tokio::test]
    async fn dpi_watcher_noop_when_profile_steps_empty() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![], 1200);
        let app = make_app();

        start_dpi_watcher(
            rx,
            None,
            Arc::clone(&state),
            "test_device".to_string(),
            Some(app),
        );

        tx.send(HidppNotification {
            devnum: 0xff,
            sub_id: 0x0b,
            address: 0x10,
            data: vec![0, 0, 0, 0, 0, 0, 0, 0],
        })
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(1200));
    }

    #[tokio::test]
    async fn dpi_watcher_out_of_range_step_index_is_noop() {
        let (tx, rx) = tokio::sync::broadcast::channel::<HidppNotification>(8);
        let state = make_state_with_dpi(0x0b, vec![800, 1600], 800);
        let app = make_app();

        start_dpi_watcher(
            rx,
            None,
            Arc::clone(&state),
            "test_device".to_string(),
            Some(app),
        );

        tx.send(HidppNotification {
            devnum: 0xff,
            sub_id: 0x0b,
            address: 0x10,
            data: vec![5, 0, 0, 0, 0, 0, 0, 0],
        })
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(state.lock().await.dpi.dpi_current, Some(800));
    }

    // A transient failure reading the profile directory must NOT blank the
    // cached directory (which would make the onboard capability vanish from the
    // UI); the successfully-read mode is still applied.
    #[tokio::test]
    async fn init_onboard_profile_preserves_dir_when_directory_read_fails() {
        use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
        use crate::drivers::vendors::logitech::protocols::hidpp::test_util::MockHidppChannel;
        use halod_shared::types::DeviceType;
        use std::collections::VecDeque;

        let mut by_func: HashMap<u8, VecDeque<std::result::Result<Vec<u8>, String>>> =
            HashMap::new();
        by_func.insert(0x20, VecDeque::from([Ok(vec![0x02])])); // getMode -> host
        by_func.insert(
            0x00,
            VecDeque::from([Ok(vec![0, 0, 0, 0, 2, 0, 0, 0x00, 0x10])]),
        ); // getInfo: rom=2, sector=16
        by_func.insert(
            0x50,
            VecDeque::from([
                Err("code=0x02".to_string()),
                Err("code=0x02".to_string()),
                Err("code=0x02".to_string()),
            ]),
        ); // memoryRead fails every attempt

        let ch = Arc::new(MockHidppChannel::new(by_func));
        let dev =
            LogitechDevice::new_without_coordinator(0x01, 0xFFFF, None, DeviceType::Mouse, ch);
        {
            let mut st = dev.state.lock().await;
            st.features = Arc::new(HashMap::from([(feature::ONBOARD_PROFILES, 0x0b)]));
            st.profile.profile_dir = vec![(0x0001, true), (0x0002, false)];
            st.profile.profile_sector = 0x0001;
            st.profile.profile_sector_size = 16;
            st.profile.onboard_mode = Some(0x01);
        }
        let mut st = dev.state.lock().await;
        dev.init_onboard_profile(&mut st).await;

        assert_eq!(
            st.profile.profile_dir,
            vec![(0x0001, true), (0x0002, false)]
        );
        assert_eq!(st.profile.onboard_mode, Some(0x02));
    }

    // The wired DPI watcher forwards button events to the key-remap engine only
    // when it holds an AppState — the invariant `connect_direct` must preserve so
    // a runtime wireless→wired switch doesn't break host-mode remapping.
    #[tokio::test]
    async fn button_notification_dispatched_only_with_app() {
        let state = Arc::new(Mutex::new(LogitechDeviceState {
            features: Arc::new(HashMap::from([(feature::REPROG_CONTROLS_V4, 0x07)])),
            ..LogitechDeviceState::default()
        }));
        let app = make_app();
        let mut rx = app.input.button_event_tx.subscribe();

        // With app: the diverted-buttons event (CID 10) reaches the channel.
        let consumed = reconcile_feature_notification(
            None,
            0x07,
            0x00,
            &[0x00, 0x0A],
            &state,
            Some(&app),
            "d",
            true,
        )
        .await;
        assert_eq!(consumed, None, "the dispatcher consumes the notification");
        assert_eq!(rx.try_recv().unwrap().pressed, vec![10]);

        // Without app (the pre-fix watcher): nothing is dispatched.
        state.lock().await.remap.prev_diverted_cids.clear();
        let _ = reconcile_feature_notification(
            None,
            0x07,
            0x00,
            &[0x00, 0x0A],
            &state,
            None,
            "d",
            true,
        )
        .await;
        assert!(rx.try_recv().is_err(), "no dispatch without an AppState");
    }

    #[test]
    fn apply_onboard_mode_reply_updates_when_drifted() {
        let mut current = Some(0x02);
        assert!(apply_onboard_mode_reply(0x01, &mut current));
        assert_eq!(current, Some(0x01));
    }

    #[test]
    fn apply_onboard_mode_reply_noop_when_unchanged() {
        let mut current = Some(0x02);
        assert!(!apply_onboard_mode_reply(0x02, &mut current));
        assert_eq!(current, Some(0x02));
    }

    #[test]
    fn apply_onboard_mode_reply_seeds_from_none() {
        let mut current: Option<u8> = None;
        assert!(apply_onboard_mode_reply(0x02, &mut current));
        assert_eq!(current, Some(0x02));
    }
}
