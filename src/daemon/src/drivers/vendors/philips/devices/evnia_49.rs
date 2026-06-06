mod inner {
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use halod_protocol::types::{
        Action, Boolean, Choice, ChoiceOption, DeviceCapability, DeviceType, Range, WireDevice,
    };
    use std::sync::{
        atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
        Arc,
    };

    use crate::{
        discovery::{DeviceDescriptor, DiscoveryHandle},
        drivers::{
            vendors::generic::devices::common::WireDeviceBuilder,
            vendors::philips::protocols::philips_evnia::{build_extended_set, build_write, PhilipsEvnia49Protocol},
            ActionCapability, BooleanCapability, CapabilityRef, ChoiceCapability,
            ChoiceStateCache, Device, RangeCapability, RangeStateCache,
        },
    };

    const MCCS_VID: u16 = 0x2109;
    const MCCS_PID: u16 = 0x8884;

    // --- Input-source encoding -------------------------------------------------
    // VCP 0x60 carries the standard MCCS port code in the low byte; the Philips
    // unit always reports 0x35 in the high byte. We expose four physical ports.
    const INPUT_PORTS: &[(u8, &str, &str)] = &[
        (0x0F, "dp1", "DisplayPort 1"),
        (0x10, "dp2", "DisplayPort 2"),
        (0x11, "hdmi1", "HDMI 1"),
        (0x12, "hdmi2", "HDMI 2"),
    ];

    fn input_index_from_raw(raw: u16) -> usize {
        let port = (raw & 0xFF) as u8;
        INPUT_PORTS
            .iter()
            .position(|(code, _, _)| *code == port)
            .unwrap_or(0)
    }

    // VCP 0xCC (OSD Language) values that the unit exposes in its capability
    // string. Indices match the value the monitor returns.
    const OSD_LANGUAGES: &[(u8, &str)] = &[
        (0x01, "Chinese (Traditional)"),
        (0x02, "English"),
        (0x03, "French"),
        (0x04, "German"),
        (0x05, "Italian"),
        (0x06, "Japanese"),
        (0x07, "Korean"),
        (0x08, "Portuguese"),
        (0x09, "Russian"),
        (0x0A, "Spanish"),
        (0x0B, "Chinese (Simplified)"),
        (0x0C, "Dutch"),
        (0x0D, "Czech"),
        (0x0E, "Polish"),
        (0x12, "Hungarian"),
        (0x14, "Turkish"),
        (0x16, "Brazilian Portuguese"),
        (0x17, "Finnish"),
        (0x1A, "Greek"),
        (0x1E, "Ukrainian"),
        (0x24, "Swedish"),
    ];

    fn language_index_from_raw(raw: u16) -> usize {
        let code = (raw & 0xFF) as u8;
        OSD_LANGUAGES
            .iter()
            .position(|(c, _)| *c == code)
            .unwrap_or(0)
    }

    // VCP 0xDC is the unified SmartImage selector. SDR and HDR presets share
    // one VCP; the OSD only shows the subset that is valid in the current
    // input mode. We expose all known entries so the UI matches the OSD in
    // either mode. The 18th cap-string entry (0xE2) is intentionally omitted
    // — its OSD label is unknown.
    const SMART_IMAGE_MODES: &[(u8, &str)] = &[
        (0x00, "Standard"),
        (0x01, "FPS"),
        (0x03, "Movie"),
        (0x04, "Game 1"),
        (0x05, "Game 2"),
        (0x06, "Racing"),
        (0x07, "RTS"),
        (0x08, "Economy"),
        (0x0B, "LowBlue Mode"),
        (0x0E, "EasyRead"),
        (0x11, "Console Mode"),
        (0x21, "HDR Game"),
        (0x22, "HDR Movie"),
        (0x23, "HDR Vivid"),
        (0x30, "HDR True Black"),
        (0x24, "HDR Personal"),
        (0x20, "HDR Off"),
    ];

    /// Indices into SMART_IMAGE_MODES that represent an "HDR is being applied"
    /// state. Everything from `HDR Game` (index 11) through `HDR Personal`
    /// (index 15); the final `HDR Off` entry is in the HDR menu but signals
    /// that HDR processing is disabled.
    const HDR_ACTIVE_INDEX_RANGE: std::ops::RangeInclusive<usize> = 11..=15;

    fn smart_image_is_hdr_active(idx: usize) -> bool {
        HDR_ACTIVE_INDEX_RANGE.contains(&idx)
    }

    // VCP 0x14 (Color Temperature). The Philips unit advertises a non-MCCS
    // set of values; pulled straight from the capability string + OSD labels.
    const COLOR_TEMPERATURES: &[(u8, &str)] = &[
        (0x02, "Native"),
        (0x04, "5000K"),
        (0x05, "6500K"),
        (0x06, "7500K"),
        (0x07, "8200K"),
        (0x08, "9300K"),
        (0x0A, "11500K"),
        (0x0D, "Preset"),
    ];

    // VCP 0x72 (Select Gamma). Cap string lists exactly five values; this
    // table is in OSD order.
    const GAMMA_VALUES: &[(u8, &str)] = &[
        (0x50, "1.0"),
        (0x64, "2.0"),
        (0x78, "2.2"),
        (0x8C, "2.4"),
        (0xA0, "2.6"),
    ];

    // E2A034 (Pixel Orbiting) uses a non-contiguous value set: the cap string
    // lists `(00 02 03 04)` and OSD labels map as below.
    const PIXEL_ORBITING_OPTIONS: &[(u8, &str)] = &[
        (0x00, "Off"),
        (0x02, "Slow"),
        (0x03, "Normal"),
        (0x04, "Fast"),
    ];

    // E2A035 (Screen Saver) — cap string `(00 02 03)`.
    const SCREEN_SAVER_OPTIONS: &[(u8, &str)] = &[
        (0x00, "Off"),
        (0x02, "Slow"),
        (0x03, "Fast"),
    ];

    fn lookup_index(table: &[(u8, &str)], raw: u16) -> usize {
        let code = (raw & 0xFF) as u8;
        table.iter().position(|(c, _)| *c == code).unwrap_or(0)
    }

    fn choices_from_table(table: &[(u8, &str)]) -> Vec<ChoiceOption> {
        table
            .iter()
            .enumerate()
            .map(|(i, (_, label))| ChoiceOption { id: i.to_string(), label: (*label).into() })
            .collect()
    }

    /// Device-info strings read from the monitor once at initialize().
    #[derive(Default, Clone)]
    struct DeviceInfo {
        model: Option<String>,
        firmware: Option<String>,
        panel_variant: Option<String>,
        panel_id: Option<String>,
        serial: Option<String>,
    }

    pub struct PhilipsEvnia49 {
        id: String,
        protocol: PhilipsEvnia49Protocol,
        info: std::sync::Mutex<DeviceInfo>,
        brightness: AtomicI32,
        contrast: AtomicI32,
        light_enhancement: AtomicI32,
        color_enhancement: AtomicI32,
        osd_h_position: AtomicI32,
        osd_v_position: AtomicI32,
        volume: AtomicI32,
        power_led: AtomicI32,
        sharpness: AtomicI32,
        adaptive_sync: AtomicBool,
        low_input_lag: AtomicBool,
        audio_mute: AtomicBool,
        resolution_notice: AtomicBool,
        usb_standby: AtomicBool,
        smart_power: AtomicBool,
        cec: AtomicBool,
        auto_warning: AtomicBool,
        srgb: AtomicBool,
        crosshair: AtomicUsize,
        smart_response: AtomicUsize,
        osd_transparency: AtomicUsize,
        osd_timeout: AtomicUsize,
        input_source: AtomicUsize,
        osd_language: AtomicUsize,
        usb_c_setting: AtomicUsize,
        kvm: AtomicUsize,
        smart_image: AtomicUsize,
        pixel_orbiting: AtomicUsize,
        screen_saver: AtomicUsize,
        gamma: AtomicUsize,
        color_temperature: AtomicUsize,
        range_cache: RangeStateCache,
        choice_cache: ChoiceStateCache,
    }

    impl PhilipsEvnia49 {
        pub fn new() -> Self {
            Self {
                id: format!("philips_evnia_49_{:04x}_{:04x}", MCCS_VID, MCCS_PID),
                protocol: PhilipsEvnia49Protocol::new(),
                info: std::sync::Mutex::new(DeviceInfo::default()),
                brightness: AtomicI32::new(50),
                contrast: AtomicI32::new(50),
                light_enhancement: AtomicI32::new(0),
                color_enhancement: AtomicI32::new(0),
                osd_h_position: AtomicI32::new(50),
                osd_v_position: AtomicI32::new(50),
                volume: AtomicI32::new(0),
                power_led: AtomicI32::new(2),
                sharpness: AtomicI32::new(50),
                adaptive_sync: AtomicBool::new(false),
                low_input_lag: AtomicBool::new(false),
                audio_mute: AtomicBool::new(false),
                resolution_notice: AtomicBool::new(false),
                usb_standby: AtomicBool::new(false),
                smart_power: AtomicBool::new(false),
                cec: AtomicBool::new(false),
                auto_warning: AtomicBool::new(false),
                srgb: AtomicBool::new(false),
                crosshair: AtomicUsize::new(0),
                smart_response: AtomicUsize::new(0),
                osd_transparency: AtomicUsize::new(0),
                osd_timeout: AtomicUsize::new(0),
                input_source: AtomicUsize::new(0),
                osd_language: AtomicUsize::new(1),
                usb_c_setting: AtomicUsize::new(0),
                kvm: AtomicUsize::new(0),
                smart_image: AtomicUsize::new(0),
                pixel_orbiting: AtomicUsize::new(0),
                screen_saver: AtomicUsize::new(0),
                gamma: AtomicUsize::new(2), // 2.2 — the most common default
                color_temperature: AtomicUsize::new(0),
                range_cache: RangeStateCache::default(),
                choice_cache: ChoiceStateCache::default(),
            }
        }

        /// Read all known device-info strings once. The set is small and the
        /// strings never change, so we only call this from `initialize()`.
        async fn read_device_info(&self) {
            let mut info = DeviceInfo::default();
            let probes: &[(&str, [u8; 4], fn(&mut DeviceInfo, String))] = &[
                ("model", [0xE9, 0x0D, 0x00, 0x00], |i, s| i.model = Some(s)),
                ("firmware", [0xE1, 0xE6, 0x06, 0x00], |i, s| i.firmware = Some(s)),
                ("panel_variant", [0xE1, 0xE6, 0x1D, 0x00], |i, s| i.panel_variant = Some(s)),
                ("panel_id", [0xE1, 0xE8, 0x00, 0x00], |i, s| i.panel_id = Some(s)),
                ("serial", [0xEF, 0x13, 0x00, 0x20], |i, s| i.serial = Some(s)),
            ];
            for (label, addr, setter) in probes {
                match self.protocol.get_info(*addr).await {
                    Ok(s) if !s.is_empty() => {
                        log::debug!("PhilipsEvnia49: info {} = {:?}", label, s);
                        setter(&mut info, s);
                    }
                    Ok(_) => log::debug!("PhilipsEvnia49: info {} returned empty string", label),
                    Err(e) => log::warn!("PhilipsEvnia49: info {} read failed: {}", label, e),
                }
            }
            *self.info.lock().unwrap() = info;
        }

        /// Read every VCP the UI surfaces and populate the atomic caches. Any
        /// individual read that fails is logged and skipped — partial state is
        /// better than refusing to connect because one VCP happens to be
        /// disabled in the current picture mode (e.g. brightness is locked when
        /// SmartImage is not Personal).
        async fn refresh_from_monitor(&self) {
            macro_rules! try_read {
                ($label:expr, $expr:expr) => {
                    match $expr.await {
                        Ok(v) => Some(v),
                        Err(e) => {
                            log::warn!("PhilipsEvnia49: read {} failed: {}", $label, e);
                            None
                        }
                    }
                };
            }

            if let Some(v) = try_read!("brightness", self.protocol.get_standard(0x10)) {
                self.brightness.store((v as i32).clamp(0, 100), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("contrast", self.protocol.get_standard(0x12)) {
                self.contrast.store((v as i32).clamp(0, 100), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("volume", self.protocol.get_standard(0x62)) {
                self.volume.store((v as i32).clamp(0, 100), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("power_led", self.protocol.get_standard(0xF2)) {
                self.power_led.store((v as i32).clamp(0, 4), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("audio_mute", self.protocol.get_standard(0x8D)) {
                // 1 = mute, 2 = unmute on this VCP.
                self.audio_mute.store(v == 1, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("resolution_notice", self.protocol.get_standard(0xE9)) {
                self.resolution_notice.store(v != 0, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("smart_response", self.protocol.get_standard(0xEB)) {
                self.smart_response.store((v as usize).min(3), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("input_source", self.protocol.get_standard(0x60)) {
                self.input_source.store(input_index_from_raw(v), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("osd_language", self.protocol.get_standard(0xCC)) {
                self.osd_language.store(language_index_from_raw(v), Ordering::Relaxed);
            }

            if let Some(v) = try_read!("crosshair", self.protocol.get_extended(0x04)) {
                self.crosshair.store((v as usize).min(2), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("low_input_lag", self.protocol.get_extended(0x07)) {
                self.low_input_lag.store(v != 0, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("osd_h_position", self.protocol.get_extended(0x0E)) {
                self.osd_h_position.store((v as i32).clamp(0, 100), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("osd_v_position", self.protocol.get_extended(0x0F)) {
                self.osd_v_position.store((v as i32).clamp(0, 100), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("osd_transparency", self.protocol.get_extended(0x10)) {
                self.osd_transparency.store((v as usize).min(4), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("osd_timeout", self.protocol.get_extended(0x11)) {
                self.osd_timeout.store((v as usize).min(4), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("usb_c_setting", self.protocol.get_extended(0x12)) {
                self.usb_c_setting.store((v as usize).min(1), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("usb_standby", self.protocol.get_extended(0x13)) {
                self.usb_standby.store(v != 0, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("kvm", self.protocol.get_extended(0x15)) {
                self.kvm.store((v as usize).min(2), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("smart_power", self.protocol.get_extended(0x16)) {
                self.smart_power.store(v != 0, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("cec", self.protocol.get_extended(0x17)) {
                self.cec.store(v != 0, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("smart_image", self.protocol.get_standard(0xDC)) {
                self.smart_image
                    .store(lookup_index(SMART_IMAGE_MODES, v), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("sharpness", self.protocol.get_standard(0x87)) {
                self.sharpness.store((v as i32).clamp(0, 100), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("gamma", self.protocol.get_standard(0x72)) {
                self.gamma.store(lookup_index(GAMMA_VALUES, v), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("color_temperature", self.protocol.get_standard(0x14)) {
                self.color_temperature
                    .store(lookup_index(COLOR_TEMPERATURES, v), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("srgb", self.protocol.get_extended(0x20)) {
                self.srgb.store(v != 0, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("pixel_orbiting", self.protocol.get_extended(0x34)) {
                self.pixel_orbiting
                    .store(lookup_index(PIXEL_ORBITING_OPTIONS, v), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("screen_saver", self.protocol.get_extended(0x35)) {
                self.screen_saver
                    .store(lookup_index(SCREEN_SAVER_OPTIONS, v), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("auto_warning", self.protocol.get_extended(0x43)) {
                self.auto_warning.store(v != 0, Ordering::Relaxed);
            }
            if let Some(v) = try_read!("light_enhancement", self.protocol.get_extended(0x3D)) {
                self.light_enhancement.store((v as i32).clamp(0, 3), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("color_enhancement", self.protocol.get_extended(0x3E)) {
                self.color_enhancement.store((v as i32).clamp(0, 3), Ordering::Relaxed);
            }
            if let Some(v) = try_read!("adaptive_sync", self.protocol.get_extended(0x40)) {
                self.adaptive_sync.store(v != 0, Ordering::Relaxed);
            }
        }

        fn input_choice_options() -> Vec<ChoiceOption> {
            INPUT_PORTS
                .iter()
                .map(|(_, id, label)| ChoiceOption {
                    id: (*id).into(),
                    label: (*label).into(),
                })
                .collect()
        }

        fn language_choice_options() -> Vec<ChoiceOption> {
            OSD_LANGUAGES
                .iter()
                .map(|(code, label)| ChoiceOption {
                    id: format!("{:02x}", code),
                    label: (*label).into(),
                })
                .collect()
        }

    }

    #[async_trait]
    impl Device for PhilipsEvnia49 {
        fn id(&self) -> String {
            self.id.clone()
        }

        fn name(&self) -> &str {
            "Philips Evnia 49"
        }

        fn vendor(&self) -> &str {
            "Philips"
        }

        fn model(&self) -> &str {
            "49M2C8900"
        }

        async fn initialize(&self) -> Result<bool> {
            match self.protocol.open(MCCS_VID, MCCS_PID, 0) {
                Ok(()) => {
                    log::info!("PhilipsEvnia49: MCCS transport opened");
                    self.read_device_info().await;
                    self.refresh_from_monitor().await;
                    Ok(true)
                }
                Err(e) => Err(anyhow!(
                    "MCCS control transport (USB {MCCS_VID:04x}:{MCCS_PID:04x}) open failed: {e}"
                )),
            }
        }

        async fn close(&self) {
            self.protocol.close();
        }

        async fn serialize(&self) -> WireDevice {
            let connected = self.protocol.is_connected();
            let serial = self.info.lock().unwrap().serial.clone();
            WireDeviceBuilder::from_device(self)
                .device_type(DeviceType::Monitor)
                .connected(connected)
                .serial_number(serial)
                .capabilities(vec![
                    DeviceCapability::Range(vec![
                        Range {
                            key: "brightness".into(),
                            label: "Brightness".into(),
                            min: 0,
                            max: 100,
                            step: 1,
                            value: self.brightness.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "contrast".into(),
                            label: "Contrast".into(),
                            min: 0,
                            max: 100,
                            step: 1,
                            value: self.contrast.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "volume".into(),
                            label: "Volume".into(),
                            min: 0,
                            max: 100,
                            step: 1,
                            value: self.volume.load(Ordering::Relaxed),
                            read_only: false,
                            category: "audio".into(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "sharpness".into(),
                            label: "Sharpness".into(),
                            min: 0,
                            max: 100,
                            step: 1,
                            value: self.sharpness.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "light_enhancement".into(),
                            label: "Light Enhancement".into(),
                            min: 0,
                            max: 3,
                            step: 1,
                            value: self.light_enhancement.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "color_enhancement".into(),
                            label: "Color Enhancement".into(),
                            min: 0,
                            max: 3,
                            step: 1,
                            value: self.color_enhancement.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "osd_h_position".into(),
                            label: "OSD Horizontal Position".into(),
                            min: 0,
                            max: 100,
                            step: 1,
                            value: self.osd_h_position.load(Ordering::Relaxed),
                            read_only: false,
                            category: "osd".into(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "osd_v_position".into(),
                            label: "OSD Vertical Position".into(),
                            min: 0,
                            max: 100,
                            step: 1,
                            value: self.osd_v_position.load(Ordering::Relaxed),
                            read_only: false,
                            category: "osd".into(),
                            start_label: None,
                            end_label: None,
                        },
                        Range {
                            key: "power_led".into(),
                            label: "Power LED Brightness".into(),
                            min: 0,
                            max: 4,
                            step: 1,
                            value: self.power_led.load(Ordering::Relaxed),
                            read_only: false,
                            category: "setup".into(),
                            start_label: Some("Off".into()),
                            end_label: Some("Max".into()),
                        },
                    ]),
                    DeviceCapability::Boolean(vec![
                        Boolean {
                            key: "adaptive_sync".into(),
                            label: "Adaptive Sync".into(),
                            value: self.adaptive_sync.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                        },
                        Boolean {
                            key: "low_input_lag".into(),
                            label: "Low Input Lag".into(),
                            value: self.low_input_lag.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                        },
                        Boolean {
                            key: "audio_mute".into(),
                            label: "Mute Audio".into(),
                            value: self.audio_mute.load(Ordering::Relaxed),
                            read_only: false,
                            category: "audio".into(),
                        },
                        Boolean {
                            key: "resolution_notice".into(),
                            label: "Resolution Notice".into(),
                            value: self.resolution_notice.load(Ordering::Relaxed),
                            read_only: false,
                            category: "osd".into(),
                        },
                        Boolean {
                            key: "usb_standby".into(),
                            label: "USB Standby Mode".into(),
                            value: self.usb_standby.load(Ordering::Relaxed),
                            read_only: false,
                            category: "system_usb".into(),
                        },
                        Boolean {
                            key: "smart_power".into(),
                            label: "Smart Power".into(),
                            value: self.smart_power.load(Ordering::Relaxed),
                            read_only: false,
                            category: "setup".into(),
                        },
                        Boolean {
                            key: "cec".into(),
                            label: "CEC".into(),
                            value: self.cec.load(Ordering::Relaxed),
                            read_only: false,
                            category: "setup".into(),
                        },
                        Boolean {
                            key: "auto_warning".into(),
                            label: "Auto Warning".into(),
                            value: self.auto_warning.load(Ordering::Relaxed),
                            read_only: false,
                            category: "setup".into(),
                        },
                        Boolean {
                            key: "srgb".into(),
                            label: "sRGB".into(),
                            value: self.srgb.load(Ordering::Relaxed),
                            read_only: false,
                            category: String::new(),
                        },
                        Boolean {
                            key: "hdr_active".into(),
                            label: "HDR Active".into(),
                            value: smart_image_is_hdr_active(
                                self.smart_image.load(Ordering::Relaxed),
                            ),
                            read_only: true,
                            category: String::new(),
                        },
                    ]),
                    DeviceCapability::Choice(vec![
                        Choice {
                            key: "input_source".into(),
                            label: "Input Source".into(),
                            options: Self::input_choice_options(),
                            selected: self.input_source.load(Ordering::Relaxed),
                            category: String::new(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "smart_image".into(),
                            label: "SmartImage".into(),
                            options: choices_from_table(SMART_IMAGE_MODES),
                            selected: self.smart_image.load(Ordering::Relaxed),
                            category: String::new(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "color_temperature".into(),
                            label: "Color Temperature".into(),
                            options: choices_from_table(COLOR_TEMPERATURES),
                            selected: self.color_temperature.load(Ordering::Relaxed),
                            category: String::new(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "gamma".into(),
                            label: "Gamma".into(),
                            options: choices_from_table(GAMMA_VALUES),
                            selected: self.gamma.load(Ordering::Relaxed),
                            category: String::new(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "pixel_orbiting".into(),
                            label: "Pixel Orbiting".into(),
                            options: choices_from_table(PIXEL_ORBITING_OPTIONS),
                            selected: self.pixel_orbiting.load(Ordering::Relaxed),
                            category: "setup".into(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "screen_saver".into(),
                            label: "Screen Saver".into(),
                            options: choices_from_table(SCREEN_SAVER_OPTIONS),
                            selected: self.screen_saver.load(Ordering::Relaxed),
                            category: "setup".into(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "crosshair".into(),
                            label: "Crosshair".into(),
                            options: vec![
                                ChoiceOption { id: "0".into(), label: "Off".into() },
                                ChoiceOption { id: "1".into(), label: "On".into() },
                                ChoiceOption { id: "2".into(), label: "Smart".into() },
                            ],
                            selected: self.crosshair.load(Ordering::Relaxed),
                            category: String::new(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "smart_response".into(),
                            label: "SmartResponse".into(),
                            options: vec![
                                ChoiceOption { id: "0".into(), label: "Off".into() },
                                ChoiceOption { id: "1".into(), label: "Fast".into() },
                                ChoiceOption { id: "2".into(), label: "Faster".into() },
                                ChoiceOption { id: "3".into(), label: "Fastest".into() },
                            ],
                            selected: self.smart_response.load(Ordering::Relaxed),
                            category: String::new(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "osd_transparency".into(),
                            label: "OSD Transparency".into(),
                            options: vec![
                                ChoiceOption { id: "0".into(), label: "Off".into() },
                                ChoiceOption { id: "1".into(), label: "1".into() },
                                ChoiceOption { id: "2".into(), label: "2".into() },
                                ChoiceOption { id: "3".into(), label: "3".into() },
                                ChoiceOption { id: "4".into(), label: "4".into() },
                            ],
                            selected: self.osd_transparency.load(Ordering::Relaxed),
                            category: "osd".into(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "osd_timeout".into(),
                            label: "OSD Timeout".into(),
                            options: vec![
                                ChoiceOption { id: "0".into(), label: "5 s".into() },
                                ChoiceOption { id: "1".into(), label: "10 s".into() },
                                ChoiceOption { id: "2".into(), label: "20 s".into() },
                                ChoiceOption { id: "3".into(), label: "30 s".into() },
                                ChoiceOption { id: "4".into(), label: "60 s".into() },
                            ],
                            selected: self.osd_timeout.load(Ordering::Relaxed),
                            category: "osd".into(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "osd_language".into(),
                            label: "OSD Language".into(),
                            options: Self::language_choice_options(),
                            selected: self.osd_language.load(Ordering::Relaxed),
                            category: "osd".into(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "usb_c_setting".into(),
                            label: "USB-C Setting".into(),
                            options: vec![
                                ChoiceOption {
                                    id: "0".into(),
                                    label: "High Resolution (USB 2.0)".into(),
                                },
                                ChoiceOption {
                                    id: "1".into(),
                                    label: "High Data Speed (USB 3.2)".into(),
                                },
                            ],
                            selected: self.usb_c_setting.load(Ordering::Relaxed),
                            category: "system_usb".into(),
                            display: Default::default(),
                        },
                        Choice {
                            key: "kvm".into(),
                            label: "KVM".into(),
                            options: vec![
                                ChoiceOption { id: "0".into(), label: "Auto".into() },
                                ChoiceOption { id: "1".into(), label: "USB Up".into() },
                                ChoiceOption { id: "2".into(), label: "USB-C".into() },
                            ],
                            selected: self.kvm.load(Ordering::Relaxed),
                            category: "system_usb".into(),
                            display: Default::default(),
                        },
                    ]),
                    DeviceCapability::Action(vec![
                        Action {
                            key: "pixel_refresh".into(),
                            label: "Pixel Refresh".into(),
                            category: "setup".into(),
                        },
                    ]),
                ])
                .build()
        }

        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![
                CapabilityRef::Range(self),
                CapabilityRef::Choice(self),
                CapabilityRef::Boolean(self),
                CapabilityRef::Action(self),
            ]
        }

        fn debug_transport(&self) -> Option<&'static str> {
            Some("usb_control")
        }

        fn hardware_serial(&self) -> Option<String> {
            self.info.lock().unwrap().serial.clone()
        }

        fn debug_info_extra(&self) -> Vec<(String, String)> {
            let info = self.info.lock().unwrap();
            let mut out = Vec::new();
            if let Some(m) = &info.model {
                out.push(("model_number".into(), m.clone()));
            }
            if let Some(fw) = &info.firmware {
                out.push(("firmware_version".into(), fw.clone()));
            }
            if let Some(pv) = &info.panel_variant {
                out.push(("panel_variant".into(), pv.clone()));
            }
            if let Some(pid) = &info.panel_id {
                out.push(("panel_id".into(), pid.clone()));
            }
            if let Some(sn) = &info.serial {
                out.push(("serial_number".into(), sn.clone()));
            }
            out
        }

    }

    #[async_trait]
    impl RangeCapability for PhilipsEvnia49 {
        fn range_cache(&self) -> &RangeStateCache {
            &self.range_cache
        }

        async fn set_range(&self, key: &str, value: i32) -> Result<()> {
            self.range_cache.record(key, value);
            match key {
                "brightness" => {
                    let v = value.clamp(0, 100) as u8;
                    self.protocol.write_packet(&build_write(0x10, v)).await?;
                    self.brightness.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "contrast" => {
                    let v = value.clamp(0, 100) as u8;
                    self.protocol.write_packet(&build_write(0x12, v)).await?;
                    self.contrast.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "volume" => {
                    let v = value.clamp(0, 100) as u8;
                    self.protocol.write_packet(&build_write(0x62, v)).await?;
                    self.volume.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "sharpness" => {
                    let v = value.clamp(0, 100) as u8;
                    self.protocol.write_packet(&build_write(0x87, v)).await?;
                    self.sharpness.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "power_led" => {
                    let v = value.clamp(0, 4) as u8;
                    self.protocol.write_packet(&build_write(0xF2, v)).await?;
                    self.power_led.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "light_enhancement" => {
                    let v = value.clamp(0, 3) as u8;
                    self.protocol.write_packet(&build_extended_set(0x3D, v)).await?;
                    self.light_enhancement.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "color_enhancement" => {
                    let v = value.clamp(0, 3) as u8;
                    self.protocol.write_packet(&build_extended_set(0x3E, v)).await?;
                    self.color_enhancement.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "osd_h_position" => {
                    let v = value.clamp(0, 100) as u8;
                    self.protocol.write_packet(&build_extended_set(0x0E, v)).await?;
                    self.osd_h_position.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                "osd_v_position" => {
                    let v = value.clamp(0, 100) as u8;
                    self.protocol.write_packet(&build_extended_set(0x0F, v)).await?;
                    self.osd_v_position.store(v as i32, Ordering::Relaxed);
                    Ok(())
                }
                other => Err(anyhow!("unknown range key: {other}")),
            }
        }
    }

    #[async_trait]
    impl ChoiceCapability for PhilipsEvnia49 {
        fn choice_cache(&self) -> &ChoiceStateCache {
            &self.choice_cache
        }

        async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
            self.choice_cache.record(key, selected);
            match key {
                "crosshair" => {
                    let v = selected.min(2) as u8;
                    self.protocol.write_packet(&build_extended_set(0x04, v)).await?;
                    self.crosshair.store(v as usize, Ordering::Relaxed);
                    Ok(())
                }
                "smart_response" => {
                    let v = selected.min(3) as u8;
                    self.protocol.write_packet(&build_write(0xEB, v)).await?;
                    self.smart_response.store(v as usize, Ordering::Relaxed);
                    Ok(())
                }
                "osd_transparency" => {
                    let v = selected.min(4) as u8;
                    self.protocol.write_packet(&build_extended_set(0x10, v)).await?;
                    self.osd_transparency.store(v as usize, Ordering::Relaxed);
                    Ok(())
                }
                "osd_timeout" => {
                    let v = selected.min(4) as u8;
                    self.protocol.write_packet(&build_extended_set(0x11, v)).await?;
                    self.osd_timeout.store(v as usize, Ordering::Relaxed);
                    Ok(())
                }
                "input_source" => {
                    let idx = selected.min(INPUT_PORTS.len() - 1);
                    let port = INPUT_PORTS[idx].0;
                    self.protocol.write_packet(&build_write(0x60, port)).await?;
                    self.input_source.store(idx, Ordering::Relaxed);
                    Ok(())
                }
                "osd_language" => {
                    let idx = selected.min(OSD_LANGUAGES.len() - 1);
                    let code = OSD_LANGUAGES[idx].0;
                    self.protocol.write_packet(&build_write(0xCC, code)).await?;
                    self.osd_language.store(idx, Ordering::Relaxed);
                    Ok(())
                }
                "usb_c_setting" => {
                    let v = selected.min(1) as u8;
                    self.protocol.write_packet(&build_extended_set(0x12, v)).await?;
                    self.usb_c_setting.store(v as usize, Ordering::Relaxed);
                    Ok(())
                }
                "kvm" => {
                    let v = selected.min(2) as u8;
                    self.protocol.write_packet(&build_extended_set(0x15, v)).await?;
                    self.kvm.store(v as usize, Ordering::Relaxed);
                    Ok(())
                }
                "smart_image" => {
                    let idx = selected.min(SMART_IMAGE_MODES.len() - 1);
                    let code = SMART_IMAGE_MODES[idx].0;
                    self.protocol.write_packet(&build_write(0xDC, code)).await?;
                    self.smart_image.store(idx, Ordering::Relaxed);
                    Ok(())
                }
                "color_temperature" => {
                    let idx = selected.min(COLOR_TEMPERATURES.len() - 1);
                    let code = COLOR_TEMPERATURES[idx].0;
                    self.protocol.write_packet(&build_write(0x14, code)).await?;
                    self.color_temperature.store(idx, Ordering::Relaxed);
                    Ok(())
                }
                "gamma" => {
                    let idx = selected.min(GAMMA_VALUES.len() - 1);
                    let code = GAMMA_VALUES[idx].0;
                    self.protocol.write_packet(&build_write(0x72, code)).await?;
                    self.gamma.store(idx, Ordering::Relaxed);
                    Ok(())
                }
                "pixel_orbiting" => {
                    let idx = selected.min(PIXEL_ORBITING_OPTIONS.len() - 1);
                    let code = PIXEL_ORBITING_OPTIONS[idx].0;
                    self.protocol.write_packet(&build_extended_set(0x34, code)).await?;
                    self.pixel_orbiting.store(idx, Ordering::Relaxed);
                    Ok(())
                }
                "screen_saver" => {
                    let idx = selected.min(SCREEN_SAVER_OPTIONS.len() - 1);
                    let code = SCREEN_SAVER_OPTIONS[idx].0;
                    self.protocol.write_packet(&build_extended_set(0x35, code)).await?;
                    self.screen_saver.store(idx, Ordering::Relaxed);
                    Ok(())
                }
                other => Err(anyhow!("unknown choice key: {other}")),
            }
        }
    }

    #[async_trait]
    impl BooleanCapability for PhilipsEvnia49 {
        async fn get_booleans(&self) -> Result<Vec<Boolean>> {
            Ok(vec![
                Boolean {
                    key: "adaptive_sync".into(),
                    label: "Adaptive Sync".into(),
                    value: self.adaptive_sync.load(Ordering::Relaxed),
                    read_only: false,
                    category: String::new(),
                },
                Boolean {
                    key: "low_input_lag".into(),
                    label: "Low Input Lag".into(),
                    value: self.low_input_lag.load(Ordering::Relaxed),
                    read_only: false,
                    category: String::new(),
                },
                Boolean {
                    key: "audio_mute".into(),
                    label: "Mute Audio".into(),
                    value: self.audio_mute.load(Ordering::Relaxed),
                    read_only: false,
                    category: "audio".into(),
                },
                Boolean {
                    key: "resolution_notice".into(),
                    label: "Resolution Notice".into(),
                    value: self.resolution_notice.load(Ordering::Relaxed),
                    read_only: false,
                    category: "osd".into(),
                },
                Boolean {
                    key: "usb_standby".into(),
                    label: "USB Standby Mode".into(),
                    value: self.usb_standby.load(Ordering::Relaxed),
                    read_only: false,
                    category: "setup".into(),
                },
                Boolean {
                    key: "smart_power".into(),
                    label: "Smart Power".into(),
                    value: self.smart_power.load(Ordering::Relaxed),
                    read_only: false,
                    category: "system_smart_power".into(),
                },
                Boolean {
                    key: "cec".into(),
                    label: "CEC".into(),
                    value: self.cec.load(Ordering::Relaxed),
                    read_only: false,
                    category: "setup".into(),
                },
                Boolean {
                    key: "auto_warning".into(),
                    label: "Auto Warning".into(),
                    value: self.auto_warning.load(Ordering::Relaxed),
                    read_only: false,
                    category: "setup".into(),
                },
                Boolean {
                    key: "srgb".into(),
                    label: "sRGB".into(),
                    value: self.srgb.load(Ordering::Relaxed),
                    read_only: false,
                    category: String::new(),
                },
                Boolean {
                    key: "hdr_active".into(),
                    label: "HDR Active".into(),
                    value: smart_image_is_hdr_active(self.smart_image.load(Ordering::Relaxed)),
                    read_only: true,
                    category: String::new(),
                },
            ])
        }

        async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
            match key {
                "adaptive_sync" => {
                    self.protocol.write_packet(&build_extended_set(0x40, value as u8)).await?;
                    self.adaptive_sync.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "low_input_lag" => {
                    self.protocol.write_packet(&build_extended_set(0x07, value as u8)).await?;
                    self.low_input_lag.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "audio_mute" => {
                    // MCCS VCP 0x8d (Audio Mute): 0x01 = mute, 0x02 = unmute.
                    let v = if value { 0x01 } else { 0x02 };
                    self.protocol.write_packet(&build_write(0x8D, v)).await?;
                    self.audio_mute.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "resolution_notice" => {
                    // VCP 0xE9 (Resolution Notice): 0x00 = off, 0x02 = on.
                    let v = if value { 0x02 } else { 0x00 };
                    self.protocol.write_packet(&build_write(0xE9, v)).await?;
                    self.resolution_notice.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "usb_standby" => {
                    self.protocol.write_packet(&build_extended_set(0x13, value as u8)).await?;
                    self.usb_standby.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "smart_power" => {
                    self.protocol.write_packet(&build_extended_set(0x16, value as u8)).await?;
                    self.smart_power.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "cec" => {
                    self.protocol.write_packet(&build_extended_set(0x17, value as u8)).await?;
                    self.cec.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "auto_warning" => {
                    self.protocol.write_packet(&build_extended_set(0x43, value as u8)).await?;
                    self.auto_warning.store(value, Ordering::Relaxed);
                    Ok(())
                }
                "srgb" => {
                    // E2A020 (sRGB): 0x00 = Off, 0x02 = On.
                    let v = if value { 0x02 } else { 0x00 };
                    self.protocol.write_packet(&build_extended_set(0x20, v)).await?;
                    self.srgb.store(value, Ordering::Relaxed);
                    Ok(())
                }
                other => Err(anyhow!("unknown boolean key: {other}")),
            }
        }
    }

    #[async_trait]
    impl ActionCapability for PhilipsEvnia49 {
        async fn trigger_action(&self, key: &str) -> Result<()> {
            match key {
                "pixel_refresh" => {
                    self.protocol.write_packet(&build_extended_set(0x36, 0x01)).await?;
                    Ok(())
                }
                other => Err(anyhow!("unknown action key: {other}")),
            }
        }
    }

    inventory::submit!(DeviceDescriptor {
        matches: |h| matches!(h, DiscoveryHandle::UsbNonHid { vid: MCCS_VID, pid: MCCS_PID }),
        make: |_h| Ok(Arc::new(PhilipsEvnia49::new()) as Arc<dyn crate::drivers::Device>),
    });

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::drivers::vendors::philips::protocols::philips_evnia::{
            build_get_extended, build_get_info, build_get_standard, ddcci_xor, parse_get_reply,
            parse_info_reply,
        };

        #[test]
        fn brightness_frame_uses_vcp_10() {
            // 6e 51 84 03 <vcp> 00 <value> <xor>
            let p = build_write(0x10, 75);
            assert_eq!(&p[..7], &[0x6e, 0x51, 0x84, 0x03, 0x10, 0x00, 75]);
            assert_eq!(p[7], ddcci_xor(&p[..7]));
        }

        #[test]
        fn contrast_frame_matches_capture() {
            // Captured DDC/CI payload for "contrast = 0": 6e 51 84 03 12 00 00 aa
            let p = build_write(0x12, 0x00);
            assert_eq!(p, [0x6e, 0x51, 0x84, 0x03, 0x12, 0x00, 0x00, 0xaa]);
        }

        #[test]
        fn light_enhancement_frame_matches_capture() {
            // Captured: 6e 51 86 03 e2 a0 3d 00 03 c6
            let p = build_extended_set(0x3D, 3);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x3d, 0x00, 0x03, 0xc6]);
        }

        #[test]
        fn color_enhancement_frame_matches_capture() {
            // Captured: 6e 51 86 03 e2 a0 3e 00 03 c5
            let p = build_extended_set(0x3E, 3);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x3e, 0x00, 0x03, 0xc5]);
        }

        #[test]
        fn adaptive_sync_frame_matches_capture() {
            // Captured: 6e 51 86 03 e2 a0 40 00 01 b9
            let p = build_extended_set(0x40, 1);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x40, 0x00, 0x01, 0xb9]);
        }

        #[test]
        fn low_input_lag_frame_matches_capture() {
            // Captured: 6e 51 86 03 e2 a0 07 00 01 fe
            let p = build_extended_set(0x07, 1);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x07, 0x00, 0x01, 0xfe]);
        }

        #[test]
        fn audio_mute_frames_match_captures() {
            assert_eq!(build_write(0x8D, 1), [0x6e, 0x51, 0x84, 0x03, 0x8d, 0x00, 0x01, 0x34]);
            assert_eq!(build_write(0x8D, 2), [0x6e, 0x51, 0x84, 0x03, 0x8d, 0x00, 0x02, 0x37]);
        }

        #[test]
        fn smart_response_frames_match_captures() {
            assert_eq!(build_write(0xEB, 0), [0x6e, 0x51, 0x84, 0x03, 0xeb, 0x00, 0x00, 0x53]);
            assert_eq!(build_write(0xEB, 1), [0x6e, 0x51, 0x84, 0x03, 0xeb, 0x00, 0x01, 0x52]);
            assert_eq!(build_write(0xEB, 2), [0x6e, 0x51, 0x84, 0x03, 0xeb, 0x00, 0x02, 0x51]);
            assert_eq!(build_write(0xEB, 3), [0x6e, 0x51, 0x84, 0x03, 0xeb, 0x00, 0x03, 0x50]);
        }

        #[test]
        fn osd_position_frames_match_captures() {
            let h = build_extended_set(0x0E, 0);
            assert_eq!(h, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x0e, 0x00, 0x00, 0xf6]);
            let v = build_extended_set(0x0F, 0);
            assert_eq!(v, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x0f, 0x00, 0x00, 0xf7]);
        }

        #[test]
        fn osd_transparency_frames_match_captures() {
            let t4 = build_extended_set(0x10, 4);
            assert_eq!(t4, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x10, 0x00, 0x04, 0xec]);
            let t0 = build_extended_set(0x10, 0);
            assert_eq!(t0, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x10, 0x00, 0x00, 0xe8]);
        }

        #[test]
        fn osd_timeout_frame_matches_capture() {
            let p = build_extended_set(0x11, 2);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x11, 0x00, 0x02, 0xeb]);
        }

        #[test]
        fn usb_c_setting_frames_match_captures() {
            // From phil.md: USB-C high data speed → 6e 51 86 03 e2 a0 12 00 01 eb
            let p = build_extended_set(0x12, 1);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x12, 0x00, 0x01, 0xeb]);
            // High res 2.0 → 6e 51 86 03 e2 a0 12 00 00 ea
            let p = build_extended_set(0x12, 0);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x12, 0x00, 0x00, 0xea]);
        }

        #[test]
        fn usb_standby_frames_match_captures() {
            // off → 6e 51 86 03 e2 a0 13 00 00 eb, on → ... 01 ea
            let p_off = build_extended_set(0x13, 0);
            assert_eq!(p_off, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x13, 0x00, 0x00, 0xeb]);
            let p_on = build_extended_set(0x13, 1);
            assert_eq!(p_on, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x13, 0x00, 0x01, 0xea]);
        }

        #[test]
        fn kvm_usb_c_frame_matches_capture() {
            // KVM → USB-C: 6e 51 86 03 e2 a0 15 00 02 ef
            let p = build_extended_set(0x15, 2);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x15, 0x00, 0x02, 0xef]);
        }

        #[test]
        fn smart_power_frames_match_captures() {
            let off = build_extended_set(0x16, 0);
            assert_eq!(off, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x16, 0x00, 0x00, 0xee]);
            let on = build_extended_set(0x16, 1);
            assert_eq!(on, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x16, 0x00, 0x01, 0xef]);
        }

        #[test]
        fn cec_frames_match_captures() {
            let off = build_extended_set(0x17, 0);
            assert_eq!(off, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x17, 0x00, 0x00, 0xef]);
            let on = build_extended_set(0x17, 1);
            assert_eq!(on, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x17, 0x00, 0x01, 0xee]);
        }

        #[test]
        fn power_led_frames_match_captures() {
            // Sweep 0..4 against the bytes captured in phil.md.
            let f0 = build_write(0xF2, 0);
            assert_eq!(f0, [0x6e, 0x51, 0x84, 0x03, 0xf2, 0x00, 0x00, 0x4a]);
            let f2 = build_write(0xF2, 2);
            assert_eq!(f2, [0x6e, 0x51, 0x84, 0x03, 0xf2, 0x00, 0x02, 0x48]);
            let f3 = build_write(0xF2, 3);
            assert_eq!(f3, [0x6e, 0x51, 0x84, 0x03, 0xf2, 0x00, 0x03, 0x49]);
            let f4 = build_write(0xF2, 4);
            assert_eq!(f4, [0x6e, 0x51, 0x84, 0x03, 0xf2, 0x00, 0x04, 0x4e]);
        }

        #[test]
        fn resolution_notice_frames_match_captures() {
            let on = build_write(0xE9, 2);
            assert_eq!(on, [0x6e, 0x51, 0x84, 0x03, 0xe9, 0x00, 0x02, 0x53]);
            let off = build_write(0xE9, 0);
            assert_eq!(off, [0x6e, 0x51, 0x84, 0x03, 0xe9, 0x00, 0x00, 0x51]);
        }

        #[test]
        fn get_brightness_request_matches_capture() {
            // GET VCP 0x10 request: 6e 51 82 01 10 ac (XOR of the preceding bytes).
            let p = build_get_standard(0x10);
            assert_eq!(p, [0x6e, 0x51, 0x82, 0x01, 0x10, 0xac]);
        }

        #[test]
        fn get_extended_request_matches_capture() {
            // Captured GET extVCP e2a03d: 6e 51 84 01 e2 a0 3d c5
            let p = build_get_extended(0x3D);
            assert_eq!(p, [0x6e, 0x51, 0x84, 0x01, 0xe2, 0xa0, 0x3d, 0xc5]);
        }

        #[test]
        fn parse_get_reply_decodes_brightness() {
            // Captured reply: 6e 88 02 00 10 00 00 64 00 54 <xor>
            // Compute the correct checksum dynamically since the capture didn't
            // include the brightness reply (we use contrast's structure here).
            let header = [0x6e, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x54];
            let xor = 0x50u8 ^ ddcci_xor(&header);
            let mut packet = [0u8; 12];
            packet[..10].copy_from_slice(&header);
            packet[10] = xor;
            let cur = parse_get_reply(&packet).unwrap();
            assert_eq!(cur, 0x0054);
        }

        #[test]
        fn parse_get_reply_decodes_extended_value() {
            // Captured reply for GET extVCP e2a01a: 6e 88 02 00 e2 01 00 0d 00 06 <xor>
            // (max=13 cur=6 in the original SmartImage probe.)
            let body = [0x6e, 0x88, 0x02, 0x00, 0xe2, 0x01, 0x00, 0x0d, 0x00, 0x06];
            let xor = 0x50u8 ^ ddcci_xor(&body);
            let mut packet = [0u8; 12];
            packet[..10].copy_from_slice(&body);
            packet[10] = xor;
            let cur = parse_get_reply(&packet).unwrap();
            assert_eq!(cur, 6);
        }

        #[test]
        fn smart_image_hdr_frames_match_captures() {
            // From phil.md: HDR Personal = 0x24, True Black = 0x30, Game = 0x21,
            // Movie = 0x22, Vivid = 0x23, Off = 0x20. Each is a standard set on
            // VCP 0xDC.
            assert_eq!(build_write(0xDC, 0x24), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x24, 0x40]);
            assert_eq!(build_write(0xDC, 0x30), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x30, 0x54]);
            assert_eq!(build_write(0xDC, 0x21), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x21, 0x45]);
            assert_eq!(build_write(0xDC, 0x22), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x22, 0x46]);
            assert_eq!(build_write(0xDC, 0x23), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x23, 0x47]);
            assert_eq!(build_write(0xDC, 0x20), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x20, 0x44]);
        }

        #[test]
        fn smart_image_sdr_frames_match_captures() {
            // From phil.md SDR section: Standard=0x00, FPS=0x01, Movie=0x03, Game1=0x04,
            // Game2=0x05, Racing=0x06, RTS=0x07, Economy=0x08, LowBlue=0x0B, EasyRead=0x0E,
            // Console=0x11. Spot-check Standard, Game1, Economy and Console.
            assert_eq!(build_write(0xDC, 0x00), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x00, 0x64]);
            assert_eq!(build_write(0xDC, 0x04), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x04, 0x60]);
            assert_eq!(build_write(0xDC, 0x08), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x08, 0x6c]);
            assert_eq!(build_write(0xDC, 0x11), [0x6e, 0x51, 0x84, 0x03, 0xdc, 0x00, 0x11, 0x75]);
        }

        #[test]
        fn sharpness_frames_match_captures() {
            assert_eq!(build_write(0x87, 0), [0x6e, 0x51, 0x84, 0x03, 0x87, 0x00, 0x00, 0x3f]);
            assert_eq!(build_write(0x87, 100), [0x6e, 0x51, 0x84, 0x03, 0x87, 0x00, 0x64, 0x5b]);
        }

        #[test]
        fn gamma_frames_match_captures() {
            assert_eq!(build_write(0x72, 0x50), [0x6e, 0x51, 0x84, 0x03, 0x72, 0x00, 0x50, 0x9a]);
            assert_eq!(build_write(0x72, 0x64), [0x6e, 0x51, 0x84, 0x03, 0x72, 0x00, 0x64, 0xae]);
            assert_eq!(build_write(0x72, 0x78), [0x6e, 0x51, 0x84, 0x03, 0x72, 0x00, 0x78, 0xb2]);
            assert_eq!(build_write(0x72, 0x8C), [0x6e, 0x51, 0x84, 0x03, 0x72, 0x00, 0x8c, 0x46]);
            assert_eq!(build_write(0x72, 0xA0), [0x6e, 0x51, 0x84, 0x03, 0x72, 0x00, 0xa0, 0x6a]);
        }

        #[test]
        fn srgb_frames_match_captures() {
            // E2A020: Off=0x00, On=0x02.
            assert_eq!(
                build_extended_set(0x20, 0x00),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x20, 0x00, 0x00, 0xd8]
            );
            assert_eq!(
                build_extended_set(0x20, 0x02),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x20, 0x00, 0x02, 0xda]
            );
        }

        #[test]
        fn color_temp_frames_match_captures() {
            // Native=0x02 ... Preset=0x0D. Spot-check several from phil.md.
            assert_eq!(build_write(0x14, 0x02), [0x6e, 0x51, 0x84, 0x03, 0x14, 0x00, 0x02, 0xae]);
            assert_eq!(build_write(0x14, 0x04), [0x6e, 0x51, 0x84, 0x03, 0x14, 0x00, 0x04, 0xa8]);
            assert_eq!(build_write(0x14, 0x05), [0x6e, 0x51, 0x84, 0x03, 0x14, 0x00, 0x05, 0xa9]);
            assert_eq!(build_write(0x14, 0x0A), [0x6e, 0x51, 0x84, 0x03, 0x14, 0x00, 0x0a, 0xa6]);
            assert_eq!(build_write(0x14, 0x0D), [0x6e, 0x51, 0x84, 0x03, 0x14, 0x00, 0x0d, 0xa1]);
        }

        #[test]
        fn pixel_orbiting_frames_match_captures() {
            // From phil.md: Off=0, Slow=2, Normal=3, Fast=4.
            assert_eq!(
                build_extended_set(0x34, 0),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x34, 0x00, 0x00, 0xcc]
            );
            assert_eq!(
                build_extended_set(0x34, 2),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x34, 0x00, 0x02, 0xce]
            );
            assert_eq!(
                build_extended_set(0x34, 3),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x34, 0x00, 0x03, 0xcf]
            );
            assert_eq!(
                build_extended_set(0x34, 4),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x34, 0x00, 0x04, 0xc8]
            );
        }

        #[test]
        fn screen_saver_frames_match_captures() {
            // From phil.md: Off=0, Slow=2, Fast=3.
            assert_eq!(
                build_extended_set(0x35, 0),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x35, 0x00, 0x00, 0xcd]
            );
            assert_eq!(
                build_extended_set(0x35, 2),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x35, 0x00, 0x02, 0xcf]
            );
            assert_eq!(
                build_extended_set(0x35, 3),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x35, 0x00, 0x03, 0xce]
            );
        }

        #[test]
        fn pixel_refresh_frame_matches_capture() {
            // Captured: 6e 51 86 03 e2 a0 36 00 01 cf
            let p = build_extended_set(0x36, 0x01);
            assert_eq!(p, [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x36, 0x00, 0x01, 0xcf]);
        }

        #[test]
        fn auto_warning_frames_match_captures() {
            assert_eq!(
                build_extended_set(0x43, 0),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x43, 0x00, 0x00, 0xbb]
            );
            assert_eq!(
                build_extended_set(0x43, 1),
                [0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, 0x43, 0x00, 0x01, 0xba]
            );
        }

        #[test]
        fn info_request_frames_match_captures() {
            // Captured info-string GETs from rev_eng/parsed.txt.
            // Model "49M2C8900" came from 01 fe e9 0d 00 00 → xor 0xa2.
            assert_eq!(
                build_get_info([0xE9, 0x0D, 0x00, 0x00]),
                [0x6e, 0x51, 0x86, 0x01, 0xfe, 0xe9, 0x0d, 0x00, 0x00, 0xa2]
            );
            // Firmware "1.02" from 01 fe e1 e6 06 00 → xor 0x47.
            assert_eq!(
                build_get_info([0xE1, 0xE6, 0x06, 0x00]),
                [0x6e, 0x51, 0x86, 0x01, 0xfe, 0xe1, 0xe6, 0x06, 0x00, 0x47]
            );
            // Panel variant "100GPMSV001NT1SXXY" from 01 fe e1 e6 1d 00 → xor 0x5c.
            assert_eq!(
                build_get_info([0xE1, 0xE6, 0x1D, 0x00]),
                [0x6e, 0x51, 0x86, 0x01, 0xfe, 0xe1, 0xe6, 0x1d, 0x00, 0x5c]
            );
            // Panel id "MT9810MGQJBG" from 01 fe e1 e8 00 00 → xor 0x4f.
            assert_eq!(
                build_get_info([0xE1, 0xE8, 0x00, 0x00]),
                [0x6e, 0x51, 0x86, 0x01, 0xfe, 0xe1, 0xe8, 0x00, 0x00, 0x4f]
            );
            // Serial "AU424390000035" from 01 fe ef 13 00 20 → xor 0x9a.
            assert_eq!(
                build_get_info([0xEF, 0x13, 0x00, 0x20]),
                [0x6e, 0x51, 0x86, 0x01, 0xfe, 0xef, 0x13, 0x00, 0x20, 0x9a]
            );
        }

        #[test]
        fn parse_info_reply_decodes_model() {
            // Captured reply for the model probe:
            // 6e 8d 02 fe e9 34 39 4d 32 43 38 39 30 30 00 96
            let reply = [
                0x6e, 0x8d, 0x02, 0xfe, 0xe9, 0x34, 0x39, 0x4d, 0x32, 0x43, 0x38, 0x39, 0x30, 0x30,
                0x00, 0x96,
            ];
            assert_eq!(parse_info_reply(&reply).unwrap(), "49M2C8900");
        }

        #[test]
        fn parse_info_reply_decodes_firmware() {
            // Captured reply for the firmware probe:
            // 6e 88 02 fe e1 31 2e 30 32 00 b6
            let reply = [
                0x6e, 0x88, 0x02, 0xfe, 0xe1, 0x31, 0x2e, 0x30, 0x32, 0x00, 0xb6,
            ];
            assert_eq!(parse_info_reply(&reply).unwrap(), "1.02");
        }

        #[test]
        fn parse_info_reply_rejects_bad_checksum() {
            let mut reply = [
                0x6e, 0x88, 0x02, 0xfe, 0xe1, 0x31, 0x2e, 0x30, 0x32, 0x00, 0xb6,
            ];
            reply[10] = 0xFF;
            assert!(parse_info_reply(&reply).is_err());
        }

        #[test]
        fn parse_info_reply_decodes_serial_without_envelope() {
            // Asset-EEPROM read for `ef 13 00 20` returns raw ASCII (no 02 fe
            // header), terminated by the XOR checksum:
            // 6e 8d 41 55 34 32 34 33 39 30 30 30 30 33 35 99
            // The bytes spell "AU42439000035" — 13 chars, 4 zeros (one fewer
            // than the marketing sticker, which reads "AU424390000035").
            let reply = [
                0x6e, 0x8d, 0x41, 0x55, 0x34, 0x32, 0x34, 0x33, 0x39, 0x30, 0x30, 0x30, 0x30, 0x33,
                0x35, 0x99,
            ];
            assert_eq!(parse_info_reply(&reply).unwrap(), "AU42439000035");
        }

        #[test]
        fn hdr_active_derives_from_smart_image_index() {
            // SDR entries (Standard..Console Mode at indices 0..=10) are not HDR.
            assert!(!smart_image_is_hdr_active(0));
            assert!(!smart_image_is_hdr_active(10));
            // HDR Game..HDR Personal (indices 11..=15) are HDR active.
            assert!(smart_image_is_hdr_active(11));
            assert!(smart_image_is_hdr_active(15));
            // "HDR Off" (index 16) sits in the HDR menu but disables HDR processing.
            assert!(!smart_image_is_hdr_active(16));
        }

        #[test]
        fn smart_image_index_round_trip() {
            // Standard=0x00 is the first entry; HDR Personal=0x24 is the 16th.
            assert_eq!(lookup_index(SMART_IMAGE_MODES, 0x0000), 0);
            assert_eq!(lookup_index(SMART_IMAGE_MODES, 0x0024), 15);
            assert_eq!(SMART_IMAGE_MODES[15].0, 0x24);
        }

        #[test]
        fn parse_get_reply_rejects_bad_checksum() {
            let mut packet = [0x6e, 0x88, 0x02, 0x00, 0x10, 0x00, 0x00, 0x64, 0x00, 0x54, 0x00, 0];
            packet[10] = 0xFF;
            assert!(parse_get_reply(&packet).is_err());
        }

        #[test]
        fn input_index_decodes_dp1_from_capture() {
            // Captured: cur = 0x350F. Low byte 0x0F = DP1, first entry.
            assert_eq!(input_index_from_raw(0x350F), 0);
            assert_eq!(input_index_from_raw(0x3511), 2); // HDMI1
        }

        #[test]
        fn language_index_decodes_english() {
            assert_eq!(language_index_from_raw(0x0002), 1); // English = entry 1
            assert_eq!(language_index_from_raw(0x0001), 0); // Chinese (Traditional)
        }
    }
}
