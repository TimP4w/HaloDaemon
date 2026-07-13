// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// Generic Logitech HID++ 2.0 device — feature-driven capability composition.
///
/// Reference: Solaar (GPL-2.0-or-later) — hidpp20.py, settings_templates.py
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::{
    vendors::generic::devices::common::TaskHandle,
    vendors::logitech::devices::generic::onboard::start_dpi_watcher,
    vendors::logitech::devices::generic::profile::{
        build_device_id, find_profile, LogitechDeviceProfile, LOGITECH_VID, WIRED_HIDPP_INTERFACE,
    },
    vendors::logitech::devices::generic::state::LogitechDeviceState,
    vendors::logitech::protocols::hidpp::{
        self,
        v2::{FeatureTable, Hidpp20},
        HidppChannel, PkWriteCoordinator, RECEIVER_DEVNUM,
    },
    CapabilityRef, ChoiceStateCache, ConnectionCapability, Device, RangeStateCache, RgbStateSlot,
    TransportMode, TransportSwitchable, VisibilitySlot,
};
use halod_shared::types::{
    ConnectionStatus, ConnectionType, DeviceType, OnboardProfiles, RgbDescriptor,
};

/// Active transport for a LogitechDevice. Swapped atomically when the device
/// transitions between wireless (via receiver) and wired (direct USB).
pub(super) struct LogitechTransport {
    pub(super) messenger: Arc<dyn HidppChannel>,
    pub(super) devnum: u8,
    /// When wired, the device owns its HID listener directly.
    pub(super) is_wired: bool,
    /// Wireless transport, kept only while temporarily in wired mode.
    pub(super) wireless_fallback: Option<(Arc<dyn HidppChannel>, u8)>,
    /// Per-receiver coordinator for per-key RGB; `None` for wired devices.
    pub(super) coordinator: Option<Arc<PkWriteCoordinator>>,
}

type TransportHook = Box<dyn Fn(TransportMode) + Send + Sync + 'static>;

pub struct LogitechDevice {
    pub(super) id: String,
    pub(super) profile: Option<&'static LogitechDeviceProfile>,
    /// Static display name used when no profile names the model and the device
    /// doesn't advertise the DEVICE_NAME feature (e.g. headsets). A plain label.
    pub(super) model_name: &'static str,
    /// Device kind declared at registration (next to vid/pid/name). The kind is
    /// never inferred from the feature table — see [`profile`] for the tables.
    pub(super) device_type: DeviceType,
    pub(super) transport: Mutex<LogitechTransport>,
    pub(super) state: Arc<Mutex<LogitechDeviceState>>,
    pub(super) rgb_descriptor: std::sync::OnceLock<RgbDescriptor>,
    /// Callback fired after a transport switch (direct ↔ hub-mediated).
    /// Registered by `after_register` so the application layer can re-apply
    /// config without the device holding an `AppState` reference.
    transport_hook: std::sync::Mutex<Option<TransportHook>>,
    pub(super) rgb: RgbStateSlot,
    pub(super) visibility: VisibilitySlot,
    /// Choice (report rate) state cache — required by `ChoiceCapability`.
    pub(super) choice_cache: ChoiceStateCache,
    /// Range (sidetone) state cache — required by `RangeCapability`.
    pub(super) range_cache: RangeStateCache,
    /// Background battery re-poll task — present only for voltage-battery devices.
    pub(super) poll_task: Mutex<Option<TaskHandle>>,
}

impl LogitechDevice {
    /// Whether this device is a keyboard, per its static profile descriptor.
    /// Used to branch RGB init / per-LED batching without inline topology checks.
    pub(super) fn is_keyboard(&self) -> bool {
        self.profile
            .map(|p| matches!(p.device_type, DeviceType::Keyboard))
            .unwrap_or(false)
    }

    /// Assemble a device from its id, optional profile, and ready transport.
    /// Every constructor funnels through here so the cache/slot fields are
    /// initialised in exactly one place.
    fn from_parts(
        id: String,
        profile: Option<&'static LogitechDeviceProfile>,
        model_name: &'static str,
        device_type: DeviceType,
        transport: LogitechTransport,
    ) -> Self {
        Self {
            id,
            profile,
            model_name,
            device_type,
            transport: Mutex::new(transport),
            state: Arc::new(Mutex::new(LogitechDeviceState::default())),
            rgb_descriptor: std::sync::OnceLock::new(),
            transport_hook: std::sync::Mutex::new(None),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            choice_cache: ChoiceStateCache::default(),
            range_cache: RangeStateCache::default(),
            poll_task: Mutex::new(None),
        }
    }

    /// Constructor for a wireless device reached through a receiver. `device_type`
    /// is declared at registration (see [`profile`]), never inferred.
    /// `coordinator` is the shared per-key write drain owned by the receiver.
    pub fn new(
        devnum: u8,
        wpid: u16,
        serial: Option<&str>,
        device_type: DeviceType,
        messenger: Arc<dyn HidppChannel>,
        coordinator: Arc<PkWriteCoordinator>,
    ) -> Self {
        let id = build_device_id(serial, wpid, devnum as usize);
        Self::from_parts(
            id,
            find_profile(wpid),
            "Logitech Device",
            device_type,
            LogitechTransport {
                messenger,
                devnum,
                is_wired: false,
                wireless_fallback: None,
                coordinator: Some(coordinator),
            },
        )
    }

    /// Constructor used by inventory descriptors that don't have a coordinator
    /// available at construction time (e.g. `DiscoveryHandle::LogitechSlot`).
    /// Per-key RGB writes will fall back to the direct-write path.
    pub fn new_without_coordinator(
        devnum: u8,
        wpid: u16,
        serial: Option<&str>,
        device_type: DeviceType,
        messenger: Arc<dyn HidppChannel>,
    ) -> Self {
        let id = build_device_id(serial, wpid, devnum as usize);
        Self::from_parts(
            id,
            find_profile(wpid),
            "Logitech Device",
            device_type,
            LogitechTransport {
                messenger,
                devnum,
                is_wired: false,
                wireless_fallback: None,
                coordinator: None,
            },
        )
    }

    /// Constructor for a directly-connected (USB) device. `interface` selects the
    /// HID++ collection; `report` selects whether the interface exposes the split
    /// short/long collections or only the long report. The messenger is built by
    /// the protocol's [`hidpp::open_wired`]; both run at `devnum = 0xFF` and own
    /// their HID listener. `device_type` is declared at registration.
    #[allow(clippy::too_many_arguments)] // mirrors the HID identity/collection tuple
    pub fn new_direct(
        path: &str,
        serial: Option<&str>,
        pid: u16,
        idx: usize,
        interface: i32,
        report: hidpp::DirectReport,
        model_name: &'static str,
        device_type: DeviceType,
    ) -> anyhow::Result<Self> {
        let messenger: Arc<dyn HidppChannel> =
            hidpp::open_wired(LOGITECH_VID, pid, interface, path, serial, report)?;
        let id = build_device_id(serial, pid, idx);
        Ok(Self::from_parts(
            id,
            find_profile(pid),
            model_name,
            device_type,
            LogitechTransport {
                messenger,
                devnum: RECEIVER_DEVNUM,
                is_wired: true,
                wireless_fallback: None,
                coordinator: None,
            },
        ))
    }

    /// Register a callback that fires after every transport switch (direct ↔
    /// hub-mediated). Called by `after_register` so the application layer can
    /// restart the DPI watcher and re-apply persisted config.
    pub fn set_transport_hook(&self, hook: impl Fn(TransportMode) + Send + Sync + 'static) {
        *self.transport_hook.lock().unwrap() = Some(Box::new(hook));
    }

    pub async fn devnum(&self) -> u8 {
        self.transport.lock().await.devnum
    }

    pub(super) async fn transport_snapshot(&self) -> (Arc<dyn HidppChannel>, u8) {
        let t = self.transport.lock().await;
        (Arc::clone(&t.messenger), t.devnum)
    }

    /// A typed HID++ 2.0 handle bound to the current transport and the device's
    /// cached feature table. Used by capability methods after `initialize`.
    pub(super) async fn hidpp2(&self) -> Hidpp20 {
        let features: Arc<HashMap<u16, u8>> = Arc::clone(&self.state.lock().await.features);
        let (msg, devnum) = self.transport_snapshot().await;
        Hidpp20::new(msg, devnum, (*features).clone())
    }

    /// A handle bound to an explicit feature table — for the `init_*` path, where
    /// the table is still being assembled and not yet stored in state.
    pub(super) async fn hidpp2_with(&self, features: &FeatureTable) -> Hidpp20 {
        let (msg, devnum) = self.transport_snapshot().await;
        Hidpp20::new(msg, devnum, features.clone())
    }
}

#[async_trait]
impl Device for LogitechDevice {
    fn id(&self) -> &str {
        &self.id
    }

    fn hardware_serial(&self) -> Option<String> {
        // ID formats:
        //   "logitech_<8CHAR_HEX>"        — canonical (serial known, same for wired + wireless)
        //   "logitech_<PID>_<fallback>"   — no serial available
        let parts: Vec<&str> = self.id.splitn(3, '_').collect();
        let candidate = match parts.len() {
            2 => parts[1], // "logitech_<serial>"
            3 => parts[2], // "logitech_<pid>_<something>" (legacy fallback format) // TODO: remove fallback
            _ => return None,
        };
        if candidate.len() == 8 && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(candidate.to_uppercase())
        } else {
            None
        }
    }

    fn name(&self) -> &str {
        self.profile.map(|p| p.name).unwrap_or(self.model_name)
    }

    fn vendor(&self) -> &str {
        "Logitech"
    }

    fn model(&self) -> &str {
        self.profile.map(|p| p.name).unwrap_or(self.model_name)
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
        let features = match self.init_features().await {
            Ok(f) => f,
            Err(e) => {
                log::info!("[{}] Offline at init ({e})", self.id);
                return Ok(false);
            }
        };

        let mut state = self.state.lock().await;
        state.online = true;

        // Publish the feature table before the capability inits: init_onboard_profile
        // resolves HID++ indices via state.features, so it must be set here, not at
        // the end, or onboard reads fail with "feature unavailable" at cold start.
        state.features = Arc::new(features.clone());

        // Prefer the device's own DEVICE_NAME; fall back to the static label for
        // devices that don't advertise it (e.g. headsets).
        state.name = self
            .init_name(&features)
            .await
            .unwrap_or_else(|| self.model_name.to_string());
        state.firmware = self.init_firmware(&features).await;

        self.init_battery(&features, &mut state).await;
        self.init_report_rate(&features, &mut state).await;
        self.init_onboard(&features, &mut state).await;
        self.init_dpi(&features, &mut state).await;
        self.init_reprog_controls(&features, &mut state).await;
        self.init_bitmap_buttons(&features, &mut state).await;
        self.seed_default_button_mappings(&mut state);
        let keyboard_layout = self.init_keyboard_layout(&features).await;
        self.init_rgb(&features, &keyboard_layout, &mut state).await;
        self.init_audio(&features, &mut state).await;
        self.init_hires_wheel(&features, &mut state).await;
        self.init_fn_inversion(&features, &mut state).await;
        self.init_brightness(&features, &mut state).await;

        log::info!(
            "[{}] {}: battery={:?}%, rate={:?}, dpi={:?}, steps={:?}",
            self.id,
            state.name,
            state.battery.battery_level,
            state.report_rate.current,
            state.dpi.dpi_current,
            state.profile.profile_steps,
        );

        // Re-apply any persisted button mappings (enables GKEY software control
        // when mappings exist). Drop the state guard first — apply_reprog_mappings
        // locks it itself.
        drop(state);
        self.apply_reprog_mappings().await;
        // Voltage-battery devices (e.g. headsets) have no battery notifications;
        // re-poll on a timer. Notification-driven (UNIFIED) devices skip this.
        self.start_battery_poll().await;

        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
        let t = self.transport.lock().await;
        if t.is_wired {
            t.messenger.stop_listener();
        }
    }

    fn as_post_register_hook(&self) -> Option<&dyn crate::drivers::PostRegisterHook> {
        Some(self)
    }

    fn debug_info_extra(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        if let Some(p) = self.profile {
            if let Some(wpid) = p.wpid {
                out.push(("wpid".to_string(), format!("{wpid:04x}")));
            }
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
                    if t.is_wired {
                        "wired".to_string()
                    } else {
                        "wireless".to_string()
                    },
                ));
                out.push(("devnum".to_string(), format!("0x{:02x}", t.devnum)));
                if t.wireless_fallback.is_some() {
                    out.push(("wireless_fallback".to_string(), "present".to_string()));
                }
            }
            Err(_) => {
                out.push((
                    "connection".to_string(),
                    "unknown (transport busy)".to_string(),
                ));
            }
        }
        if let Ok(state) = self.state.try_lock() {
            if let Some(fw) = &state.firmware {
                out.push(("firmware".to_string(), fw.clone()));
            }
            if let Some(ws) = &state.wireless_status {
                out.push((
                    "wireless_status".to_string(),
                    format!(
                        "{} (reason {})",
                        if ws.reconnected {
                            "reconnected"
                        } else {
                            "disconnected"
                        },
                        ws.reason
                    ),
                ));
            }
        }
        out
    }

    fn wire_device_type(&self) -> DeviceType {
        // Declared at registration: a profile names it, else the type passed to
        // the constructor. Never inferred from the feature table.
        self.profile
            .map(|p| p.device_type)
            .unwrap_or(self.device_type)
    }

    async fn wire_connection_type(&self) -> Option<ConnectionType> {
        Some(if self.transport.lock().await.is_wired {
            ConnectionType::Wired
        } else {
            ConnectionType::Wireless
        })
    }

    fn wire_serial_number(&self) -> Option<String> {
        self.hardware_serial()
    }

    async fn wire_device_connected(&self) -> bool {
        self.state.lock().await.online
    }

    async fn wire_device_name(&self) -> String {
        self.state.lock().await.name.clone()
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        // List every supported capability; each to_wire() gates on live state and
        // returns None when absent, so panels appear as state populates rather than
        // being frozen at the end of initialize().
        vec![
            CapabilityRef::TransportSwitchable(self),
            CapabilityRef::Battery(self),
            CapabilityRef::Connection(self),
            CapabilityRef::Choice(self),
            CapabilityRef::Rgb(self),
            CapabilityRef::Boolean(self),
            CapabilityRef::Dpi(self),
            CapabilityRef::OnboardProfiles(self),
            CapabilityRef::KeyRemap(self),
            CapabilityRef::Range(self),
            CapabilityRef::Equalizer(self),
        ]
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        // Best-effort: this is a sync trait method, so a transport switch
        // mid-read just means a missed sample rather than a blocked call —
        // the GUI polls again in well under a second.
        self.transport.try_lock().ok()?.messenger.rate_status()
    }
}

#[async_trait]
impl crate::drivers::PostRegisterHook for LogitechDevice {
    async fn on_registered(&self, app: std::sync::Arc<crate::state::AppState>) {
        let (is_wired, messenger, devnum) = {
            let t = self.transport.lock().await;
            (t.is_wired, Arc::clone(&t.messenger), t.devnum)
        };

        // Register the transport-switch hook so connect_direct() /
        // disconnect_direct() can trigger config re-apply without the device
        // holding a direct reference to AppState.
        let app2 = Arc::clone(&app);
        let id = self.id.clone();
        self.set_transport_hook(move |mode| match mode {
            TransportMode::Direct => {
                let app = Arc::clone(&app2);
                let id = id.clone();
                tokio::spawn(async move {
                    let saved_state = {
                        let cfg = app.config.read().await;
                        Some(cfg.effective_device_state(&id)).filter(|v| !v.is_null())
                    };
                    if let Some(s) = saved_state {
                        let dev_arc = {
                            let devs = app.devices.read().await;
                            devs.iter().find(|d| d.id() == id).cloned()
                        };
                        if let Some(d) = dev_arc {
                            d.load_state(&s).await;
                        }
                    }
                });
            }
            TransportMode::HubMediated => {
                // DPI watcher exits naturally when the direct messenger's
                // weak ref drops (the messenger is stopped and replaced in
                // disconnect_direct).
            }
        });

        if is_wired {
            start_dpi_watcher(
                messenger.subscribe_notifications(),
                Some((Arc::downgrade(&messenger), devnum)),
                Arc::clone(&self.state),
                self.id.clone(),
                Some(app),
            );
        }
    }
}

#[async_trait]
impl TransportSwitchable for LogitechDevice {
    async fn connect_direct(
        &self,
        path: &str,
        pid: u16,
        app: &Arc<crate::state::AppState>,
    ) -> anyhow::Result<()> {
        // Standard wired devices use the split short/long HID++ collections.
        let new_messenger: Arc<dyn HidppChannel> = hidpp::open_wired(
            LOGITECH_VID,
            pid,
            WIRED_HIDPP_INTERFACE,
            path,
            None,
            hidpp::DirectReport::ShortLong,
        )?;

        {
            let mut t = self.transport.lock().await;
            // Save the wireless fallback before first direct adoption.
            if t.wireless_fallback.is_none() && !t.is_wired {
                t.wireless_fallback = Some((Arc::clone(&t.messenger), t.devnum));
            }
            // Stop old listener if we were already in direct mode (re-adoption).
            if t.is_wired {
                t.messenger.stop_listener();
            }
            t.messenger = new_messenger;
            t.devnum = RECEIVER_DEVNUM;
            t.is_wired = true;
        }

        // Save RGB state before initialize(),
        // so the user's color survives the transport switch.
        let saved_rgb = self.state.lock().await.rgb.rgb_state.clone();

        // initialize() starts the listener and rediscovers all features.
        match self.initialize().await {
            Ok(true) => log::info!("[{}] Re-initialized on direct transport", self.id),
            Ok(false) => log::warn!(
                "[{}] Device offline after direct transport adoption",
                self.id
            ),
            Err(e) => {
                log::warn!("[{}] Re-init on direct transport failed: {e}", self.id);
            }
        }

        // Restart the DPI watcher on the new transport, app-aware: without the
        // AppState the watcher would silently drop button notifications (the
        // key-remap engine never sees them) and never broadcast — which breaks
        // host-mode remapping after a runtime wireless→wired switch.
        let (messenger, devnum) = self.transport_snapshot().await;
        start_dpi_watcher(
            messenger.subscribe_notifications(),
            Some((Arc::downgrade(&messenger), devnum)),
            Arc::clone(&self.state),
            self.id.clone(),
            Some(Arc::clone(app)),
        );

        // Restore RGB after the transport switch.
        if let Some(rgb_state) = saved_rgb {
            self.state.lock().await.rgb.rgb_state = Some(rgb_state);
            self.restore_rgb_control().await;
        }

        // Fire the transport hook so the application layer can re-apply config.
        if let Some(hook) = self.transport_hook.lock().unwrap().as_ref() {
            hook(TransportMode::Direct);
        }

        Ok(())
    }

    async fn disconnect_direct(&self) -> bool {
        let mut t = self.transport.lock().await;
        let Some((wireless_messenger, wireless_devnum)) = t.wireless_fallback.take() else {
            return false;
        };
        t.messenger.stop_listener();
        t.messenger = wireless_messenger;
        t.devnum = wireless_devnum;
        t.is_wired = false;
        drop(t);

        // Notify the application layer that the device reverted to hub-mediated.
        if let Some(hook) = self.transport_hook.lock().unwrap().as_ref() {
            hook(TransportMode::HubMediated);
        }
        true
    }

    async fn is_direct(&self) -> bool {
        self.transport.lock().await.is_wired
    }
}

pub(super) fn is_wireless_capable(has_wpid: bool, is_wired: bool, has_wds_feature: bool) -> bool {
    has_wpid || !is_wired || has_wds_feature
}

#[async_trait]
impl ConnectionCapability for LogitechDevice {
    async fn connection_status(&self) -> Option<ConnectionStatus> {
        let is_wired = self.transport.lock().await.is_wired;
        let has_wds = self
            .state
            .lock()
            .await
            .features
            .contains_key(&feature::WIRELESS_DEVICE_STATUS);
        is_wireless_capable(
            self.profile.and_then(|p| p.wpid).is_some(),
            is_wired,
            has_wds,
        )
        .then_some(ConnectionStatus {
            connection_type: if is_wired {
                ConnectionType::Wired
            } else {
                ConnectionType::Wireless
            },
        })
    }
}

pub(super) fn clear_active_slot_in_host_mode(profiles: &mut OnboardProfiles, host_mode: bool) {
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
    use halod_shared::types::{OnboardProfileSlot, OnboardProfiles};

    fn sample_profiles(active_slot: u8) -> OnboardProfiles {
        OnboardProfiles {
            active_slot,
            slots: vec![
                OnboardProfileSlot {
                    index: 1,
                    enabled: true,
                    active: active_slot == 1,
                    has_rom_default: true,
                },
                OnboardProfileSlot {
                    index: 2,
                    enabled: true,
                    active: active_slot == 2,
                    has_rom_default: true,
                },
            ],
        }
    }

    #[test]
    fn wireless_capable_true_on_wpid_link_or_feature() {
        // Wired-only device with no wireless identity and no WDS feature.
        assert!(!is_wireless_capable(false, true, false));
        // Any one signal is enough.
        assert!(is_wireless_capable(true, true, false)); // has wpid
        assert!(is_wireless_capable(false, false, false)); // currently wireless
        assert!(is_wireless_capable(false, true, true)); // advertises WDS
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
