// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: linux-arctis-manager contributors <https://github.com/elegos/Linux-Arctis-Manager>
// Protocol reference: linux-arctis-manager (GPL-3.0) and sennheiser-gsx-control (MIT)

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

use crate::{
    audio::{self, ChatMixSinks},
    discovery::{DeviceDescriptor, DiscoveryHandle},
    drivers::{
        transports::hid::HidTransport,
        vendors::generic::devices::common::{TaskHandle, WireDeviceBuilder},
        vendors::steelseries::protocols::steelseries_arctis::{
            ArctisChatMixFields, ArctisProtocol, ArctisSettingsFields, ArctisStatusFields,
            POWER_CHARGING, POWER_OFFLINE, POWER_ONLINE,
        },
        BatteryCapability, BooleanCapability, CapabilityRef, ChoiceCapability, ChoiceStateCache,
        Device, EqualizerCapability, RangeCapability, RangeStateCache,
    },
};
use halod_protocol::types::{
    Battery, BatteryStatus, Boolean, Choice, ChoiceDisplay, ChoiceOption, DeviceCapability,
    DeviceType, EqBand, Equalizer, Range, WireDevice,
};

static ARCTIS_IDS: &[(u16, u16, &str)] = &[
    (0x1038, 0x12E0, "Arctis Nova Pro Wireless"),
    (0x1038, 0x12E5, "Arctis Nova Pro Wireless X"),
];

inventory::submit!(DeviceDescriptor {
    matches: |h| {
        let DiscoveryHandle::Hid {
            vid,
            pid,
            interface_number: Some(4),
            ..
        } = h
        else {
            return false;
        };
        ARCTIS_IDS.iter().any(|&(v, p, _)| v == *vid && p == *pid)
    },
    make: |h| {
        let DiscoveryHandle::Hid { path, idx, pid, .. } = h else {
            anyhow::bail!("descriptor matched non-HID handle");
        };
        Ok(Arc::new(ArctisNovaProWireless::new(path, pid, idx)?)
            as Arc<dyn crate::drivers::Device>)
    },
});

const SUPPRESS_SECS: u64 = 3;
const POLL_INTERVAL_MS: u64 = 250;

const EQ_BAND_LABELS: [&str; 10] = [
    "31 Hz", "62 Hz", "125 Hz", "250 Hz", "500 Hz", "1 kHz", "2 kHz", "4 kHz", "8 kHz", "16 kHz",
];
const EQ_PRESET_NAMES: [&str; 5] = ["Flat", "Bass Boost", "Reference", "Smiley", "Custom"];
const AUTO_OFF_LABELS: [&str; 7] = [
    "Off", "1 min", "5 min", "10 min", "15 min", "30 min", "60 min",
];

struct ArctisStatus {
    headset_battery: u8, // 0–100 %
    slot_battery: u8,    // 0–100 %
    power_status: u8,
    mic_muted: bool,
    nc_mode: u8,       // 0=off 1=transparent 2=on
    nc_level: u8,      // 0–100 in steps of 10
    wireless_mode: u8, // 0=speed 1=range
    auto_off_raw: u8,  // 0x00–0x06
    gain: u8,          // 0=low 1=high
    sidetone: u8,      // 0=off 1=low 2=medium 3=high
    eq_preset: usize,  // 0–4
    eq_bands: [f32; 10],
    suppress_until: Instant,
    chatmix_game: u8,       // 0–100, volume for game/media sink
    chatmix_chat: u8,       // 0–100, volume for chat sink
    sonar_eq: u8,           // 0=off 1=on  (cmd 0x8d)
    screen_mode: u8,        // 0=detailed 1=simple  (cmd 0x89)
    mic_led_brightness: u8, // 0–100 in steps of 10 (raw 0–10, cmd 0xbf)
    // True after the first successful status poll; guards against showing 0% before any data arrives.
    polled: bool,
}

impl Default for ArctisStatus {
    fn default() -> Self {
        Self {
            headset_battery: 0,
            slot_battery: 0,
            power_status: POWER_OFFLINE,
            mic_muted: false,
            nc_mode: 0,
            nc_level: 0,
            wireless_mode: 0,
            auto_off_raw: 0,
            gain: 0,
            sidetone: 0,
            eq_preset: 0,
            eq_bands: [0.0; 10],
            suppress_until: Instant::now(),
            chatmix_game: 100,
            chatmix_chat: 100,
            sonar_eq: 0,
            screen_mode: 0,
            mic_led_brightness: 100,
            polled: false,
        }
    }
}

pub struct ArctisNovaProWireless {
    id: String,
    model_name: &'static str,
    protocol: ArctisProtocol<HidTransport>,
    status: Arc<TokioMutex<ArctisStatus>>,
    chatmix_sinks: Arc<TokioMutex<Option<ChatMixSinks>>>,
    poll_task: Arc<TokioMutex<Option<TaskHandle>>>,
    range_cache: RangeStateCache,
    choice_cache: ChoiceStateCache,
    eq_cache: std::sync::Mutex<Option<(usize, [f32; 10])>>,
}

async fn dispatch_chatmix(
    fields: ArctisChatMixFields,
    status: &Arc<TokioMutex<ArctisStatus>>,
    chatmix_sinks: &Arc<TokioMutex<Option<ChatMixSinks>>>,
) {
    {
        let mut s = status.lock().await;
        s.chatmix_game = fields.game;
        s.chatmix_chat = fields.chat;
    }
    let sinks = chatmix_sinks.lock().await;
    if let Some(ref s) = *sinks {
        audio::set_chatmix_volume(s, fields.game, fields.chat).await;
    }
}

fn apply_status_fields(s: &mut ArctisStatus, f: ArctisStatusFields) {
    s.headset_battery = f.headset_battery;
    s.slot_battery = f.slot_battery;
    s.power_status = f.power_status;
    s.mic_muted = f.mic_muted;
    s.nc_mode = f.nc_mode;
    s.nc_level = f.nc_level;
    s.wireless_mode = f.wireless_mode;
    s.auto_off_raw = f.auto_off_raw;
    s.mic_led_brightness = f.mic_led_brightness;
    s.polled = true;
}

fn apply_settings_fields(s: &mut ArctisStatus, f: ArctisSettingsFields) {
    s.gain = f.gain;
    s.eq_preset = f.eq_preset;
    s.sidetone = f.sidetone;
}

impl ArctisNovaProWireless {
    pub fn new(path: &str, pid: u16, index: usize) -> Result<Self> {
        let model_name = ARCTIS_IDS
            .iter()
            .find(|&&(_, p, _)| p == pid)
            .map(|&(_, _, name)| name)
            .unwrap_or("Arctis Nova Pro Wireless");
        Ok(Self {
            id: format!("steelseries_arctis_{index}"),
            model_name,
            protocol: ArctisProtocol::open(path)?,
            status: Arc::new(TokioMutex::new(ArctisStatus::default())),
            chatmix_sinks: Arc::new(TokioMutex::new(None)),
            poll_task: Arc::new(TokioMutex::new(None)),
            range_cache: RangeStateCache::default(),
            choice_cache: ChoiceStateCache::default(),
            eq_cache: std::sync::Mutex::new(None),
        })
    }

    async fn suppress(&self) {
        let mut s = self.status.lock().await;
        s.suppress_until = Instant::now() + Duration::from_secs(SUPPRESS_SECS);
    }

    async fn persist(&self) -> Result<()> {
        self.protocol.persist().await
    }
}

#[async_trait]
impl Device for ArctisNovaProWireless {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn name(&self) -> &str {
        self.model_name
    }
    fn vendor(&self) -> &str {
        "SteelSeries"
    }
    fn model(&self) -> &str {
        self.model_name
    }

    async fn initialize(&self) -> Result<bool> {
        let _ = self.protocol.activate_chatmix_display().await;

        let protocol = self.protocol.clone();
        let status = self.status.clone();
        let chatmix_sinks = self.chatmix_sinks.clone();

        let handle = tokio::spawn(async move {
            // Initial one-shot fetch so state is populated before the first poll interval.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (status_fields, chatmix_list) = protocol.poll_status().await;
            if let Some(f) = status_fields {
                apply_status_fields(&mut *status.lock().await, f);
            }
            for cm in chatmix_list {
                dispatch_chatmix(cm, &status, &chatmix_sinks).await;
            }
            let (settings_fields, chatmix_list) = protocol.poll_settings().await;
            if let Some(f) = settings_fields {
                apply_settings_fields(&mut *status.lock().await, f);
            }
            for cm in chatmix_list {
                dispatch_chatmix(cm, &status, &chatmix_sinks).await;
            }

            let mut interval = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
            loop {
                interval.tick().await;

                let suppressed = status.lock().await.suppress_until > Instant::now();
                if suppressed {
                    continue;
                }

                let (status_fields, chatmix_list) = protocol.poll_status().await;
                if let Some(f) = status_fields {
                    apply_status_fields(&mut *status.lock().await, f);
                }
                for cm in chatmix_list {
                    dispatch_chatmix(cm, &status, &chatmix_sinks).await;
                }
                let (settings_fields, chatmix_list) = protocol.poll_settings().await;
                if let Some(f) = settings_fields {
                    apply_settings_fields(&mut *status.lock().await, f);
                }
                for cm in chatmix_list {
                    dispatch_chatmix(cm, &status, &chatmix_sinks).await;
                }

                // Drain any remaining unsolicited packets.
                for cm in protocol.drain_chatmix(8).await {
                    dispatch_chatmix(cm, &status, &chatmix_sinks).await;
                }
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));

        let sinks = audio::setup_chatmix_sinks(self.vendor(), self.model_name).await;
        *self.chatmix_sinks.lock().await = sinks;

        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
        if let Some(sinks) = self.chatmix_sinks.lock().await.take() {
            audio::teardown_chatmix_sinks(sinks).await;
        }
    }

    async fn serialize(&self) -> WireDevice {
        let s = self.status.lock().await;

        // The USB base station is always present once registered; only the wireless
        // headset may be offline. Show the device regardless so the user can configure
        // base-station settings even when the headset is powered off.
        let headset_online = s.power_status == POWER_ONLINE || s.power_status == POWER_CHARGING;

        let headset_battery = Battery {
            key: "headset".into(),
            label: "Headset".into(),
            level: if headset_online { s.headset_battery } else { 0 },
            status: if s.power_status == POWER_CHARGING {
                BatteryStatus::Charging
            } else if headset_online {
                BatteryStatus::Discharging
            } else {
                BatteryStatus::Unknown
            },
        };
        let slot_battery = Battery {
            key: "slot".into(),
            label: "Charging Slot".into(),
            level: s.slot_battery,
            status: BatteryStatus::Discharging,
        };

        // Inverted: value=true means "active" (not muted). Shows "Active"/"Inactive" correctly.
        let mic_muted = Boolean {
            key: "mic_active".into(),
            label: "Microphone".into(),
            value: !s.mic_muted,
            read_only: true,
            category: "Microphone".into(),
        };

        let nc_options = vec![
            ChoiceOption {
                id: "0".into(),
                label: "Off".into(),
            },
            ChoiceOption {
                id: "1".into(),
                label: "Transparent".into(),
            },
            ChoiceOption {
                id: "2".into(),
                label: "Noise Cancelling".into(),
            },
        ];
        let sidetone_options = vec![
            ChoiceOption {
                id: "0".into(),
                label: "Off".into(),
            },
            ChoiceOption {
                id: "1".into(),
                label: "Low".into(),
            },
            ChoiceOption {
                id: "2".into(),
                label: "Medium".into(),
            },
            ChoiceOption {
                id: "3".into(),
                label: "High".into(),
            },
        ];
        let wireless_options = vec![
            ChoiceOption {
                id: "0".into(),
                label: "Maximum Speed".into(),
            },
            ChoiceOption {
                id: "1".into(),
                label: "Maximum Range".into(),
            },
        ];
        let gain_options = vec![
            ChoiceOption {
                id: "0".into(),
                label: "Low".into(),
            },
            ChoiceOption {
                id: "1".into(),
                label: "High".into(),
            },
        ];
        let auto_off_options = AUTO_OFF_LABELS
            .iter()
            .enumerate()
            .map(|(i, &l)| ChoiceOption {
                id: i.to_string(),
                label: l.into(),
            })
            .collect();

        let choices = vec![
            // Microphone category
            Choice {
                key: "gain".into(),
                label: "Microphone Gain".into(),
                options: gain_options,
                selected: s.gain as usize,
                category: "Microphone".into(),
                display: ChoiceDisplay::Inline,
            },
            Choice {
                key: "sidetone".into(),
                label: "Sidetone".into(),
                options: sidetone_options,
                selected: s.sidetone as usize,
                category: "Microphone".into(),
                display: ChoiceDisplay::Inline,
            },
            // Noise Cancelling category
            Choice {
                key: "nc_mode".into(),
                label: "Mode".into(),
                options: nc_options,
                selected: s.nc_mode as usize,
                category: "Noise Cancelling".into(),
                display: ChoiceDisplay::Inline,
            },
            // Base Station category
            Choice {
                key: "wireless_mode".into(),
                label: "Wireless Mode".into(),
                options: wireless_options,
                selected: s.wireless_mode as usize,
                category: "Base Station".into(),
                display: ChoiceDisplay::Inline,
            },
            Choice {
                key: "auto_off".into(),
                label: "Auto-Off Timeout".into(),
                options: auto_off_options,
                selected: s.auto_off_raw as usize,
                category: "Base Station".into(),
                display: ChoiceDisplay::List,
            },
            Choice {
                key: "screen_mode".into(),
                label: "Screen Mode".into(),
                options: vec![
                    ChoiceOption {
                        id: "0".into(),
                        label: "Detailed".into(),
                    },
                    ChoiceOption {
                        id: "1".into(),
                        label: "Simple".into(),
                    },
                ],
                selected: s.screen_mode as usize,
                category: "Base Station".into(),
                display: ChoiceDisplay::Inline,
            },
            // Audio category
            Choice {
                key: "sonar_eq".into(),
                label: "Sonar EQ".into(),
                options: vec![
                    ChoiceOption {
                        id: "0".into(),
                        label: "Off".into(),
                    },
                    ChoiceOption {
                        id: "1".into(),
                        label: "On".into(),
                    },
                ],
                selected: s.sonar_eq as usize,
                category: "Audio".into(),
                display: ChoiceDisplay::Toggle,
            },
        ];

        let ranges = vec![
            Range {
                key: "mic_led_brightness".into(),
                label: "LED Brightness".into(),
                min: 10,
                max: 100,
                step: 10,
                value: s.mic_led_brightness as i32,
                read_only: false,
                category: "Microphone".into(),
                start_label: None,
                end_label: None,
            },
            Range {
                key: "nc_level".into(),
                label: "Transparency Level".into(),
                min: 0,
                max: 100,
                step: 10,
                value: s.nc_level as i32,
                read_only: false,
                category: "Noise Cancelling".into(),
                start_label: None,
                end_label: None,
            },
            Range {
                key: "chatmix".into(),
                label: "ChatMix".into(),
                min: 0,
                max: 100,
                step: 1,
                // game=100,chat=100→50 (center); game=100,chat=0→0; game=0,chat=100→100
                value: (s.chatmix_chat as i32 - s.chatmix_game as i32 + 100) / 2,
                read_only: true,
                category: "Audio".into(),
                start_label: Some("Media".into()),
                end_label: Some("Chat".into()),
            },
        ];

        let eq_presets = EQ_PRESET_NAMES
            .iter()
            .enumerate()
            .map(|(i, &n)| ChoiceOption {
                id: i.to_string(),
                label: n.into(),
            })
            .collect();
        let eq_bands = s
            .eq_bands
            .iter()
            .enumerate()
            .map(|(i, &v)| EqBand {
                index: i,
                label: EQ_BAND_LABELS[i].into(),
                min: -10.0,
                max: 10.0,
                step: 0.5,
                value: v,
            })
            .collect();
        let equalizer = Equalizer {
            presets: eq_presets,
            selected_preset: s.eq_preset,
            bands: eq_bands,
        };

        let mut capabilities = vec![
            DeviceCapability::Boolean(vec![mic_muted]),
            DeviceCapability::Choice(choices),
            DeviceCapability::Range(ranges),
            DeviceCapability::Equalizer(equalizer),
        ];
        if s.polled {
            capabilities.insert(
                0,
                DeviceCapability::Battery(vec![headset_battery, slot_battery]),
            );
        }

        WireDeviceBuilder::from_device(self)
            .device_type(DeviceType::Headset)
            .connected(true)
            .capabilities(capabilities)
            .build()
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![
            CapabilityRef::Battery(self),
            CapabilityRef::Boolean(self),
            CapabilityRef::Choice(self),
            CapabilityRef::Range(self),
            CapabilityRef::Equalizer(self),
        ]
    }
}

#[async_trait]
impl BatteryCapability for ArctisNovaProWireless {
    async fn get_batteries(&self) -> Result<Vec<Battery>> {
        let s = self.status.lock().await;
        let headset_online = s.power_status == POWER_ONLINE || s.power_status == POWER_CHARGING;
        Ok(vec![
            Battery {
                key: "headset".into(),
                label: "Headset".into(),
                level: if headset_online { s.headset_battery } else { 0 },
                status: if s.power_status == POWER_CHARGING {
                    BatteryStatus::Charging
                } else if headset_online {
                    BatteryStatus::Discharging
                } else {
                    BatteryStatus::Unknown
                },
            },
            Battery {
                key: "slot".into(),
                label: "Charging Slot".into(),
                level: s.slot_battery,
                status: BatteryStatus::Discharging,
            },
        ])
    }
}

#[async_trait]
impl BooleanCapability for ArctisNovaProWireless {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        let s = self.status.lock().await;
        Ok(vec![Boolean {
            key: "mic_active".into(),
            label: "Microphone".into(),
            value: !s.mic_muted,
            read_only: true,
            category: "Microphone".into(),
        }])
    }

    async fn set_boolean(&self, _key: &str, _value: bool) -> Result<()> {
        anyhow::bail!("microphone mute state is read-only on this device")
    }
}

#[async_trait]
impl ChoiceCapability for ArctisNovaProWireless {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.choice_cache
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        self.choice_cache.record(key, selected);
        let result = match key {
            "nc_mode" => self.protocol.send_nc_mode(selected.min(2) as u8).await,
            "sidetone" => self.protocol.send_sidetone(selected.min(3) as u8).await,
            "wireless_mode" => {
                self.protocol
                    .send_wireless_mode(selected.min(1) as u8)
                    .await
            }
            "gain" => self.protocol.send_mic_gain(selected != 0).await,
            "auto_off" => self.protocol.send_auto_off(selected.min(6) as u8).await,
            "sonar_eq" => self.protocol.send_sonar_eq(selected != 0).await,
            "screen_mode" => self.protocol.send_screen_mode(selected != 0).await,
            other => Err(anyhow::anyhow!("unknown choice key: {other}")),
        };
        // Update in-memory state as soon as the send attempt is resolved.
        // A persist() failure must not leave status stale — the hardware has
        // the new value and the daemon must reflect it in broadcasts.
        {
            let mut s = self.status.lock().await;
            apply_choice_to_status(key, selected, &mut s);
        }
        result?;
        let _ = self.persist().await;
        self.suppress().await;
        Ok(())
    }
}

fn apply_choice_to_status(key: &str, selected: usize, s: &mut ArctisStatus) {
    match key {
        "nc_mode" => s.nc_mode = selected.min(2) as u8,
        "sidetone" => s.sidetone = selected.min(3) as u8,
        "wireless_mode" => s.wireless_mode = selected.min(1) as u8,
        "gain" => s.gain = selected.min(1) as u8,
        "auto_off" => s.auto_off_raw = selected.min(6) as u8,
        "sonar_eq" => s.sonar_eq = selected.min(1) as u8,
        "screen_mode" => s.screen_mode = selected.min(1) as u8,
        _ => {}
    }
}

#[async_trait]
impl RangeCapability for ArctisNovaProWireless {
    fn range_cache(&self) -> &RangeStateCache {
        &self.range_cache
    }

    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        self.range_cache.record(key, value);
        match key {
            "mic_led_brightness" => {
                let clamped = value.clamp(10, 100) as u8;
                self.protocol.send_mic_led_brightness(clamped).await?;
                self.status.lock().await.mic_led_brightness = clamped;
                let _ = self.persist().await;
                self.suppress().await;
            }
            "nc_level" => {
                let clamped = value.clamp(0, 100) as u8;
                self.protocol.send_nc_level(clamped).await?;
                self.status.lock().await.nc_level = clamped;
                let _ = self.persist().await;
                self.suppress().await;
            }
            other => anyhow::bail!("unknown range key: {other}"),
        }
        Ok(())
    }
}

#[async_trait]
impl EqualizerCapability for ArctisNovaProWireless {
    async fn get_equalizer(&self) -> Result<Equalizer> {
        let s = self.status.lock().await;
        Ok(Equalizer {
            presets: EQ_PRESET_NAMES
                .iter()
                .enumerate()
                .map(|(i, &n)| ChoiceOption {
                    id: i.to_string(),
                    label: n.into(),
                })
                .collect(),
            selected_preset: s.eq_preset,
            bands: s
                .eq_bands
                .iter()
                .enumerate()
                .map(|(i, &v)| EqBand {
                    index: i,
                    label: EQ_BAND_LABELS[i].into(),
                    min: -10.0,
                    max: 10.0,
                    step: 0.5,
                    value: v,
                })
                .collect(),
        })
    }

    async fn set_eq_preset(&self, preset_index: usize) -> Result<()> {
        let clamped = preset_index.min(4);
        self.protocol.send_eq_preset(clamped as u8).await?;
        let _ = self.persist().await;
        self.suppress().await;
        let mut s = self.status.lock().await;
        s.eq_preset = clamped;
        *self.eq_cache.lock().unwrap() = Some((clamped, s.eq_bands));
        Ok(())
    }

    async fn set_eq_bands(&self, values: &[f32]) -> Result<()> {
        if values.len() != 10 {
            anyhow::bail!("expected 10 EQ band values, got {}", values.len());
        }
        self.protocol.send_eq_preset(4).await?;
        self.protocol.send_eq_bands(values).await?;
        let _ = self.persist().await;
        self.suppress().await;
        let mut s = self.status.lock().await;
        s.eq_preset = 4;
        let mut bands = [0.0f32; 10];
        for (i, &v) in values.iter().enumerate() {
            bands[i] = v.clamp(-10.0, 10.0);
            s.eq_bands[i] = bands[i];
        }
        *self.eq_cache.lock().unwrap() = Some((4, bands));
        Ok(())
    }

    fn current_state(&self) -> Option<Equalizer> {
        let cache = self.eq_cache.lock().unwrap();
        let (preset, bands) = (*cache)?;
        Some(Equalizer {
            presets: EQ_PRESET_NAMES
                .iter()
                .enumerate()
                .map(|(i, &n)| ChoiceOption {
                    id: i.to_string(),
                    label: n.into(),
                })
                .collect(),
            selected_preset: preset,
            bands: bands
                .iter()
                .enumerate()
                .map(|(i, &v)| EqBand {
                    index: i,
                    label: EQ_BAND_LABELS[i].into(),
                    min: -10.0,
                    max: 10.0,
                    step: 0.5,
                    value: v,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── apply_choice_to_status ────────────────────────────────────────────

    fn default_status() -> ArctisStatus {
        ArctisStatus::default()
    }

    #[test]
    fn apply_choice_to_status_nc_mode() {
        let mut s = default_status();
        apply_choice_to_status("nc_mode", 2, &mut s);
        assert_eq!(s.nc_mode, 2);
    }

    #[test]
    fn apply_choice_to_status_nc_mode_clamps_above_2() {
        let mut s = default_status();
        apply_choice_to_status("nc_mode", 99, &mut s);
        assert_eq!(s.nc_mode, 2);
    }

    #[test]
    fn apply_choice_to_status_sidetone() {
        let mut s = default_status();
        apply_choice_to_status("sidetone", 3, &mut s);
        assert_eq!(s.sidetone, 3);
    }

    #[test]
    fn apply_choice_to_status_sidetone_clamps_above_3() {
        let mut s = default_status();
        apply_choice_to_status("sidetone", 99, &mut s);
        assert_eq!(s.sidetone, 3);
    }

    #[test]
    fn apply_choice_to_status_wireless_mode() {
        let mut s = default_status();
        apply_choice_to_status("wireless_mode", 1, &mut s);
        assert_eq!(s.wireless_mode, 1);
    }

    #[test]
    fn apply_choice_to_status_wireless_mode_clamps_above_1() {
        let mut s = default_status();
        apply_choice_to_status("wireless_mode", 5, &mut s);
        assert_eq!(s.wireless_mode, 1);
    }

    #[test]
    fn apply_choice_to_status_gain() {
        let mut s = default_status();
        apply_choice_to_status("gain", 1, &mut s);
        assert_eq!(s.gain, 1);
    }

    #[test]
    fn apply_choice_to_status_gain_clamps_above_1() {
        let mut s = default_status();
        apply_choice_to_status("gain", 9, &mut s);
        assert_eq!(s.gain, 1);
    }

    #[test]
    fn apply_choice_to_status_auto_off() {
        let mut s = default_status();
        apply_choice_to_status("auto_off", 4, &mut s);
        assert_eq!(s.auto_off_raw, 4);
    }

    #[test]
    fn apply_choice_to_status_auto_off_clamps_above_6() {
        let mut s = default_status();
        apply_choice_to_status("auto_off", 99, &mut s);
        assert_eq!(s.auto_off_raw, 6);
    }

    #[test]
    fn apply_choice_to_status_sonar_eq() {
        let mut s = default_status();
        apply_choice_to_status("sonar_eq", 1, &mut s);
        assert_eq!(s.sonar_eq, 1);
    }

    #[test]
    fn apply_choice_to_status_screen_mode() {
        let mut s = default_status();
        apply_choice_to_status("screen_mode", 1, &mut s);
        assert_eq!(s.screen_mode, 1);
    }

    #[test]
    fn apply_choice_to_status_unknown_key_is_no_op() {
        let mut s = default_status();
        s.nc_mode = 2;
        apply_choice_to_status("totally_unknown", 1, &mut s);
        assert_eq!(s.nc_mode, 2);
        assert_eq!(s.sidetone, 0);
    }

    // ── EQ persistence ────────────────────────────────────────────────────

    #[test]
    fn eq_save_state_null_before_any_set() {
        let cache: std::sync::Mutex<Option<(usize, [f32; 10])>> = std::sync::Mutex::new(None);
        let saved = match *cache.lock().unwrap() {
            None => serde_json::Value::Null,
            Some((preset, bands)) => serde_json::json!({"preset": preset, "bands": bands}),
        };
        assert!(saved.is_null(), "save_state must be null before any set");
    }

    #[test]
    fn eq_cache_save_roundtrip_preset_only() {
        let cache: std::sync::Mutex<Option<(usize, [f32; 10])>> =
            std::sync::Mutex::new(Some((2, [0.0f32; 10])));
        let saved = match *cache.lock().unwrap() {
            None => serde_json::Value::Null,
            Some((preset, bands)) => serde_json::json!({"preset": preset, "bands": bands}),
        };
        assert_eq!(saved["preset"].as_u64().unwrap(), 2);
        assert_eq!(saved["bands"].as_array().unwrap().len(), 10);
    }

    #[test]
    fn eq_cache_save_roundtrip_with_bands() {
        let bands = [1.0f32, -1.0, 2.5, 0.0, -3.0, 4.0, 0.5, -0.5, 3.5, -2.0];
        let cache: std::sync::Mutex<Option<(usize, [f32; 10])>> =
            std::sync::Mutex::new(Some((4, bands)));
        let saved = match *cache.lock().unwrap() {
            None => serde_json::Value::Null,
            Some((preset, b)) => serde_json::json!({"preset": preset, "bands": b}),
        };
        let saved_bands: Vec<f64> = saved["bands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        assert!((saved_bands[0] - 1.0).abs() < 0.001);
        assert!((saved_bands[2] - 2.5).abs() < 0.001);
        assert_eq!(saved["preset"].as_u64().unwrap(), 4);
    }
}
