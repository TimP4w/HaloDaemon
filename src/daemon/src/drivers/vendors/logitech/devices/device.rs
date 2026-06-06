// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// Generic Logitech HID++ 2.0 device — feature-driven capability composition.
///
/// Reference: Solaar (GPL-2.0-or-later) — hidpp20.py, settings_templates.py
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::{
    vendors::generic::devices::common::WireDeviceBuilder,
    vendors::logitech::devices::onboard::start_dpi_watcher,
    vendors::logitech::devices::profile::{
        build_device_id, find_profile, LogitechDeviceProfile, LOGITECH_VID, WIRED_HIDPP_INTERFACE,
    },
    vendors::logitech::devices::state::{is_host_mode, LogitechDeviceState},
    vendors::logitech::protocols::hidpp::{
        self, feature,
        onboard::build_onboard_profiles,
        HidppMessenger, RECEIVER_DEVNUM,
    },
    transports::hid::HidTransport,
    BatteryCapability, BooleanCapability, CapabilityRef,
    ChoiceCapability, ChoiceStateCache, Device, RgbStateSlot,
    TransportSwitchable, VisibilitySlot,
};
use crate::ipc::broadcast_state;
use crate::notify;
use crate::state::AppState;
use halod_protocol::types::{
    Battery, Boolean, Choice, ChoiceOption, ConnectionType, DeviceCapability,
    DeviceType, DpiMode, DpiStatus, KeyRemapStatus, OnboardProfiles, RgbDescriptor, RgbStatus, WireDevice,
};

// ── LogitechDevice ────────────────────────────────────────────────────────────

/// Shared per-receiver coordinator for per-key RGB writes.
///
/// write_frame on each device posts its packet batch here. A single background
/// task (owned by LogitechReceiver) collects all concurrent posts and sends them
/// in one feature_send_many_fire call, ensuring mouse and keyboard always write
/// the same canvas frame together and stay in sync.
pub struct PkWriteCoordinator {
    pub pending: tokio::sync::Mutex<HashMap<u8, Vec<Vec<u8>>>>,
    pub notify: tokio::sync::Notify,
}

impl PkWriteCoordinator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            pending: tokio::sync::Mutex::new(HashMap::new()),
            notify: tokio::sync::Notify::new(),
        })
    }

    pub async fn post(&self, devnum: u8, packets: Vec<Vec<u8>>) {
        self.pending.lock().await.insert(devnum, packets);
        self.notify.notify_one();
    }
}

/// Active transport for a LogitechDevice. Swapped atomically when the device
/// transitions between wireless (via receiver) and wired (direct USB).
pub(super) struct LogitechTransport {
    pub(super) messenger: Arc<HidppMessenger>,
    pub(super) devnum: u8,
    /// True when connected via wired USB — device owns its HID listener.
    pub(super) is_wired: bool,
    /// Saved wireless transport present only while device is in wired mode.
    pub(super) wireless_fallback: Option<(Arc<HidppMessenger>, u8)>,
}

pub struct LogitechDevice {
    pub(super) id: String,
    pub(super) profile: Option<&'static LogitechDeviceProfile>,
    pub(super) transport: Mutex<LogitechTransport>,
    pub(super) state: Arc<Mutex<LogitechDeviceState>>,
    pub(super) rgb_descriptor: std::sync::OnceLock<RgbDescriptor>,
    /// Set by LogitechReceiver after init. write_frame posts here instead of
    /// writing directly so all devices on the same receiver write the same frame.
    pub(super) pk_coordinator: std::sync::OnceLock<Arc<PkWriteCoordinator>>,
    /// Set in after_register. Used to restart the DPI watcher after a wired transport switch.
    pub(super) app_ref: std::sync::OnceLock<std::sync::Weak<crate::state::AppState>>,
    pub(super) rgb: RgbStateSlot,
    pub(super) visibility: VisibilitySlot,
    /// Choice (report rate) state cache — required by `ChoiceCapability`.
    pub(super) choice_cache: ChoiceStateCache,
    /// Sync-readable mirror of software DPI steps for `DpiCapability::save_state`.
    pub(super) dpi_steps_cache: std::sync::Mutex<Vec<u16>>,
    /// Sync-readable mirror of software DPI index for `DpiCapability::save_state`.
    pub(super) dpi_index_cache: std::sync::Mutex<usize>,
}

impl LogitechDevice {
    /// Whether this device is a keyboard, per its static profile descriptor.
    /// Used to branch RGB init / per-LED batching without inline topology checks.
    pub(super) fn is_keyboard(&self) -> bool {
        self.profile
            .map(|p| matches!(p.device_type, DeviceType::Keyboard))
            .unwrap_or(false)
    }

    pub fn new(
        devnum: u8,
        wpid: u16,
        serial: Option<&str>,
        messenger: Arc<HidppMessenger>,
    ) -> Self {
        let profile = find_profile(wpid);
        let id = build_device_id(serial, wpid, devnum as usize);
        Self {
            id,
            profile,
            transport: Mutex::new(LogitechTransport {
                messenger,
                devnum,
                is_wired: false,
                wireless_fallback: None,
            }),
            state: Arc::new(Mutex::new(LogitechDeviceState::default())),
            rgb_descriptor: std::sync::OnceLock::new(),
            pk_coordinator: std::sync::OnceLock::new(),
            app_ref: std::sync::OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            choice_cache: ChoiceStateCache::default(),
            dpi_steps_cache: std::sync::Mutex::new(Vec::new()),
            dpi_index_cache: std::sync::Mutex::new(0),
        }
    }

    /// Constructor for a directly-connected (wired USB) device.
    pub fn new_direct(path: &str, serial: Option<&str>, wired_pid: u16, idx: usize) -> anyhow::Result<Self> {
        // Windows exposes the HID++ interface as two collections — short reports
        // (0x10) and long reports (0x11) on separate device paths. Resolve both
        // and open a dual-handle transport; on Linux only one path exists and
        // the transport collapses to a single handle.
        let (short_path, long_path) = hidpp::collection::resolve_hidpp_paths(
            LOGITECH_VID,
            wired_pid,
            WIRED_HIDPP_INTERFACE,
            path,
            serial,
        )?;
        let transport_hid = HidTransport::open_dual(
            &short_path,
            long_path.as_deref().unwrap_or(""),
            None,
            50,
            false,
        )?;
        let messenger = Arc::new(HidppMessenger::new(transport_hid));
        let profile = find_profile(wired_pid);
        let id = build_device_id(serial, wired_pid, idx);
        Ok(Self {
            id,
            profile,
            transport: Mutex::new(LogitechTransport {
                messenger,
                devnum: RECEIVER_DEVNUM,
                is_wired: true,
                wireless_fallback: None,
            }),
            state: Arc::new(Mutex::new(LogitechDeviceState::default())),
            rgb_descriptor: std::sync::OnceLock::new(),
            pk_coordinator: std::sync::OnceLock::new(),
            app_ref: std::sync::OnceLock::new(),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            choice_cache: ChoiceStateCache::default(),
            dpi_steps_cache: std::sync::Mutex::new(Vec::new()),
            dpi_index_cache: std::sync::Mutex::new(0),
        })
    }

    pub fn set_pk_coordinator(&self, c: Arc<PkWriteCoordinator>) {
        let _ = self.pk_coordinator.set(c);
    }

    pub async fn devnum(&self) -> u8 {
        self.transport.lock().await.devnum
    }

    pub(super) async fn transport_snapshot(&self) -> (Arc<HidppMessenger>, u8) {
        let t = self.transport.lock().await;
        (Arc::clone(&t.messenger), t.devnum)
    }
}

// ── Device trait ──────────────────────────────────────────────────────────────

#[async_trait]
impl Device for LogitechDevice {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn hardware_serial(&self) -> Option<String> {
        // ID formats:
        //   "logitech_<8CHAR_HEX>"        — canonical (serial known, same for wired + wireless)
        //   "logitech_<PID>_<fallback>"   — no serial available
        let parts: Vec<&str> = self.id.splitn(3, '_').collect();
        let candidate = match parts.len() {
            2 => parts[1],  // "logitech_<serial>"
            3 => parts[2],  // "logitech_<pid>_<something>" (legacy fallback format) // TODO: remove fallback
            _ => return None,
        };
        if candidate.len() == 8 && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(candidate.to_uppercase())
        } else {
            None
        }
    }

    fn name(&self) -> &str {
        self.profile.map(|p| p.name).unwrap_or("Logitech Device")
    }

    fn vendor(&self) -> &str {
        "Logitech"
    }

    fn model(&self) -> &str {
        self.profile.map(|p| p.name).unwrap_or("Logitech Device")
    }

    // TODO: check razer init, it's way cleaner
    async fn initialize(&self) -> Result<bool> {
        let (is_wired, messenger) = {
            let t = self.transport.lock().await;
            (t.is_wired, Arc::clone(&t.messenger))
        };
        if is_wired {
            messenger.start_listener();
        }
        // Ping the device via ROOT to see if it's online
        let features = match self.init_features().await {
            Ok(f) => f,
            Err(e) => {
                log::info!("[{}] Offline at init ({e})", self.id);
                return Ok(false);
            }
        };

        let mut state = self.state.lock().await;
        state.online = true;

        let name = self.init_name(&features).await;
        state.name = name;

        self.init_battery(&features, &mut state).await;
        self.init_report_rate(&features, &mut state).await;
        self.init_onboard(&features, &mut state).await;
        self.init_dpi(&features, &mut state).await;
        self.init_reprog_controls(&features, &mut state).await;
        self.init_bitmap_buttons(&features, &mut state).await;
        let keyboard_layout = self.init_keyboard_layout(&features).await;
        self.init_rgb(&features, &keyboard_layout, &mut state).await;

        state.features = features;

        log::info!(
            "[{}] {} — battery={:?}%, rate={:?}ms, dpi={:?}, steps={:?}",
            self.id,
            state.name,
            state.battery.battery_level,
            state.report_rate.report_rate_ms,
            state.dpi.dpi_current,
            state.profile.profile_steps,
        );

        // Re-apply any persisted button mappings (enables GKEY software control
        // when mappings exist). Drop the state guard first — apply_reprog_mappings
        // locks it itself.
        drop(state);
        self.apply_reprog_mappings().await;

        Ok(true)
    }

    async fn close(&self) {
        let t = self.transport.lock().await;
        if t.is_wired {
            t.messenger.stop_listener();
        }
    }

    async fn after_register(&self, app: Arc<AppState>) {
        let _ = self.app_ref.set(Arc::downgrade(&app));
        let (is_wired, messenger, devnum) = {
            let t = self.transport.lock().await;
            (t.is_wired, Arc::clone(&t.messenger), t.devnum)
        };
        if is_wired {
            start_dpi_watcher(
                messenger.notify_tx.subscribe(),
                Some((Arc::downgrade(&messenger), devnum)),
                Arc::clone(&self.state),
                self.id.clone(),
                app,
            );
        }
    }

    fn debug_info_extra(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(p) = self.profile {
            out.push(("wpid".to_string(), format!("{:04x}", p.wpid)));
            out.push(("wired_pid".to_string(), format!("{:04x}", p.pid)));
            out.push(("profile".to_string(), p.name.to_string()));
        }
        // The transport mutex is async; reading it from a sync method would
        // require blocking. Use try_lock to surface wired/wireless when it is
        // free, and fall back to "unknown" when contended so the row always
        // renders in the debug UI.
        match self.transport.try_lock() {
            Ok(t) => {
                out.push((
                    "connection".to_string(),
                    if t.is_wired { "wired".to_string() } else { "wireless".to_string() },
                ));
                out.push(("devnum".to_string(), format!("0x{:02x}", t.devnum)));
                if t.wireless_fallback.is_some() {
                    out.push(("wireless_fallback".to_string(), "present".to_string()));
                }
            }
            Err(_) => {
                out.push(("connection".to_string(), "unknown (transport busy)".to_string()));
            }
        }
        out
    }

    async fn serialize(&self) -> WireDevice {
        let is_wired = self.transport.lock().await.is_wired;
        let state = self.state.lock().await;
        let mut caps: Vec<DeviceCapability> = Vec::new();

        // Battery
        if let Some(level) = state.battery.battery_level {
            caps.push(DeviceCapability::Battery(vec![Battery {
                key: "battery".to_string(),
                label: "Battery".to_string(),
                level,
                status: if state.battery.battery_charging {
                    halod_protocol::types::BatteryStatus::Charging
                } else {
                    halod_protocol::types::BatteryStatus::Discharging
                },
            }]));
        }

        // Report rate
        if !state.report_rate.report_rate_options.is_empty() {
            let options: Vec<ChoiceOption> = if state.report_rate.report_rate_ext {
                let labels = ["8ms", "4ms", "2ms", "1ms", "500µs", "250µs", "125µs"];
                let ms_map: [u8; 7] = [8, 4, 2, 1, 0, 0, 0];
                ms_map
                    .iter()
                    .zip(labels.iter())
                    .enumerate()
                    .filter(|(i, _)| state.report_rate.report_rate_options.contains(&ms_map[*i]))
                    .map(|(i, (_, label))| ChoiceOption {
                        id: i.to_string(),
                        label: label.to_string(),
                    })
                    .collect()
            } else {
                state
                    .report_rate.report_rate_options
                    .iter()
                    .map(|&ms| ChoiceOption {
                        id: ms.to_string(),
                        label: format!("{ms}ms"),
                    })
                    .collect()
            };

            let selected = if state.report_rate.report_rate_ext {
                let ms_map: [u8; 7] = [8, 4, 2, 1, 0, 0, 0];
                ms_map
                    .iter()
                    .position(|&m| Some(m) == state.report_rate.report_rate_ms)
                    .unwrap_or(3)
            } else {
                state
                    .report_rate.report_rate_options
                    .iter()
                    .position(|&m| Some(m) == state.report_rate.report_rate_ms)
                    .unwrap_or(0)
            };

            caps.push(DeviceCapability::Choice(vec![Choice {
                key: "report_rate".to_string(),
                label: "Report Rate".to_string(),
                options,
                selected,
                category: String::new(),
                display: Default::default(),
            }]));
        }

        // DPI
        let has_dpi = !state.profile.profile_steps.is_empty()
            || state.dpi.dpi_current.is_some()
            || state.features.contains_key(&feature::ADJUSTABLE_DPI);
        if has_dpi {
            let host = state.profile.onboard_mode.map(is_host_mode).unwrap_or(false);
            let available_dpis = state.dpi.dpi_list.clone();
            let dpi = if host {
                DpiStatus {
                    steps: state.dpi.software_dpi_steps.clone(),
                    current_index: state.dpi.software_dpi_index,
                    current_dpi: state.dpi.host_current_dpi(),
                    available_dpis,
                    mode: DpiMode::Host,
                }
            } else {
                // Onboard mode — use the active profile steps; fall back to a
                // single-step list built from the live DPI when no profile.
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
            };
            caps.push(DeviceCapability::Dpi(dpi));
        }

        // Host / onboard mode
        if let Some(mode) = state.profile.onboard_mode {
            caps.push(DeviceCapability::Boolean(vec![Boolean {
                key: "host_mode".to_string(),
                label: "Host Mode".to_string(),
                value: is_host_mode(mode),
                read_only: false,
                category: "Profiles".to_string(),
            }]));
        }

        if state.features.contains_key(&feature::ONBOARD_PROFILES) {
            if let Some(mut profiles) = build_onboard_profiles(
                &state.profile.profile_dir,
                state.profile.profile_sector,
                state.profile.rom_profile_count,
            ) {
                // Cached profile_sector points at the fallback "first enabled"
                // slot in host mode so DPI steps stay populated — zero it here
                // so the UI's active_slot==0 ⇒ host-mode contract holds.
                let host = state.profile.onboard_mode.map(is_host_mode).unwrap_or(false);
                clear_active_slot_in_host_mode(&mut profiles, host);
                caps.push(DeviceCapability::OnboardProfiles(profiles));
            }
        }

        // RGB
        if !state.rgb.rgb_zones.is_empty() {
            if let Some(descriptor) = self.rgb_descriptor.get() {
                caps.push(DeviceCapability::Rgb(RgbStatus {
                    descriptor: descriptor.clone(),
                    state: state.rgb.rgb_state.clone(),
                    zone_transforms: self.rgb.zone_transforms(),
                    chainable_channels: Vec::new(),
                }));
            }
        }

        // Key remapper (only when a divert-capable feature is present)
        if !state.remap.reprog_cids.is_empty() || state.features.contains_key(&feature::REPROG_CONTROLS_V4) {
            let host_mode_active = state.profile.onboard_mode.map(is_host_mode).unwrap_or(false);
            caps.push(DeviceCapability::KeyRemap(KeyRemapStatus {
                buttons: state.remap.reprog_cids.clone(),
                mappings: state.remap.button_mappings.clone(),
                requires_host_mode: state.key_remap_requires_host_mode(),
                host_mode_active,
            }));
        }

        let device_type = self.profile.map(|p| p.device_type.clone()).unwrap_or(DeviceType::Other);

        WireDeviceBuilder::from_device(self)
            .name(state.name.clone())
            .device_type(device_type)
            .connected(state.online)
            .capabilities(caps)
            .connection_type(Some(if is_wired {
                ConnectionType::Wired
            } else {
                ConnectionType::Wireless
            }))
            .serial_number(self.hardware_serial())
            .build()
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![
            CapabilityRef::TransportSwitchable(self),
            CapabilityRef::Rgb(self),
            CapabilityRef::Boolean(self),
            CapabilityRef::Choice(self),
            CapabilityRef::Battery(self),
            CapabilityRef::Dpi(self),
            CapabilityRef::OnboardProfiles(self),
            CapabilityRef::KeyRemap(self),
        ]
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

}

// ── TransportSwitchable ───────────────────────────────────────────────────────

#[async_trait]
impl TransportSwitchable for LogitechDevice {
    async fn adopt_wired_transport(&self, path: &str, pid: u16) -> anyhow::Result<()> {
        // Resolve the wired device's short/long HID++ collections — Windows
        // splits the vendor interface in two; Linux collapses to one handle.
        let (short_path, long_path) = hidpp::collection::resolve_hidpp_paths(
            LOGITECH_VID,
            pid,
            WIRED_HIDPP_INTERFACE,
            path,
            None,
        )?;
        let new_hid = HidTransport::open_dual(
            &short_path,
            long_path.as_deref().unwrap_or(""),
            None,
            50,
            false,
        )?;
        let new_messenger = Arc::new(HidppMessenger::new(new_hid));

        {
            let mut t = self.transport.lock().await;
            // Save the wireless fallback before first wired adoption
            if t.wireless_fallback.is_none() && !t.is_wired {
                t.wireless_fallback = Some((Arc::clone(&t.messenger), t.devnum));
            }
            // Stop old listener if we were already in wired mode (re-adoption)
            if t.is_wired {
                t.messenger.stop_listener();
            }
            t.messenger = new_messenger;
            t.devnum = RECEIVER_DEVNUM;
            t.is_wired = true;
        }

        // Save RGB state before initialize(),
        // so the user's color survives the wired transport switch.
        let saved_rgb = self.state.lock().await.rgb.rgb_state.clone();

        // initialize() starts the listener and rediscovers all features
        match self.initialize().await {
            Ok(true) => log::info!("[{}] Re-initialized on wired transport", self.id),
            Ok(false) => log::warn!("[{}] Device offline after wired transport adoption", self.id),
            Err(e) => {
                if let Some(app) = self.app_ref.get().and_then(|w| w.upgrade()) {
                    notify::warn(
                        &app,
                        format!("Re-init failed on wired transport for {}", self.id),
                        e.to_string(),
                    )
                    .await;
                } else {
                    log::warn!("[{}] Re-init on wired transport failed: {e}", self.id);
                }
            }
        }

        if let Some(weak) = self.app_ref.get() {
            if let Some(app) = weak.upgrade() {
                // Re-apply persisted settings (host mode, button mappings, …) —
                // same as add_hid_device on first registration. The wired firmware
                // may come up in a different mode than what the user configured.
                let saved_state = {
                    let cfg = app.config.read().await;
                    cfg.active_profile_data().device_states.get(&self.id).cloned()
                };
                if let Some(state) = saved_state {
                    self.load_state(&state).await;
                }

                if let Some(rgb_state) = saved_rgb {
                    self.state.lock().await.rgb.rgb_state = Some(rgb_state);
                    self.restore_rgb_control().await;
                }

                let (messenger, devnum) = self.transport_snapshot().await;
                start_dpi_watcher(
                    messenger.notify_tx.subscribe(),
                    Some((Arc::downgrade(&messenger), devnum)),
                    Arc::clone(&self.state),
                    self.id.clone(),
                    app,
                );
            }
        }

        Ok(())
    }

    async fn revert_to_wireless(&self) -> bool {
        let mut t = self.transport.lock().await;
        let Some((wireless_messenger, wireless_devnum)) = t.wireless_fallback.take() else {
            return false;
        };
        t.messenger.stop_listener();
        t.messenger = wireless_messenger;
        t.devnum = wireless_devnum;
        t.is_wired = false;
        true
    }

    async fn is_using_wired_transport(&self) -> bool {
        self.transport.lock().await.is_wired
    }
}

// ── ChoiceCapability (report rate) ────────────────────────────────────────────

#[async_trait]
impl ChoiceCapability for LogitechDevice {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.choice_cache
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        self.choice_cache.record(key, selected);
        if key != "report_rate" {
            bail!("Unknown choice key: {key}");
        }
        let state = self.state.lock().await;
        let Some(&ms) = state.report_rate.report_rate_options.get(selected) else {
            bail!("Index {selected} out of range for report_rate options (len={})", state.report_rate.report_rate_options.len());
        };
        let ext = state.report_rate.report_rate_ext;
        let rate_idx = if ext {
            let ms_map: [u8; 7] = [8, 4, 2, 1, 0, 0, 0];
            ms_map.iter().position(|&m| m == ms).unwrap_or(3) as u8
        } else {
            ms
        };
        let features = state.features.clone();
        let original_mode = state.profile.onboard_mode;
        drop(state);

        log::debug!("[{}] set_choice report_rate: ms={ms} ext={ext} rate_idx={rate_idx}", self.id);

        let (msg, devnum) = self.transport_snapshot().await;

        // Setting the rate requires Host mode. Switch only if we weren't already
        // in Host mode, and restore the user's original mode afterwards so this
        // doesn't silently deactivate Host mode.
        let op_idx = features.get(&feature::ONBOARD_PROFILES).copied();
        let was_host = report_rate_was_host(original_mode);
        if let Some(op) = op_idx {
            if !was_host {
                let _ = msg.feature_request(devnum, op, 0x10, &[0x02]).await;
            }
        }

        let result = if ext {
            let idx = features.get(&feature::EXT_REPORT_RATE)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("EXT_REPORT_RATE feature not in table"))?;
            msg.feature_request(devnum, idx, 0x30, &[rate_idx]).await
                .map_err(|e| { log::warn!("[{}] EXT_REPORT_RATE set failed: {e}", self.id); e })
        } else {
            let idx = features.get(&feature::REPORT_RATE)
                .copied()
                .ok_or_else(|| anyhow::anyhow!("REPORT_RATE feature not in table"))?;
            msg.feature_request(devnum, idx, 0x20, &[ms]).await
                .map_err(|e| { log::warn!("[{}] REPORT_RATE set failed: {e}", self.id); e })
        };

        if let Some(op) = op_idx {
            if !was_host {
                let _ = msg.feature_request(devnum, op, 0x10, &[0x01]).await;
            }
        }

        result?;

        self.state.lock().await.report_rate.report_rate_ms = Some(ms);

        // If we transitioned back to Onboard mode the firmware reclaims LED
        // control, so re-enable SW control and re-apply the last known RGB.
        if !was_host {
            self.restore_rgb_control().await;
        }

        Ok(())
    }
}

// ── BooleanCapability ─────────────────────────────────────────────────────────

#[async_trait]
impl BooleanCapability for LogitechDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        let state = self.state.lock().await;
        let Some(mode) = state.profile.onboard_mode else { return Ok(vec![]); };
        Ok(vec![Boolean {
            key: "host_mode".to_string(),
            label: "Host Mode".to_string(),
            value: is_host_mode(mode),
            read_only: false,
            category: "Profiles".to_string(),
        }])
    }

    fn state_key(&self) -> &'static str { "boolean" }

    fn save_state(&self) -> serde_json::Value {
        // Sync read: try_lock returns None when the lock is contended; host_mode
        // is a rare write (user-initiated) so contention here is negligible.
        let host_mode = self
            .state
            .try_lock()
            .ok()
            .and_then(|s| s.profile.onboard_mode.map(is_host_mode));
        match host_mode {
            Some(v) => serde_json::json!({ "host_mode": v }),
            None    => serde_json::Value::Null,
        }
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        let want_host = match v.get("host_mode").and_then(|h| h.as_bool()) {
            Some(b) => b,
            None    => return,
        };
        // Only apply if the device's current mode differs from what we want.
        let current = self
            .state
            .lock()
            .await
            .profile
            .onboard_mode
            .map(is_host_mode);
        if current != Some(want_host) {
            if let Err(e) = self.set_boolean("host_mode", want_host).await {
                log::warn!("[{}] restoring host_mode={want_host} failed: {e}", self.id);
            }
        }
    }

    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        if key != "host_mode" {
            bail!("unknown boolean key: {key}");
        }
        let op_idx = {
            let state = self.state.lock().await;
            *state.features.get(&feature::ONBOARD_PROFILES)
                .ok_or_else(|| anyhow::anyhow!("ONBOARD_PROFILES not available"))?
        };
        let (msg, devnum) = self.transport_snapshot().await;
        // 0x02 = host mode (software controls), 0x01 = onboard mode (firmware profiles)
        let mode_byte = if value { 0x02u8 } else { 0x01u8 };
        msg.feature_request(devnum, op_idx, 0x10, &[mode_byte]).await?;
        self.state.lock().await.profile.onboard_mode = Some(mode_byte);

        // Reload DPI + onboard-profile state: switching modes changes which
        // profile the firmware drives, so re-read the device profile to refresh
        // the reported DPI steps and profile directory for the new mode.
        {
            let mut state = self.state.lock().await;
            let features = state.features.clone();
            self.init_onboard(&features, &mut state).await;
            self.init_dpi(&features, &mut state).await;
        }

        // When switching to host mode, re-enable SW LED control and reapply RGB.
        if value {
            self.restore_rgb_control().await;
        }

        if let Some(weak) = self.app_ref.get() {
            if let Some(app) = weak.upgrade() {
                broadcast_state(app).await;
            }
        }
        Ok(())
    }
}

// ── BatteryCapability ─────────────────────────────────────────────────────────

#[async_trait]
impl BatteryCapability for LogitechDevice {
    async fn get_batteries(&self) -> Result<Vec<Battery>> {
        let state = self.state.lock().await;
        if let Some(level) = state.battery.battery_level {
            Ok(vec![Battery {
                key: "battery".to_string(),
                label: "Battery".to_string(),
                level,
                status: if state.battery.battery_charging {
                    halod_protocol::types::BatteryStatus::Charging
                } else {
                    halod_protocol::types::BatteryStatus::Discharging
                },
            }])
        } else {
            Ok(vec![])
        }
    }
}

fn report_rate_was_host(original_mode: Option<u8>) -> bool {
    original_mode.map(is_host_mode).unwrap_or(false)
}

fn clear_active_slot_in_host_mode(profiles: &mut OnboardProfiles, host_mode: bool) {
    if !host_mode {
        return;
    }
    profiles.active_slot = 0;
    for slot in &mut profiles.slots {
        slot.active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{OnboardProfileSlot, OnboardProfiles};

    #[test]
    fn report_rate_skips_dance_when_already_host() {
        assert!(report_rate_was_host(Some(0x02)));
    }

    #[test]
    fn report_rate_does_dance_when_onboard() {
        assert!(!report_rate_was_host(Some(0x01)));
    }

    #[test]
    fn report_rate_does_dance_when_mode_unknown() {
        assert!(!report_rate_was_host(None));
    }

    fn sample_profiles(active_slot: u8) -> OnboardProfiles {
        OnboardProfiles {
            active_slot,
            slots: vec![
                OnboardProfileSlot { index: 1, enabled: true, active: active_slot == 1, has_rom_default: true },
                OnboardProfileSlot { index: 2, enabled: true, active: active_slot == 2, has_rom_default: true },
            ],
        }
    }

    #[test]
    fn clear_active_slot_zeroes_when_host_mode() {
        let mut p = sample_profiles(1);
        clear_active_slot_in_host_mode(&mut p, true);
        assert_eq!(p.active_slot, 0);
        assert!(p.slots.iter().all(|s| !s.active));
    }

    #[test]
    fn clear_active_slot_preserves_when_onboard_mode() {
        let mut p = sample_profiles(2);
        clear_active_slot_in_host_mode(&mut p, false);
        assert_eq!(p.active_slot, 2);
        assert!(p.slots.iter().any(|s| s.active));
    }
}
