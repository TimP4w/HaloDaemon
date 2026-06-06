//! Logitech device profiles — the static per-model descriptor table, WPID/PID
//! constants, keyboard-layout statics, and device registration.

use std::sync::Arc;

use crate::discovery::{DeviceDescriptor, DiscoveryHandle};
use crate::drivers::vendors::logitech::devices::device::LogitechDevice;
use halod_protocol::keyboard::{KeyCell, KeyEdit, KeyId, KeyLayoutSpec, StandardLayout};
use halod_protocol::types::{DeviceType, KeyboardFormFactor, KeyboardLayout, ZoneTopology};

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
pub const WIRED_PID_GPROX_TKL: u16 = 0xC352;

// ── Device profiles ───────────────────────────────────────────────────────────

pub(super) struct LogitechZoneInfo {
    pub(super) name: &'static str,
    pub(super) topology: ZoneTopology,
    pub(super) led_count: u8,
}

pub(super) struct LogitechDeviceProfile {
    pub(super) wpid: u16,
    pub(super) pid: u16,
    pub(super) name: &'static str,
    pub(super) device_type: DeviceType,
    pub(super) zones: &'static [LogitechZoneInfo],
    /// Native RGB effect ids advertised for this device, in display order.
    /// Each id must resolve via `find_native_effect`.
    pub(super) native_effects: &'static [&'static str],
    /// Button labels for bitmap-event backends (GKEY / MOUSE_BUTTON_SPY), keyed by
    /// synthetic CID (bitmap bit index + 1). `None` → use `bitmap_button_prefix`.
    pub(super) bitmap_button_labels: Option<&'static [(u16, &'static str)]>,
    /// Fallback label prefix when no per-CID table applies (e.g. "Button", "G").
    pub(super) bitmap_button_prefix: &'static str,
    /// Keyboard layout for this model (`None` for mice). The driver resolves it
    /// and maps each key through its `cid_map` to build per-key LED positions.
    pub(super) key_layout: Option<&'static KeyLayoutSpec>,
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

pub(super) static DEVICE_PROFILES: &[LogitechDeviceProfile] = &[
    LogitechDeviceProfile {
        wpid: WPID_G502X_PLUS,
        pid: WIRED_PID_G502X_PLUS,
        name: "Logitech G502 X Plus",
        device_type: DeviceType::Mouse,
        zones: &[LogitechZoneInfo { name: "Lighting", topology: ZoneTopology::Linear, led_count: 8 }],
        native_effects: &["color_wave"],
        bitmap_button_labels: Some(G502X_PLUS_BITMAP_BUTTONS),
        bitmap_button_prefix: "Button",
        key_layout: None,
    },
    LogitechDeviceProfile {
        wpid: WPID_GPROX_TKL,
        pid: WIRED_PID_GPROX_TKL,
        name: "Logitech G PRO X TKL",
        device_type: DeviceType::Keyboard,
        zones: &[LogitechZoneInfo {
            name: "Keys",
            topology: ZoneTopology::Keyboard { form_factor: KeyboardFormFactor::TKL, layout: KeyboardLayout::US },
            led_count: 0,
        }],
        // color_wave is mouse-only until its keyboard effect-table slot is
        // resolved by scan — slot 0 (the mouse slot) is wrong for the keyboard.
        native_effects: &["ripple"],
        // GKEY backend: no per-CID table, G-keys labelled "G1".."G12".
        bitmap_button_labels: None,
        bitmap_button_prefix: "G",
        key_layout: Some(&GPROX_TKL_LAYOUT),
    },
];

pub(super) fn find_profile(id: u16) -> Option<&'static LogitechDeviceProfile> {
    DEVICE_PROFILES.iter().find(|p| p.wpid == id || p.pid == id)
}

/// Build a stable device ID that is the same for wired and wireless transport.
/// When an 8-char hex serial is available (the unit ID from HID or receiver pairing data),
/// uses `logitech_<SERIAL>` so the two transports appear as one device.
/// Falls back to `logitech_<PID/WPID>_<index>` when no serial is known.
pub(super) fn build_device_id(serial: Option<&str>, pid_or_wpid: u16, fallback_index: usize) -> String {
    match serial.filter(|s| s.len() == 8 && s.chars().all(|c| c.is_ascii_hexdigit())) {
        Some(s) => format!("logitech_{}", s.to_uppercase()),
        None => format!("logitech_{pid_or_wpid:04X}_{fallback_index}"),
    }
}

// ── Wired device registration ─────────────────────────────────────────────────

inventory::submit! {
    DeviceDescriptor {
        // No preferred collection: like the receiver, `new_direct` re-enumerates
        // both the short- and long-report collections itself via
        // `hidpp::collection::resolve_hidpp_paths`, so the path discovery picks
        // here is only a fallback and the collection choice is irrelevant.
        matches: |h| matches!(h, DiscoveryHandle::Hid {
            vid: LOGITECH_VID,
            pid: WIRED_PID_G502X_PLUS,
            interface_number: Some(WIRED_HIDPP_INTERFACE),
            ..
        }),
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, pid, idx, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            Ok(Arc::new(LogitechDevice::new_direct(path, serial, pid as u16, idx)?))
        },
    }
}

inventory::submit! {
    DeviceDescriptor {
        // See the G502 X Plus descriptor above.
        matches: |h| matches!(h, DiscoveryHandle::Hid {
            vid: LOGITECH_VID,
            pid: WIRED_PID_GPROX_TKL,
            interface_number: Some(WIRED_HIDPP_INTERFACE),
            ..
        }),
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, pid, idx, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            Ok(Arc::new(LogitechDevice::new_direct(path, serial, pid as u16, idx)?))
        },
    }
}

// ── Wireless device registration ──────────────────────────────────────────────

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::LogitechSlot { wpid: WPID_G502X_PLUS, .. }),
    make: |h| {
        let DiscoveryHandle::LogitechSlot { devnum, wpid, serial, messenger } = h else {
            anyhow::bail!("descriptor matched non-LogitechSlot handle");
        };
        Ok(Arc::new(LogitechDevice::new(devnum, wpid, serial, messenger)) as Arc<dyn crate::drivers::Device>)
    },
});

inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::LogitechSlot { wpid: WPID_GPROX_TKL, .. }),
    make: |h| {
        let DiscoveryHandle::LogitechSlot { devnum, wpid, serial, messenger } = h else {
            anyhow::bail!("descriptor matched non-LogitechSlot handle");
        };
        Ok(Arc::new(LogitechDevice::new(devnum, wpid, serial, messenger)) as Arc<dyn crate::drivers::Device>)
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

/// Device-specific keys layered over the standard TKL base. `col`/`row` use the
/// same grid units as `StandardLayout::Tkl` (the media row sits above the
/// function row at `row = -1.5`).
static GPROX_TKL_EDITS: &[KeyEdit] = &[
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_BRIGHTNESS), 5.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_BACK), 11.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_PLAY), 12.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_FWD), 13.0, -1.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_MEDIA_MUTE), 14.0, -1.5)),
    // ISO `$` key — right of `'`, inside the Enter notch.
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_ISO_HASH), 12.875, 4.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_FN), 11.25, 7.5)),
    KeyEdit::Add(KeyCell::new(KeyId::Custom(KEY_COPY), 12.25, 7.5)),
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
    (KEY_MEDIA_BRIGHTNESS as u32, KeyId::Custom(KEY_MEDIA_BRIGHTNESS)),
    (KEY_MEDIA_BACK as u32, KeyId::Custom(KEY_MEDIA_BACK)),
    (KEY_MEDIA_PLAY as u32, KeyId::Custom(KEY_MEDIA_PLAY)),
    (KEY_MEDIA_FWD as u32, KeyId::Custom(KEY_MEDIA_FWD)),
    (KEY_MEDIA_MUTE as u32, KeyId::Custom(KEY_MEDIA_MUTE)),
    (KEY_FN as u32, KeyId::Custom(KEY_FN)),
    (KEY_COPY as u32, KeyId::Custom(KEY_COPY)),
    (KEY_ISO_HASH as u32, KeyId::Custom(KEY_ISO_HASH)),
];

static GPROX_TKL_LAYOUT: KeyLayoutSpec = KeyLayoutSpec {
    base: StandardLayout::Tkl,
    edits: GPROX_TKL_EDITS,
    cid_map: GPROX_TKL_CID_MAP,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::generic::devices::common::tkl_key_positions;
    use crate::drivers::vendors::logitech::protocols::hidpp::rgb_effects::find_native_effect;

    // ── TKL key layout ───────────────────────────────────────────────────────

    #[test]
    fn tkl_layout_includes_iso_extra_keys() {
        let ids: Vec<u32> = tkl_key_positions(&GPROX_TKL_LAYOUT).iter().map(|p| p.id).collect();
        // <> (97), COPY (98) and the media row (150, 152-155) were recovered
        // from HID++ 0x08/0x5E per-LED captures — see rev_eng notes.
        for id in [97u32, 98, 150, 152, 153, 154, 155] {
            assert!(ids.contains(&id), "missing zone id {id}");
        }
        // A duplicate zone ID would shadow an LED in the row-major grid.
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
}
