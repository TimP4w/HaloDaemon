// SPDX-License-Identifier: GPL-3.0-or-later
//! Logitech device profiles — the static per-model descriptor table, WPID/PID
//! constants, keyboard-layout statics, and device registration.

use std::sync::Arc;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::protocols::hidpp::DirectReport;
use crate::registry::discovery::{DeviceDescriptor, DiscoveryHandle};
use halod_shared::keyboard::{KeyCell, KeyEdit, KeyId, KeyLayoutSpec, StandardLayout};
use halod_shared::types::{
    ButtonAction, ButtonDescriptor, ButtonMapping, CycleDir, DeviceType, KeyboardFormFactor,
    KeyboardLayout, ZoneTopology,
};

// ── WPID constants ────────────────────────────────────────────────────────────

/// Logitech USB vendor ID.
pub(super) const LOGITECH_VID: u16 = 0x046D;
/// HID++ vendor interface number for directly-connected (wired) devices.
/// Windows splits this interface into two HID collections (short/long reports);
/// the shared `hidpp::collection` resolver handles the split.
pub(super) const WIRED_HIDPP_INTERFACE: i32 = 2;

pub const WPID_G502X_PLUS: u16 = 0x4099;
pub const WPID_GPROX_TKL: u16 = 0x40B0;

/// Wired USB PIDs — same HID++ 2.0 protocol, devnum = 0xFF (direct).
pub const WIRED_PID_G502X_PLUS: u16 = 0xC095;
pub const WIRED_PID_G502_HERO: u16 = 0xC08B;
pub const WIRED_PID_GPROX_TKL: u16 = 0xC352;

// ── Device profiles ───────────────────────────────────────────────────────────

pub(super) struct LogitechZoneInfo {
    pub(super) name: &'static str,
    pub(super) topology: ZoneTopology,
    pub(super) led_count: u8,
}

pub(super) struct LogitechDeviceProfile {
    /// Wireless (receiver-side) PID. `None` for wired-only devices that have no
    /// LIGHTSPEED variant; those are resolved through `pid` instead.
    pub(super) wpid: Option<u16>,
    pub(super) pid: u16,
    pub(super) interface: i32,
    pub(super) report_mode: DirectReport,
    pub(super) name: &'static str,
    pub(super) device_type: DeviceType,
    pub(super) zones: &'static [LogitechZoneInfo],
    /// Native RGB effect ids advertised for this device, in display order.
    pub(super) native_effects: &'static [&'static str],
    /// Button labels for bitmap-event backends (GKEY / MOUSE_BUTTON_SPY), keyed by
    /// synthetic CID (bitmap bit index + 1). `None` → use `bitmap_button_prefix`.
    pub(super) bitmap_button_labels: Option<&'static [(u16, &'static str)]>,
    /// Fallback label prefix when no per-CID table applies (e.g. "Button", "G").
    pub(super) bitmap_button_prefix: &'static str,
    /// Out-of-the-box button actions keyed by CID, applied on first run (no saved
    /// remap config) and restored by "reset". `None` → every button stays Native.
    /// Only CIDs the device actually exposes are seeded.
    pub(super) default_buttons: Option<&'static [(u16, ButtonAction)]>,
    /// Fixed LED cid_map for a non-keyboard device (mice): orders the PER_KEY
    /// bitmap LEDs. Keyboards leave this `None` and declare `keyboard` instead.
    pub(super) led_layout: Option<&'static KeyLayoutSpec<'static>>,
    /// Per-variant keyboard layouts + language selection (keyboards only).
    pub(super) keyboard: Option<&'static LogitechKeyboardSpec>,
}

impl LogitechDeviceProfile {
    /// The effective LED `KeyLayoutSpec` for a resolved variant: a keyboard's
    /// selected-variant layout, else a mouse's fixed `led_layout`.
    pub(super) fn led_spec(
        &self,
        variant: halod_shared::keyboard::KeyVariant,
    ) -> Option<&'static KeyLayoutSpec<'static>> {
        match self.keyboard {
            Some(kb) => Some(kb.spec_for(variant)),
            None => self.led_layout,
        }
    }
}

/// A keyboard model's variant-specific key layouts plus the language/remap
/// metadata the GUI selector needs. `iso: Some(..)` *is* the ISO-support
/// declaration; a fixed-ANSI board (or a mouse) sets it to `None`.
pub(super) struct LogitechKeyboardSpec {
    pub(super) ansi: KeyLayoutSpec<'static>,
    pub(super) iso: Option<KeyLayoutSpec<'static>>,
    /// Languages the GUI offers for this model.
    pub(super) languages: &'static [KeyboardLayout],
    /// Grid `KeyId` → KeyRemap control id, for the clickable Keys-tab widget.
    /// Empty when no mapping is established (clicks disabled).
    pub(super) remap_cids: &'static [(KeyId, u16)],
}

impl LogitechKeyboardSpec {
    /// The `KeyLayoutSpec` for a resolved variant. Falls back to ANSI when the
    /// variant is ISO but the model declares no ISO layout.
    pub(super) fn spec_for(
        &self,
        variant: halod_shared::keyboard::KeyVariant,
    ) -> &KeyLayoutSpec<'static> {
        match variant {
            halod_shared::keyboard::KeyVariant::Iso => self.iso.as_ref().unwrap_or(&self.ansi),
            halod_shared::keyboard::KeyVariant::Ansi => &self.ansi,
        }
    }
}

/// G502 X Plus MOUSE_BUTTON_SPY button labels, keyed by synthetic CID (bitmap
/// bit index + 1). The bitmap is sparse — bits 3–7 (cid 4–8) have no physical
/// button — so unmapped CIDs are skipped at registration time. Mapping
/// established by physically testing each button.
static G502X_PLUS_BITMAP_BUTTONS: &[(u16, &str)] = &[
    (1, "G9"),
    (2, "G8"),
    (3, "G7"),
    (9, "Left Click"),
    (10, "Right Click"),
    (11, "Wheel Center"),
    (12, "G4"),
    (13, "Thumb Trigger"),
    (14, "G5"),
    (15, "Wheel Left"),
    (16, "Wheel Right"),
];

/// G502 X Plus out-of-the-box button actions, keyed by the synthetic CIDs in
/// `G502X_PLUS_BITMAP_BUTTONS`. G8/G7 cycle DPI; the thumb trigger acts as a
/// sniper button (drops to a low DPI while held).
static G502X_PLUS_DEFAULT_BUTTONS: &[(u16, ButtonAction)] = &[
    // G8 → DPI up.
    (
        2,
        ButtonAction::DpiCycle {
            direction: CycleDir::Up,
        },
    ),
    // G7 → DPI down.
    (
        3,
        ButtonAction::DpiCycle {
            direction: CycleDir::Down,
        },
    ),
    // Thumb trigger → sniper (momentary 400 DPI while held).
    (13, ButtonAction::MomentaryDpi { dpi: 400 }),
];

/// G502 X Plus LED reorder map — a fixed cid_map used only to order the PER_KEY
/// bitmap LEDs. A mouse, so it declares `led_layout` (below), not a `keyboard`.
static G502X_PLUS_LED_LAYOUT: KeyLayoutSpec<'static> = KeyLayoutSpec {
    base: StandardLayout::Tkl,
    edits: &[],
    cid_map: &[
        (3, KeyId::Custom(5)),
        (4, KeyId::Custom(7)),
        (8, KeyId::Custom(0)),
        (7, KeyId::Custom(1)),
        (6, KeyId::Custom(4)),
        (5, KeyId::Custom(6)),
        (2, KeyId::Custom(3)),
        (1, KeyId::Custom(2)),
    ],
};

/// G502 Hero MOUSE_BUTTON_SPY button labels. The bitmap layout differs from the
/// G502 X Plus — the G502 Hero scatters its buttons across different bit
/// positions. Mapping established via trace-level logs.
static G502_HERO_BITMAP_BUTTONS: &[(u16, &str)] = &[
    (1, "G9"),
    (9, "Left Click"),
    (10, "Right Click"),
    (11, "Wheel Center"),
    (12, "G4"),
    (13, "G5"),
    (14, "Thumb Trigger"),
    (15, "G7"),
    (16, "G8"),
];

/// G502 Hero out-of-the-box button actions. G8/G7 cycle DPI; the thumb trigger
/// acts as a sniper button.
static G502_HERO_DEFAULT_BUTTONS: &[(u16, ButtonAction)] = &[
    (
        16,
        ButtonAction::DpiCycle {
            direction: CycleDir::Up,
        },
    ),
    (
        15,
        ButtonAction::DpiCycle {
            direction: CycleDir::Down,
        },
    ),
    (14, ButtonAction::MomentaryDpi { dpi: 400 }),
];

pub(super) static DEVICE_PROFILES: &[LogitechDeviceProfile] = &[
    LogitechDeviceProfile {
        wpid: Some(WPID_G502X_PLUS),
        pid: WIRED_PID_G502X_PLUS,
        interface: WIRED_HIDPP_INTERFACE,
        report_mode: DirectReport::ShortLong,
        name: "Logitech G502 X Plus",
        device_type: DeviceType::Mouse,
        zones: &[LogitechZoneInfo {
            name: "Lighting",
            topology: ZoneTopology::Linear,
            led_count: 8,
        }],
        native_effects: &["color_wave"],
        bitmap_button_labels: Some(G502X_PLUS_BITMAP_BUTTONS),
        bitmap_button_prefix: "Button",
        default_buttons: Some(G502X_PLUS_DEFAULT_BUTTONS),
        led_layout: Some(&G502X_PLUS_LED_LAYOUT),
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: WIRED_PID_G502_HERO,
        interface: G502_HERO_HIDPP_INTERFACE,
        report_mode: DirectReport::ShortLong,
        name: "Logitech G502 Hero",
        device_type: DeviceType::Mouse,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: Some(G502_HERO_BITMAP_BUTTONS),
        bitmap_button_prefix: "Button",
        default_buttons: Some(G502_HERO_DEFAULT_BUTTONS),
        led_layout: None,
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: Some(WPID_GPROX_TKL),
        pid: WIRED_PID_GPROX_TKL,
        interface: WIRED_HIDPP_INTERFACE,
        report_mode: DirectReport::ShortLong,
        name: "Logitech G PRO X TKL",
        device_type: DeviceType::Keyboard,
        zones: &[LogitechZoneInfo {
            name: "Keys",
            topology: ZoneTopology::Keyboard {
                form_factor: KeyboardFormFactor::TKL,
                layout: KeyboardLayout::US,
            },
            led_count: 0,
        }],
        native_effects: &["ripple"],
        bitmap_button_labels: None,
        bitmap_button_prefix: "G",
        default_buttons: None,
        led_layout: None,
        keyboard: Some(&GPROX_TKL_KB_SPEC),
    },
    // ── LIGHTSPEED headsets ──────────────────────────────────────────────────
    LogitechDeviceProfile {
        wpid: None,
        pid: 0x0ABA,
        interface: COMPOSITE_HIDPP_INTERFACE,
        report_mode: DirectReport::LongOnly,
        name: "Logitech PRO X Wireless Gaming Headset",
        device_type: DeviceType::Headset,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: None,
        bitmap_button_prefix: "",
        default_buttons: None,
        led_layout: None,
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: 0x0AF7,
        interface: COMPOSITE_HIDPP_INTERFACE,
        report_mode: DirectReport::LongOnly,
        name: "Logitech PRO X 2 LIGHTSPEED",
        device_type: DeviceType::Headset,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: None,
        bitmap_button_prefix: "",
        default_buttons: None,
        led_layout: None,
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: 0x0AB5,
        interface: COMPOSITE_HIDPP_INTERFACE,
        report_mode: DirectReport::LongOnly,
        name: "Logitech G733 LIGHTSPEED",
        device_type: DeviceType::Headset,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: None,
        bitmap_button_prefix: "",
        default_buttons: None,
        led_layout: None,
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: 0x0AFE,
        interface: COMPOSITE_HIDPP_INTERFACE,
        report_mode: DirectReport::LongOnly,
        name: "Logitech G733 LIGHTSPEED",
        device_type: DeviceType::Headset,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: None,
        bitmap_button_prefix: "",
        default_buttons: None,
        led_layout: None,
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: 0x0AC4,
        interface: COMPOSITE_HIDPP_INTERFACE,
        report_mode: DirectReport::LongOnly,
        name: "Logitech G535 LIGHTSPEED",
        device_type: DeviceType::Headset,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: None,
        bitmap_button_prefix: "",
        default_buttons: None,
        led_layout: None,
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: 0x0A87,
        interface: COMPOSITE_HIDPP_INTERFACE,
        report_mode: DirectReport::LongOnly,
        name: "Logitech G935",
        device_type: DeviceType::Headset,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: None,
        bitmap_button_prefix: "",
        default_buttons: None,
        led_layout: None,
        keyboard: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: 0x0A66,
        interface: COMPOSITE_HIDPP_INTERFACE,
        report_mode: DirectReport::LongOnly,
        name: "Logitech G533",
        device_type: DeviceType::Headset,
        zones: &[],
        native_effects: &[],
        bitmap_button_labels: None,
        bitmap_button_prefix: "",
        default_buttons: None,
        led_layout: None,
        keyboard: None,
    },
];

pub(super) fn find_profile(id: u16) -> Option<&'static LogitechDeviceProfile> {
    DEVICE_PROFILES
        .iter()
        .find(|p| p.wpid == Some(id) || p.pid == id)
}

/// The device's default button mappings, restricted to the CIDs it actually
/// exposes. Empty when the profile declares no `default_buttons`. Each default
/// is a `base` action with a `Native` shift layer.
pub(super) fn default_button_mappings(
    profile: Option<&LogitechDeviceProfile>,
    buttons: &[ButtonDescriptor],
) -> Vec<ButtonMapping> {
    let Some(defaults) = profile.and_then(|p| p.default_buttons) else {
        return Vec::new();
    };
    defaults
        .iter()
        .filter(|(cid, _)| buttons.iter().any(|b| b.cid == *cid))
        .map(|(cid, action)| ButtonMapping {
            cid: *cid,
            base: action.clone(),
            shifted: ButtonAction::Native,
        })
        .collect()
}

/// Build a stable device ID that is the same for wired and wireless transport.
/// When an 8-char hex serial is available (the unit ID from HID or receiver pairing data),
/// uses `logitech_<SERIAL>` so the two transports appear as one device.
/// Falls back to `logitech_<PID/WPID>_<index>` when no serial is known.
pub(super) fn build_device_id(
    serial: Option<&str>,
    pid_or_wpid: u16,
    fallback_index: usize,
) -> String {
    match serial.filter(|s| s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit())) {
        Some(s) => format!("logitech_{}", s.to_uppercase()),
        None => format!("logitech_{pid_or_wpid:04X}_{fallback_index}"),
    }
}

// ── Discovery: pid/wpid as data ───────────────────────────────────────────────
//
// Adding a HID++ device is a one-line edit: append its pid here. There is no
// device-category axis — only the transport (direct vs wireless). Capabilities
// come from the feature table at init; a profile row above is optional enrichment.

/// The declared device type for a wireless `wpid`, or `Other` when unlisted.
const COMPOSITE_HIDPP_INTERFACE: i32 = 3;
const G502_HERO_HIDPP_INTERFACE: i32 = 1;
/// Delegates to `DEVICE_PROFILES` so adding a new wireless device only requires
/// editing the profile table.
pub(crate) fn wpid_device_type(wpid: u16) -> DeviceType {
    find_profile(wpid)
        .map(|p| p.device_type)
        .unwrap_or(DeviceType::Other)
}

inventory::submit! {
    DeviceDescriptor {
        matches: |h| matches!(h, DiscoveryHandle::Hid {
            vid: LOGITECH_VID,
            pid,
            interface_number: Some(iface),
            ..
        } if DEVICE_PROFILES.iter().any(|p| p.pid == *pid && p.interface == *iface)),
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, pid, idx, interface_number, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            let p = DEVICE_PROFILES.iter().find(|p| p.pid == pid && Some(p.interface) == interface_number)
                .ok_or_else(|| anyhow::anyhow!("no profile for pid {pid:#06x}"))?;
            Ok(Arc::new(LogitechDevice::new_direct(
                path, serial, pid, idx, p.interface, p.report_mode, p.name, p.device_type,
            )?))
        },
    }
}

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::LogitechSlot { wpid, .. }
        if find_profile(*wpid).is_some()),
    make: |h| {
        let DiscoveryHandle::LogitechSlot {
            devnum,
            wpid,
            serial,
            messenger,
        } = h
        else {
            anyhow::bail!("descriptor matched non-LogitechSlot handle");
        };
        Ok(Arc::new(LogitechDevice::new_without_coordinator(
            devnum,
            wpid,
            serial,
            wpid_device_type(wpid),
            messenger,
        )) as Arc<dyn crate::drivers::Device>)
    },
});

// ── G PRO X TKL keyboard layout ───────────────────────────────────────────────
//
// Built on the generic `StandardLayout::Tkl` base; the device-specific keys
// (media row, Fn, COPY, and the ISO `$` key) are applied as `KeyEdit::Add`s.
// `cid_map` translates each Logitech internal firmware zone ID (NOT a HID USB
// keycode) onto its resolved `KeyId`.

/// Synthetic `KeyId::Custom` numbers for keys not on a plain standard TKL.
/// The number equals the Logitech firmware zone ID for traceability.
const KEY_MEDIA_BRIGHTNESS: u16 = 150;
const KEY_MEDIA_BACK: u16 = 155;
const KEY_MEDIA_PLAY: u16 = 152;
const KEY_MEDIA_FWD: u16 = 154;
const KEY_MEDIA_MUTE: u16 = 153;
const KEY_FN: u16 = 111;
const KEY_COPY: u16 = 98;
const KEY_ISO_HASH: u16 = 47;

/// Device-specific keys common to both variants: the media row above the
/// function row (`row = -1.5`), plus `Fn` and `COPY` on the modifier row.
/// `col` is the key's left edge, matching `StandardLayout` grid units.
static GPROX_TKL_ANSI_EDITS: &[KeyEdit] = &[
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_BRIGHTNESS), 5.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_BACK), 11.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_PLAY), 12.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_FWD), 13.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_MUTE), 14.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_FN), 11.25, 5.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_COPY), 12.25, 5.5)),
    KeyEdit::Remove(KeyId::IsoExtra),
];

/// ISO variant: the common device keys, plus the Logitech `$`/`#` key
/// (firmware zone 47) replacing the standard home-row Backslash.
static GPROX_TKL_ISO_EDITS: &[KeyEdit] = &[
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_BRIGHTNESS), 5.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_BACK), 11.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_PLAY), 12.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_FWD), 13.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_MUTE), 14.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_FN), 11.25, 5.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_COPY), 12.25, 5.5)),
    // ISO `$` key sits where the standard ISO home-row Backslash is.
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_ISO_HASH), 12.75, 3.5)),
    KeyEdit::Remove(KeyId::Backslash),
];

static GPROX_TKL_LANGUAGES: &[KeyboardLayout] = &[
    KeyboardLayout::US,
    KeyboardLayout::CH,
    KeyboardLayout::IT,
    KeyboardLayout::DE,
    KeyboardLayout::FR,
    KeyboardLayout::UK,
];

/// Logitech firmware zone ID → device-neutral `KeyId`. Covers every key in the
/// resolved TKL layout (standard base keys + the `GPROX_TKL_EDITS` keys).
static GPROX_TKL_CID_MAP: &[(u32, KeyId)] = &[
    // Function row
    (38, KeyId::Escape),
    (55, KeyId::F1),
    (56, KeyId::F2),
    (57, KeyId::F3),
    (58, KeyId::F4),
    (59, KeyId::F5),
    (60, KeyId::F6),
    (61, KeyId::F7),
    (62, KeyId::F8),
    (63, KeyId::F9),
    (64, KeyId::F10),
    (65, KeyId::F11),
    (66, KeyId::F12),
    (67, KeyId::PrintScreen),
    (68, KeyId::ScrollLock),
    (69, KeyId::Pause),
    // Number row
    (50, KeyId::Backtick),
    (27, KeyId::Digit1),
    (28, KeyId::Digit2),
    (29, KeyId::Digit3),
    (30, KeyId::Digit4),
    (31, KeyId::Digit5),
    (32, KeyId::Digit6),
    (33, KeyId::Digit7),
    (34, KeyId::Digit8),
    (35, KeyId::Digit9),
    (36, KeyId::Digit0),
    (42, KeyId::Minus),
    (43, KeyId::Equals),
    (39, KeyId::Backspace),
    (70, KeyId::Insert),
    (71, KeyId::Home),
    (72, KeyId::PageUp),
    // QWERTY row
    (40, KeyId::Tab),
    (17, KeyId::Q),
    (23, KeyId::W),
    (5, KeyId::E),
    (18, KeyId::R),
    (20, KeyId::T),
    (25, KeyId::Y),
    (21, KeyId::U),
    (9, KeyId::I),
    (15, KeyId::O),
    (16, KeyId::P),
    (44, KeyId::LeftBracket),
    (45, KeyId::RightBracket),
    (46, KeyId::Backslash),
    (73, KeyId::Delete),
    (74, KeyId::End),
    (75, KeyId::PageDown),
    (37, KeyId::Enter),
    // Home row
    (54, KeyId::CapsLock),
    (1, KeyId::A),
    (19, KeyId::S),
    (4, KeyId::D),
    (6, KeyId::F),
    (7, KeyId::G),
    (8, KeyId::H),
    (10, KeyId::J),
    (11, KeyId::K),
    (12, KeyId::L),
    (48, KeyId::Semicolon),
    (49, KeyId::Quote),
    // ZXCV row
    (105, KeyId::LeftShift),
    (97, KeyId::IsoExtra),
    (26, KeyId::Z),
    (24, KeyId::X),
    (3, KeyId::C),
    (22, KeyId::V),
    (2, KeyId::B),
    (14, KeyId::N),
    (13, KeyId::M),
    (51, KeyId::Comma),
    (52, KeyId::Period),
    (53, KeyId::Slash),
    (109, KeyId::RightShift),
    (79, KeyId::Up),
    // Modifier row
    (104, KeyId::LeftCtrl),
    (107, KeyId::LeftSuper),
    (106, KeyId::LeftAlt),
    (41, KeyId::Space),
    (110, KeyId::RightAlt),
    (108, KeyId::RightCtrl),
    (77, KeyId::Left),
    (78, KeyId::Down),
    (76, KeyId::Right),
    // Device-specific keys (see GPROX_TKL_EDITS)
    (
        KEY_MEDIA_BRIGHTNESS as u32,
        KeyId::Custom(KEY_MEDIA_BRIGHTNESS),
    ),
    (KEY_MEDIA_BACK as u32, KeyId::Custom(KEY_MEDIA_BACK)),
    (KEY_MEDIA_PLAY as u32, KeyId::Custom(KEY_MEDIA_PLAY)),
    (KEY_MEDIA_FWD as u32, KeyId::Custom(KEY_MEDIA_FWD)),
    (KEY_MEDIA_MUTE as u32, KeyId::Custom(KEY_MEDIA_MUTE)),
    (KEY_FN as u32, KeyId::Custom(KEY_FN)),
    (KEY_COPY as u32, KeyId::Custom(KEY_COPY)),
    (KEY_ISO_HASH as u32, KeyId::Custom(KEY_ISO_HASH)),
];

static GPROX_TKL_KB_SPEC: LogitechKeyboardSpec = LogitechKeyboardSpec {
    ansi: KeyLayoutSpec {
        base: StandardLayout::Tkl,
        edits: GPROX_TKL_ANSI_EDITS,
        cid_map: GPROX_TKL_CID_MAP,
    },
    iso: Some(KeyLayoutSpec {
        base: StandardLayout::TklIso,
        edits: GPROX_TKL_ISO_EDITS,
        cid_map: GPROX_TKL_CID_MAP,
    }),
    languages: GPROX_TKL_LANGUAGES,
    remap_cids: &[],
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::generic::devices::common::tkl_key_positions;
    use crate::drivers::vendors::logitech::protocols::hidpp::v2::rgb::find_native_effect;

    // ── Discovery tables ─────────────────────────────────────────────────────

    #[test]
    fn profile_pids_interfaces_are_unique() {
        let mut seen: Vec<(u16, i32)> = DEVICE_PROFILES
            .iter()
            .map(|p| (p.pid, p.interface))
            .collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            n,
            "duplicate (pid, interface) in DEVICE_PROFILES"
        );
    }

    #[test]
    fn wpids_are_unique() {
        // All wireless devices in DEVICE_PROFILES must have unique wpids so
        // find_profile can distinguish them. Wired-only profiles (wpid: None)
        // are resolved through pid and don't participate.
        let wpids: Vec<u16> = DEVICE_PROFILES.iter().filter_map(|p| p.wpid).collect();
        let mut seen = wpids.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), wpids.len(), "duplicate wpid in DEVICE_PROFILES");
    }

    // ── TKL key layout ───────────────────────────────────────────────────────

    fn resolved_ids(spec: &KeyLayoutSpec) -> Vec<KeyId> {
        spec.resolve().iter().map(|c| c.id).collect()
    }

    #[test]
    fn ansi_variant_has_backslash_and_no_iso_keys() {
        let ids = resolved_ids(&GPROX_TKL_KB_SPEC.ansi);
        assert!(ids.contains(&KeyId::Backslash), "ANSI keeps Backslash");
        assert!(!ids.contains(&KeyId::IsoExtra), "ANSI drops <> key");
        assert!(
            !ids.contains(&KeyId::Custom(KEY_ISO_HASH)),
            "ANSI has no $ key"
        );
    }

    #[test]
    fn iso_variant_has_iso_keys_and_no_backslash() {
        let iso = GPROX_TKL_KB_SPEC.iso.as_ref().expect("declares ISO");
        let cells = iso.resolve();
        let ids: Vec<KeyId> = cells.iter().map(|c| c.id).collect();
        assert!(ids.contains(&KeyId::IsoExtra), "ISO has <> key");
        assert!(ids.contains(&KeyId::Custom(KEY_ISO_HASH)), "ISO has $ key");
        assert!(!ids.contains(&KeyId::Backslash), "ISO drops Backslash");
        let row = |id| cells.iter().find(|c| c.id == id).unwrap().row;
        assert_eq!(
            row(KeyId::Enter),
            row(KeyId::Q),
            "ISO Enter LED sits in the QWERTY row"
        );
    }

    #[test]
    fn every_resolved_key_of_both_variants_maps_through_cid_map() {
        let map = GPROX_TKL_CID_MAP;
        for variant in [
            &GPROX_TKL_KB_SPEC.ansi,
            GPROX_TKL_KB_SPEC.iso.as_ref().unwrap(),
        ] {
            for cell in variant.resolve() {
                assert!(
                    map.iter().any(|(_, kid)| *kid == cell.id),
                    "resolved key {:?} has no cid_map entry",
                    cell.id
                );
            }
        }
    }

    #[test]
    fn tkl_led_positions_include_device_specific_keys() {
        let ids: Vec<u32> = tkl_key_positions(&GPROX_TKL_KB_SPEC.ansi)
            .iter()
            .map(|p| p.id)
            .collect();
        // COPY (98) and the media row (150, 152-155) were recovered from HID++
        // 0x08/0x5E per-LED captures — see rev_eng notes. (97/<> is ANSI-absent.)
        for id in [98u32, 150, 152, 153, 154, 155] {
            assert!(ids.contains(&id), "missing zone id {id}");
        }
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "duplicate zone ID in TKL layout");
    }

    // ── Native RGB effects ───────────────────────────────────────────────────

    #[test]
    fn device_profile_native_effects_all_resolve() {
        for p in DEVICE_PROFILES {
            for id in p.native_effects {
                assert!(
                    find_native_effect(id).is_some(),
                    "{} lists unknown native effect id {id}",
                    p.name,
                );
            }
        }
    }

    #[test]
    fn g502_bitmap_button_cids_in_range() {
        // Synthetic CIDs are bitmap bit index + 1, and the gkeysEvent/spy bitmap
        // is 16-bit, so every entry must fall in 1..=16 to be addressable.
        for (cid, label) in G502X_PLUS_BITMAP_BUTTONS {
            assert!(
                (1..=16).contains(cid),
                "G502 bitmap button {label:?} has out-of-range cid {cid}",
            );
        }
    }

    // ── Default button mappings ──────────────────────────────────────────────

    fn buttons(cids: &[u16]) -> Vec<ButtonDescriptor> {
        cids.iter()
            .map(|&cid| ButtonDescriptor {
                cid,
                label: format!("b{cid}"),
                divertable: true,
                group: 0,
            })
            .collect()
    }

    #[test]
    fn defaults_empty_when_profile_has_none() {
        let tkl = find_profile(WPID_GPROX_TKL).unwrap();
        assert!(default_button_mappings(Some(tkl), &buttons(&[1, 2, 3])).is_empty());
        assert!(default_button_mappings(None, &buttons(&[1, 2, 3])).is_empty());
    }

    #[test]
    fn defaults_only_seed_present_cids() {
        let g502 = find_profile(WPID_G502X_PLUS).unwrap();
        // Only G7 (cid 3) is present; G8 (2) and the thumb trigger (13) are not.
        let got = default_button_mappings(Some(g502), &buttons(&[3, 9, 10]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].cid, 3);
        assert_eq!(
            got[0].base,
            ButtonAction::DpiCycle {
                direction: CycleDir::Down
            }
        );
        // Defaults never populate the shift layer.
        assert_eq!(got[0].shifted, ButtonAction::Native);
    }

    #[test]
    fn defaults_seed_all_present_buttons() {
        let g502 = find_profile(WPID_G502X_PLUS).unwrap();
        let got = default_button_mappings(Some(g502), &buttons(&[2, 3, 13]));
        let cids: Vec<u16> = got.iter().map(|m| m.cid).collect();
        assert_eq!(cids, vec![2, 3, 13]);
        assert!(got
            .iter()
            .any(|m| matches!(m.base, ButtonAction::MomentaryDpi { dpi: 400 })));
    }

    #[test]
    fn g502_default_cids_have_a_physical_button() {
        // Every default must map onto a real bitmap button, or it would silently
        // never be seeded (init filters to enumerated CIDs).
        for (cid, _) in G502X_PLUS_DEFAULT_BUTTONS {
            assert!(
                G502X_PLUS_BITMAP_BUTTONS.iter().any(|(b, _)| b == cid),
                "G502 default cid {cid} has no physical button in the bitmap table",
            );
        }
    }

    // ── build_device_id ───────────────────────────────────────────────────────

    #[test]
    fn build_device_id_valid_8char_hex_serial() {
        let id = build_device_id(Some("A1B2C3D4"), 0x4099, 0);
        assert_eq!(id, "logitech_A1B2C3D4");
    }

    #[test]
    fn build_device_id_mixed_case_serial_is_uppercased() {
        let id = build_device_id(Some("a1b2c3d4"), 0x4099, 0);
        assert_eq!(id, "logitech_A1B2C3D4");
    }

    #[test]
    fn build_device_id_short_serial_falls_back() {
        let id = build_device_id(Some("ABCDEF7"), 0x4099, 2);
        assert_eq!(id, "logitech_4099_2");
    }

    #[test]
    fn build_device_id_long_serial_falls_back() {
        let id = build_device_id(Some("ABCDEF789"), 0x4099, 2);
        assert_eq!(id, "logitech_4099_2");
    }

    #[test]
    fn build_device_id_serial_with_non_hex_char_falls_back() {
        // 'G' and 'Z' are not hex digits.
        let id = build_device_id(Some("ABGDEZ12"), 0x40B0, 1);
        assert_eq!(id, "logitech_40B0_1");
    }

    #[test]
    fn build_device_id_none_serial_falls_back() {
        let id = build_device_id(None, 0x4099, 3);
        assert_eq!(id, "logitech_4099_3");
    }

    #[test]
    fn wpid_device_type_known_returns_correct_type() {
        assert!(matches!(
            wpid_device_type(WPID_G502X_PLUS),
            DeviceType::Mouse
        ));
        assert!(matches!(
            wpid_device_type(WPID_GPROX_TKL),
            DeviceType::Keyboard
        ));
    }

    #[test]
    fn wpid_device_type_unknown_falls_back_to_other() {
        assert!(matches!(wpid_device_type(0xFFFF), DeviceType::Other));
    }
}
