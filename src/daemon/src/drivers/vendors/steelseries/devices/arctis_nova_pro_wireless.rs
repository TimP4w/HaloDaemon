// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: linux-arctis-manager contributors <https://github.com/elegos/Linux-Arctis-Manager>
// Protocol reference: linux-arctis-manager (GPL-3.0) and sennheiser-gsx-control (MIT)

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as TokioMutex;

use crate::{
    drivers::{
        transports::{hid::HidTransport, Transport},
        vendors::generic::devices::common::{TaskHandle, WireDeviceBuilder},
        vendors::steelseries::protocols::steelseries_arctis::{
            ArctisChatMixFields, ArctisPoll, ArctisProtocol, ArctisSettingsFields,
            ArctisStatusFields, POWER_CHARGING, POWER_OFFLINE, POWER_ONLINE,
        },
        BatteryCapability, BooleanCapability, CapabilityRef, ChoiceCapability, ChoiceStateCache,
        Device, EqualizerCapability, RangeCapability, RangeStateCache,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle},
    services::audio::sink::{self as audio, Sink},
};
use halod_shared::types::{
    Battery, BatteryStatus, Boolean, CategoryLayout, Choice, ChoiceDisplay, ChoiceOption,
    DeviceCapability, DeviceType, EqBand, EqPreset, Equalizer, Range, VisibleWhen, WireDevice,
};

static ARCTIS_IDS: &[(u16, u16, &str)] = &[
    (0x1038, 0x12E0, "Arctis Nova Pro Wireless"),
    (0x1038, 0x12E5, "Arctis Nova Pro Wireless X"),
    (0x1038, 0x225D, "Arctis Nova Pro Wireless X"),
];

const BT_VARIANT_PIDS: &[u16] = &[0x12E5, 0x225D];

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

// Only settings the headset never reports back are re-applied on startup, to avoid clobbering NVRAM-persisted state.
const NON_READBACK_CHOICES: &[&str] = &["sonar_eq", "screen_mode"]; // TODO: verify this list is complete
const NON_READBACK_RANGES: &[&str] = &["mic_volume"];

const EQ_BAND_LABELS: [&str; 10] = [
    "31 Hz", "62 Hz", "125 Hz", "250 Hz", "500 Hz", "1 kHz", "2 kHz", "4 kHz", "8 kHz", "16 kHz",
];
/// EQ presets as `(device byte, label, info bands)`; bytes are non-contiguous, so
/// dropdown position is decoupled from the wire value. The info bands mirror each
/// firmware curve for display only (Custom carries none) — selecting a firmware
/// preset writes its byte, not the bands; editing any band switches to Custom.
const EQ_PRESETS: &[(u8, &str, &[f32])] = &[
    (0x00, "Flat", &[0.0; 10]),
    (
        0x01,
        "Bass Boost",
        &[3.5, 5.5, 4.0, 1.0, -1.5, -1.5, -1.0, -1.0, -1.0, -1.0],
    ),
    (
        0x02,
        "Focus",
        &[-5.0, -3.5, -1.0, -3.5, -2.5, 4.0, 6.0, 3.5, -3.5, 0.0],
    ),
    (
        0x03,
        "Smiley",
        &[3.0, 3.5, 1.5, -1.5, -4.0, -4.0, -2.5, 1.5, 3.0, 4.0],
    ),
    (0x04, "Custom", &[]),
    (
        0x05,
        "Apex Legends",
        &[-10.0, -6.0, 6.0, 0.0, 0.0, 4.0, 0.0, 6.5, 9.5, 4.0],
    ),
    (
        0x07,
        "Call of Duty: MWII",
        &[0.0, 1.5, 6.0, 2.5, 0.0, 0.0, 2.0, 1.0, 6.0, 4.0],
    ),
    (
        0x08,
        "Call of Duty: Warzone",
        &[-10.0, -3.0, 6.0, 0.0, 2.5, 2.0, 1.5, 1.0, 4.0, 1.5],
    ),
    (
        0x0c,
        "FPS Footsteps",
        &[-10.0, -2.0, 5.0, 4.0, 0.0, -1.5, -1.5, 1.5, 3.0, 1.5],
    ),
    (
        0x0d,
        "GTA V",
        &[3.0, 7.5, 6.0, -1.5, 0.0, 1.0, 1.5, 2.5, 3.0, 3.0],
    ),
    (
        0x0f,
        "Overwatch 2",
        &[-6.0, -4.0, 1.0, -2.0, -1.0, 0.0, 0.0, 0.0, 3.0, 6.0],
    ),
    (
        0x10,
        "PUBG",
        &[-10.0, -6.0, -1.0, 0.0, 2.0, 5.0, 2.5, -4.0, 3.5, 1.5],
    ),
    // Curves confirmed, device bytes not yet captured on hardware. Uncomment each once
    // its wire byte is known (an invalid byte would silently fall back to Custom).
    // (0x??, "Baldur's Gate 3", &[0.0, 5.0, 6.0, 3.0, -1.0, 0.0, 3.0, 1.5, 4.0, 2.5]),
    // (0x??, "Destiny 2", &[-3.0, 1.5, 0.0, 0.0, -1.5, 2.0, 1.0, 2.0, 1.5, 0.0]),
    // (0x??, "Diablo IV", &[-2.0, 3.0, 1.5, -2.5, 1.0, -1.5, -1.0, 1.0, 2.0, 4.0]),
    // (0x??, "Fortnite", &[0.0, 6.0, 3.0, 3.5, 4.0, -1.5, -1.0, 2.5, 4.5, 1.5]),
    // (0x??, "Minecraft", &[2.0, 6.0, 2.5, -1.0, 0.0, 4.0, 0.0, 0.0, 3.0, 6.0]),
    // (0x??, "Rainbow Six Siege", &[-10.0, -6.0, -1.5, 4.5, -0.5, 6.0, 3.0, -6.0, 0.0, 3.0]),
    // (0x??, "Rocket League", &[2.5, 7.5, 6.0, 3.5, 2.0, 0.0, 1.5, 3.0, 4.5, 2.0]),
];
/// Wire byte for the editable Custom preset.
const EQ_CUSTOM_BYTE: u8 = 0x04;

/// Noise-cancelling mode options; index == wire value (0=off, 1=transparent, 2=on).
fn nc_mode_options() -> Vec<ChoiceOption> {
    ["Off", "Transparent", "Noise Cancelling"]
        .iter()
        .enumerate()
        .map(|(i, &label)| ChoiceOption {
            id: i.to_string(),
            label: label.into(),
        })
        .collect()
}

fn eq_preset_options() -> Vec<EqPreset> {
    EQ_PRESETS
        .iter()
        .map(|&(byte, name, bands)| EqPreset {
            id: byte.to_string(),
            label: name.into(),
            is_custom: byte == EQ_CUSTOM_BYTE,
            is_firmware: byte != EQ_CUSTOM_BYTE,
            bands: (!bands.is_empty()).then(|| bands.to_vec()),
        })
        .collect()
}

/// Dropdown position of a device preset byte; uncatalogued bytes fall back to Custom.
fn eq_preset_index(byte: u8) -> usize {
    EQ_PRESETS
        .iter()
        .position(|&(b, _, _)| b == byte)
        .or_else(|| EQ_PRESETS.iter().position(|&(b, _, _)| b == EQ_CUSTOM_BYTE))
        .unwrap_or(0)
}

/// Device preset byte for a dropdown position; out-of-range falls back to Custom.
fn eq_preset_byte(index: usize) -> u8 {
    EQ_PRESETS
        .get(index)
        .map(|&(b, _, _)| b)
        .unwrap_or(EQ_CUSTOM_BYTE)
}

fn build_equalizer(preset_byte: u8, custom_bands: &[f32; 10]) -> Equalizer {
    let presets = eq_preset_options();
    let selected_preset = eq_preset_index(preset_byte);
    let selected = presets.get(selected_preset);
    // A preset is editable when it's the custom curve or carries its own bands.
    let editable = selected.is_some_and(|p| p.is_custom || p.bands.is_some());
    // Show the selected firmware preset's info curve, else the live custom curve.
    let values = selected
        .and_then(|p| p.bands.clone())
        .unwrap_or_else(|| custom_bands.to_vec());
    Equalizer {
        selected_preset,
        editable,
        presets,
        bands: EQ_BAND_LABELS
            .iter()
            .enumerate()
            .map(|(i, &label)| EqBand {
                index: i,
                label: label.into(),
                min: -10.0,
                max: 10.0,
                step: 0.5,
                value: values.get(i).copied().unwrap_or(0.0),
            })
            .collect(),
    }
}

const AUTO_OFF_LABELS: [&str; 7] = [
    "Off", "1 min", "5 min", "10 min", "15 min", "30 min", "60 min",
];

struct ArctisStatus {
    headset_battery: u8,
    slot_battery: u8,
    power_status: u8,
    mic_muted: bool,
    nc_mode: u8,
    nc_level: u8,      // transparency level 1–10
    wireless_mode: u8, // 0=speed 1=range
    auto_off_raw: u8,  // 0x00–0x06
    gain: u8,          // 0=low 1=high
    sidetone: u8,      // 0=off 1=low 2=medium 3=high
    eq_preset: u8,     // raw device preset byte (0–4 standard, higher = game presets)
    eq_bands: [f32; 10],
    suppress_until: Instant,
    chatmix_game: u8,       // 0–100, volume for game/media sink
    chatmix_chat: u8,       // 0–100, volume for chat sink
    sonar_eq: u8,           // 0=off 1=on  (cmd 0x8d)
    screen_mode: u8,        // 0=detailed 1=simple  (cmd 0x89)
    mic_led_brightness: u8, // 0–100 in steps of 10 (raw 0–10, cmd 0xbf)
    mic_volume: u8,         // capture level 1–10 (cmd 0x37)
    station_volume: u8,     // base-station main volume 0–100 (notify 0x25)
    // Bluetooth status block (X/BT variants only); raw bytes from the 06 B0 response.
    bt_powerup_state: u8,    // 0x02
    bt_auto_mute: u8,        // 0x03
    bt_power_status: u8,     // 0x04
    bt_connection: u8,       // 0x05
    bt_wireless_pairing: u8, // 0x0E
    // Guards against showing 0% before the first status poll completes.
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
            mic_volume: 10,
            station_volume: 100,
            bt_powerup_state: 0,
            bt_auto_mute: 0,
            bt_power_status: 0,
            bt_connection: 0,
            bt_wireless_pairing: 0,
            polled: false,
        }
    }
}

fn build_batteries(s: &ArctisStatus) -> Vec<Battery> {
    // The base station is always present; only the headset may be offline.
    let headset_online = s.power_status == POWER_ONLINE || s.power_status == POWER_CHARGING;
    vec![
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
    ]
}

/// Range controls; `nc_level` (Transparency Level) only applies in
/// Transparent ANC mode (`nc_mode` == 1), so it stays hidden otherwise.
fn build_ranges(s: &ArctisStatus) -> Vec<Range> {
    vec![
        Range {
            key: "mic_volume".into(),
            label: "Microphone Volume".into(),
            min: 1,
            max: 10,
            step: 1,
            value: s.mic_volume as i32,
            read_only: false,
            category: "Microphone".into(),
            start_label: None,
            end_label: None,
            display: Default::default(),
            visible_when: None,
        },
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
            display: Default::default(),
            visible_when: None,
        },
        Range {
            key: "nc_level".into(),
            label: "Transparency Level".into(),
            min: 1,
            max: 10,
            step: 1,
            value: s.nc_level as i32,
            read_only: false,
            category: "Noise Cancelling".into(),
            start_label: None,
            end_label: None,
            display: Default::default(),
            visible_when: Some(VisibleWhen {
                key: "nc_mode".into(),
                equals: vec![1],
            }),
        },
        Range {
            key: "station_volume".into(),
            label: "Volume".into(),
            min: 0,
            max: 100,
            step: 1,
            value: s.station_volume as i32,
            read_only: true,
            category: "Base Station".into(),
            start_label: None,
            end_label: None,
            display: Default::default(),
            visible_when: None,
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
            display: Default::default(),
            visible_when: None,
        },
    ]
}

/// Read-only status booleans.
fn build_booleans(s: &ArctisStatus, bt_variant: bool) -> Vec<Boolean> {
    // Inverted: value=true means "active" (not muted). Shows "Active"/"Inactive".
    let mut booleans = vec![Boolean {
        key: "mic_active".into(),
        label: "Microphone".into(),
        value: !s.mic_muted,
        read_only: true,
        category: "Microphone".into(),
        visible_when: None,
    }];
    if bt_variant {
        booleans.push(Boolean {
            key: "bt_connection".into(),
            label: "Bluetooth".into(),
            value: s.bt_connection != 0,
            read_only: true,
            category: "Bluetooth".into(),
            visible_when: None,
        });
        booleans.push(Boolean {
            key: "bt_auto_mute".into(),
            label: "Auto-Mute".into(),
            value: s.bt_auto_mute != 0,
            read_only: true,
            category: "Bluetooth".into(),
            visible_when: None,
        });
    }
    booleans
}

/// The pair of virtual sinks backing ChatMix: game/media audio and chat audio,
/// each looped into the headset's physical sink so the dial can balance them.
struct ChatMixSinks {
    media: Arc<Sink>,
    chat: Arc<Sink>,
}

pub struct ArctisNovaProWireless {
    id: String,
    model_name: &'static str,
    vid: u16,
    pid: u16,
    bt_variant: bool,
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
    let sinks = {
        let guard = chatmix_sinks.lock().await;
        guard
            .as_ref()
            .map(|s| (Arc::clone(&s.media), Arc::clone(&s.chat)))
    };
    if let Some((media, chat)) = sinks {
        media.set_volume(fields.game).await;
        chat.set_volume(fields.chat).await;
    }
}

/// Apply one polling pass: fold status/settings into the shared state and push
/// any ChatMix updates to the audio sinks.
async fn apply_poll(
    poll: ArctisPoll,
    status: &Arc<TokioMutex<ArctisStatus>>,
    chatmix_sinks: &Arc<TokioMutex<Option<ChatMixSinks>>>,
) {
    {
        let mut s = status.lock().await;
        if let Some(f) = poll.status {
            apply_status_fields(&mut s, f);
        }
        if let Some(f) = poll.settings {
            apply_settings_fields(&mut s, f);
        }
        if let Some(level) = poll.mic_volume_raw {
            s.mic_volume = level.clamp(1, 10);
        }
        if let Some(level) = poll.station_volume_raw {
            s.station_volume = station_volume_percent(level);
        }
    }
    for cm in poll.chatmix {
        dispatch_chatmix(cm, status, chatmix_sinks).await;
    }
}

const STATION_VOL_FLOOR_DB: i32 = -56;

fn station_volume_percent(raw: u8) -> u8 {
    let db = (raw as i8) as i32;
    let pct = (db - STATION_VOL_FLOOR_DB) * 100 / -STATION_VOL_FLOOR_DB;
    pct.clamp(0, 100) as u8
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
    s.bt_powerup_state = f.bt_powerup_state;
    s.bt_auto_mute = f.bt_auto_mute;
    s.bt_power_status = f.bt_power_status;
    s.bt_connection = f.bt_connection;
    s.bt_wireless_pairing = f.bt_wireless_pairing;
    s.polled = true;
}

fn apply_settings_fields(s: &mut ArctisStatus, f: ArctisSettingsFields) {
    s.gain = f.gain;
    s.eq_preset = f.eq_preset;
    s.sidetone = f.sidetone;
    s.eq_bands = f.eq_bands;
}

impl ArctisNovaProWireless {
    pub fn new(path: &str, pid: u16, index: usize) -> Result<Self> {
        let entry = ARCTIS_IDS.iter().find(|&&(_, p, _)| p == pid);
        let vid = entry.map(|&(v, _, _)| v).unwrap_or(0x1038);
        let model_name = entry
            .map(|&(_, _, name)| name)
            .unwrap_or("Arctis Nova Pro Wireless");
        Ok(Self {
            id: format!("steelseries_arctis_{index}"),
            model_name,
            vid,
            pid,
            bt_variant: BT_VARIANT_PIDS.contains(&pid),
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

    /// Register the two ChatMix virtual sinks (media + chat) against this
    /// headset's physical sink. Returns `None` (and tears down any partial
    /// progress) if either sink could not be created.
    async fn setup_chatmix_sinks(&self) -> Option<ChatMixSinks> {
        let base = format!("{} {}", self.vendor(), self.model_name);
        let media = audio::register_sink(self.vid, self.pid, &format!("{base} Media")).await;
        let chat = audio::register_sink(self.vid, self.pid, &format!("{base} Chat")).await;

        match (media, chat) {
            (Some(media), Some(chat)) => Some(ChatMixSinks {
                media: Arc::new(media),
                chat: Arc::new(chat),
            }),
            (media, chat) => {
                if let Some(media) = media {
                    media.remove().await;
                }
                if let Some(chat) = chat {
                    chat.remove().await;
                }
                None
            }
        }
    }
}

#[async_trait]
impl Device for ArctisNovaProWireless {
    fn id(&self) -> &str {
        &self.id
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

        let poll = tokio::spawn(async move {
            // Initial fetch so state is populated before the first poll interval.
            tokio::time::sleep(Duration::from_millis(300)).await;
            apply_poll(protocol.poll().await, &status, &chatmix_sinks).await;

            let mut interval = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
            loop {
                interval.tick().await;

                let suppressed = status.lock().await.suppress_until > Instant::now();
                if suppressed {
                    continue;
                }

                apply_poll(protocol.poll().await, &status, &chatmix_sinks).await;
            }
        });

        // Supervisor: log if the poll task exits unexpectedly (e.g. due to a panic).
        // An AbortHandle guard ensures the inner task is also cancelled when the
        // supervisor is aborted (via TaskHandle drop on close()).
        let abort_poll = poll.abort_handle();
        let handle = tokio::spawn(async move {
            struct AbortOnDrop(tokio::task::AbortHandle);
            impl Drop for AbortOnDrop {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _abort_guard = AbortOnDrop(abort_poll);
            match poll.await {
                Ok(()) => {}
                Err(e) if e.is_cancelled() => {}
                Err(e) => log::error!("[Arctis] poll task exited unexpectedly: {e}"),
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));

        *self.chatmix_sinks.lock().await = self.setup_chatmix_sinks().await;

        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
        if let Some(sinks) = self.chatmix_sinks.lock().await.take() {
            sinks.media.remove().await;
            sinks.chat.remove().await;
        }
    }

    async fn serialize(&self) -> WireDevice {
        let s = self.status.lock().await;

        let booleans = build_booleans(&s, self.bt_variant);

        let nc_options = nc_mode_options();
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
            Choice {
                key: "gain".into(),
                label: "Microphone Gain".into(),
                options: gain_options,
                selected: s.gain as usize,
                category: "Microphone".into(),
                display: ChoiceDisplay::Inline,
                visible_when: None,
            },
            Choice {
                key: "sidetone".into(),
                label: "Sidetone".into(),
                options: sidetone_options,
                selected: s.sidetone as usize,
                category: "Microphone".into(),
                display: ChoiceDisplay::Inline,
                visible_when: None,
            },
            Choice {
                key: "nc_mode".into(),
                label: "Mode".into(),
                options: nc_options,
                selected: s.nc_mode as usize,
                category: "Noise Cancelling".into(),
                display: ChoiceDisplay::Inline,
                visible_when: None,
            },
            Choice {
                key: "wireless_mode".into(),
                label: "Wireless Mode".into(),
                options: wireless_options,
                selected: s.wireless_mode as usize,
                category: "Base Station".into(),
                display: ChoiceDisplay::Inline,
                visible_when: None,
            },
            Choice {
                key: "auto_off".into(),
                label: "Auto-Off Timeout".into(),
                options: auto_off_options,
                selected: s.auto_off_raw as usize,
                category: "Base Station".into(),
                display: ChoiceDisplay::List,
                visible_when: None,
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
                visible_when: None,
            },
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
                visible_when: None,
            },
        ];

        let ranges = build_ranges(&s);

        let equalizer = build_equalizer(s.eq_preset, &s.eq_bands);

        let mut capabilities = vec![
            DeviceCapability::Boolean(booleans),
            DeviceCapability::Choice(choices),
            DeviceCapability::Range(ranges),
            DeviceCapability::Equalizer(equalizer),
        ];
        if s.polled {
            capabilities.insert(0, DeviceCapability::Battery(build_batteries(&s)));
        }

        WireDeviceBuilder::from_device(self)
            .device_type(DeviceType::Headset)
            .connected(true)
            .capabilities(capabilities)
            .control_layout(vec![
                CategoryLayout {
                    category: "Microphone".into(),
                    order: 0,
                    column: 0,
                    span: 1,
                },
                CategoryLayout {
                    category: "Noise Cancelling".into(),
                    order: 1,
                    column: 1,
                    span: 1,
                },
            ])
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

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        Some(self.protocol.transport.rate_status())
    }
}

#[async_trait]
impl BatteryCapability for ArctisNovaProWireless {
    async fn get_batteries(&self) -> Result<Vec<Battery>> {
        let s = self.status.lock().await;
        Ok(build_batteries(&s))
    }
}

#[async_trait]
impl BooleanCapability for ArctisNovaProWireless {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        let s = self.status.lock().await;
        Ok(build_booleans(&s, self.bt_variant))
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

    async fn restore_state(&self, v: &serde_json::Value) {
        for (key, selected) in self.choice_cache.load_pairs(v) {
            if NON_READBACK_CHOICES.contains(&key.as_str()) {
                let _ = self.set_choice(&key, selected).await;
            }
        }
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
        // Reflect the new value immediately, even if persist() later fails — the
        // hardware already has it and broadcasts must not go stale.
        {
            let mut s = self.status.lock().await;
            apply_choice_to_status(key, selected, &mut s);
        }
        result?;
        if let Err(e) = self.persist().await {
            log::warn!("[Arctis] persist failed: {e}");
        }
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

    async fn restore_state(&self, v: &serde_json::Value) {
        for (key, value) in self.range_cache.load_pairs(v) {
            if NON_READBACK_RANGES.contains(&key.as_str()) {
                let _ = self.set_range(&key, value).await;
            }
        }
    }

    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        self.range_cache.record(key, value);
        match key {
            "mic_volume" => {
                let clamped = value.clamp(1, 10) as u8;
                self.protocol.send_mic_volume(clamped).await?;
                self.status.lock().await.mic_volume = clamped;
                if let Err(e) = self.persist().await {
                    log::warn!("[Arctis] persist failed: {e}");
                }
                self.suppress().await;
            }
            "mic_led_brightness" => {
                let clamped = value.clamp(10, 100) as u8;
                self.protocol.send_mic_led_brightness(clamped).await?;
                self.status.lock().await.mic_led_brightness = clamped;
                if let Err(e) = self.persist().await {
                    log::warn!("[Arctis] persist failed: {e}");
                }
                self.suppress().await;
            }
            "nc_level" => {
                let clamped = value.clamp(1, 10) as u8;
                self.protocol.send_nc_level(clamped).await?;
                self.status.lock().await.nc_level = clamped;
                if let Err(e) = self.persist().await {
                    log::warn!("[Arctis] persist failed: {e}");
                }
                self.suppress().await;
            }
            other => anyhow::bail!("unknown range key: {other}"),
        }
        Ok(())
    }
}

#[async_trait]
impl EqualizerCapability for ArctisNovaProWireless {
    /// EQ is read back from the headset each poll, so the hardware stays authoritative.
    async fn restore_state(&self, _v: &serde_json::Value) {}

    async fn get_equalizer(&self) -> Result<Equalizer> {
        let s = self.status.lock().await;
        Ok(build_equalizer(s.eq_preset, &s.eq_bands))
    }

    async fn set_eq_preset(&self, preset_index: usize) -> Result<()> {
        // Every preset here is firmware (writes a device byte) or the custom byte. A
        // future non-firmware "software" preset would instead push its `bands` via the
        // custom path and retain the logical selection across readback.
        let byte = eq_preset_byte(preset_index);
        self.protocol.send_eq_preset(byte).await?;
        if let Err(e) = self.persist().await {
            log::warn!("[Arctis] persist failed: {e}");
        }
        self.suppress().await;
        let mut s = self.status.lock().await;
        s.eq_preset = byte;
        *self.eq_cache.lock().unwrap() = Some((byte as usize, s.eq_bands));
        Ok(())
    }

    async fn set_eq_bands(&self, values: &[f32]) -> Result<()> {
        if values.len() != 10 {
            anyhow::bail!("expected 10 EQ band values, got {}", values.len());
        }
        self.protocol.send_eq_preset(EQ_CUSTOM_BYTE).await?;
        self.protocol.send_eq_bands(values).await?;
        if let Err(e) = self.persist().await {
            log::warn!("[Arctis] persist failed: {e}");
        }
        self.suppress().await;
        let mut s = self.status.lock().await;
        s.eq_preset = EQ_CUSTOM_BYTE;
        let mut bands = [0.0f32; 10];
        for (i, &v) in values.iter().enumerate() {
            bands[i] = v.clamp(-10.0, 10.0);
            s.eq_bands[i] = bands[i];
        }
        *self.eq_cache.lock().unwrap() = Some((EQ_CUSTOM_BYTE as usize, bands));
        Ok(())
    }

    fn current_state(&self) -> Option<Equalizer> {
        let cache = self.eq_cache.lock().unwrap();
        let (preset, bands) = (*cache)?;
        Some(build_equalizer(preset as u8, &bands))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_status() -> ArctisStatus {
        ArctisStatus::default()
    }

    #[test]
    fn build_ranges_gates_transparency_level_on_transparent_anc() {
        let ranges = build_ranges(&default_status());
        let nc_level = ranges.iter().find(|r| r.key == "nc_level").unwrap();
        assert_eq!(
            nc_level.visible_when,
            Some(VisibleWhen {
                key: "nc_mode".into(),
                equals: vec![1],
            })
        );
        // Every other range stays unconditionally visible.
        assert!(ranges
            .iter()
            .filter(|r| r.key != "nc_level")
            .all(|r| r.visible_when.is_none()));
    }

    #[test]
    fn eq_preset_options_flags_custom_and_firmware() {
        let opts = eq_preset_options();
        for (i, o) in opts.iter().enumerate() {
            let (byte, _, bands) = EQ_PRESETS[i];
            let is_custom = byte == EQ_CUSTOM_BYTE;
            assert_eq!(o.is_custom, is_custom, "{}", o.label);
            assert_eq!(o.is_firmware, !is_custom, "{}", o.label);
            assert_eq!(o.bands.is_some(), !bands.is_empty(), "{}", o.label);
        }
        assert_eq!(opts.iter().filter(|o| o.is_custom).count(), 1);
        // Custom never carries info bands.
        assert!(opts.iter().find(|o| o.is_custom).unwrap().bands.is_none());
    }

    #[test]
    fn build_equalizer_shows_preset_bands_and_editability() {
        let custom = [1.0f32; 10];
        // Custom: editable, shows the live custom curve.
        let eq = build_equalizer(EQ_CUSTOM_BYTE, &custom);
        assert!(eq.editable);
        assert!(eq.bands.iter().all(|b| b.value == 1.0));
        // Firmware preset with info bands: editable, shows its own curve (not custom).
        let flat = build_equalizer(0x00, &custom);
        assert!(flat.editable);
        assert!(flat.bands.iter().all(|b| b.value == 0.0));
        // An uncatalogued byte resolves to Custom: editable, live custom curve.
        let unknown = build_equalizer(0x99, &custom);
        assert_eq!(unknown.selected_preset, eq_preset_index(EQ_CUSTOM_BYTE));
        assert!(unknown.editable);
        assert!(unknown.bands.iter().all(|b| b.value == 1.0));
    }

    #[test]
    fn build_batteries_reflects_power_status() {
        let mut s = default_status();
        s.headset_battery = 80;
        s.slot_battery = 55;

        // Offline: headset level zeroed and status Unknown; slot still reported.
        s.power_status = POWER_OFFLINE;
        let b = build_batteries(&s);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].key, "headset");
        assert_eq!(b[0].level, 0);
        assert_eq!(b[0].status, BatteryStatus::Unknown);
        assert_eq!(b[1].key, "slot");
        assert_eq!(b[1].level, 55);

        // Online: real level, discharging.
        s.power_status = POWER_ONLINE;
        let b = build_batteries(&s);
        assert_eq!(b[0].level, 80);
        assert_eq!(b[0].status, BatteryStatus::Discharging);

        // Charging.
        s.power_status = POWER_CHARGING;
        assert_eq!(build_batteries(&s)[0].status, BatteryStatus::Charging);
    }

    #[test]
    fn station_volume_percent_inverts_signed_db() {
        // 0 dB is full volume; the dB floor is the minimum.
        assert_eq!(station_volume_percent(0x00), 100);
        assert_eq!(
            station_volume_percent((STATION_VOL_FLOOR_DB as i8) as u8),
            0
        );
        // -55 dB (0xC9) clamps to the bottom of the range, not 100.
        assert_eq!(station_volume_percent(0xC9), 1);
        // Anything past the floor stays clamped to 0.
        assert_eq!(station_volume_percent(0x80), 0);
    }

    #[test]
    fn build_booleans_omits_bluetooth_on_non_bt_variant() {
        let s = default_status();
        let b = build_booleans(&s, false);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].key, "mic_active");
    }

    #[test]
    fn build_booleans_adds_bluetooth_on_bt_variant() {
        let mut s = default_status();
        s.bt_connection = 1;
        s.bt_auto_mute = 1;
        let b = build_booleans(&s, true);
        assert_eq!(b.len(), 3);
        let conn = b.iter().find(|x| x.key == "bt_connection").unwrap();
        assert!(conn.value);
        assert!(conn.read_only);
        assert_eq!(conn.category, "Bluetooth");
        let mute = b.iter().find(|x| x.key == "bt_auto_mute").unwrap();
        assert!(mute.value);
    }

    #[test]
    fn bt_variant_pids_match_x_base_stations() {
        assert!(BT_VARIANT_PIDS.contains(&0x12E5));
        assert!(BT_VARIANT_PIDS.contains(&0x225D));
        assert!(!BT_VARIANT_PIDS.contains(&0x12E0));
    }

    #[test]
    fn nc_mode_options_match_documented_wire_values() {
        let options = nc_mode_options();
        assert_eq!(options[0].id, "0");
        assert_eq!(options[0].label, "Off");
        assert_eq!(options[1].id, "1");
        assert_eq!(options[1].label, "Transparent");
        assert_eq!(options[2].id, "2");
        assert_eq!(options[2].label, "Noise Cancelling");
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
