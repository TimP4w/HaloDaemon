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
    /// Keyboard layout for this model (`None` for mice). The driver resolves it
    /// and maps each key through its `cid_map` to build per-key LED positions.
    pub(super) key_layout: Option<&'static KeyLayoutSpec<'static>>,
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

pub(super) static DEVICE_PROFILES: &[LogitechDeviceProfile] = &[
    LogitechDeviceProfile {
        wpid: Some(WPID_G502X_PLUS),
        pid: WIRED_PID_G502X_PLUS,
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
        key_layout: None,
    },
    LogitechDeviceProfile {
        wpid: None,
        pid: WIRED_PID_G502_HERO,
        name: "Logitech G502 Hero",
        device_type: DeviceType::Mouse,
        // Zones are discovered at runtime via COLOR_LED_EFFECTS (0x8070); a
        // single-LED fallback template is applied per-zone by the init path.
        zones: &[],
        // No native effects — 0x8070 doesn't use the 0x8071 NativeEffect system.
        native_effects: &[],
        bitmap_button_labels: Some(G502X_PLUS_BITMAP_BUTTONS),
        bitmap_button_prefix: "Button",
        default_buttons: Some(G502X_PLUS_DEFAULT_BUTTONS),
        key_layout: None,
    },
    LogitechDeviceProfile {
        wpid: Some(WPID_GPROX_TKL),
        pid: WIRED_PID_GPROX_TKL,
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
        // GKEY backend: no per-CID table, G-keys labelled "G1".."G12".
        bitmap_button_labels: None,
        bitmap_button_prefix: "G",
        default_buttons: None,
        key_layout: Some(&GPROX_TKL_LAYOUT),
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

/// HID++ vendor interface for composite (audio + HID) devices, e.g. headsets.
/// These expose only the long report, so they use [`DirectReport::LongOnly`].
const COMPOSITE_HIDPP_INTERFACE: i32 = 3;
const G502_HERO_HIDPP_INTERFACE: i32 = 1;

/// Directly-connected (USB) devices: `(pid, hid interface, report mode, name,
/// device type)`. `ShortLong` resolves the split short/long collections
/// (standard wired); `LongOnly` opens a single long-only handle (composite
/// headsets/dongles). `name` is a plain display label used when the device
/// doesn't advertise the DEVICE_NAME feature.
///
/// `device_type` is the fallback used only when no `DEVICE_PROFILES` row exists
/// for this pid; profiled devices (mice, keyboards) take their type from the
/// profile, which is the single authoritative source.
static DIRECT_PIDS: &[(u16, i32, DirectReport, &str, DeviceType)] = &[
    (
        WIRED_PID_G502X_PLUS,
        WIRED_HIDPP_INTERFACE,
        DirectReport::ShortLong,
        "Logitech G502 X Plus",
        DeviceType::Other, // authoritative type is in DEVICE_PROFILES
    ),
    (
        WIRED_PID_G502_HERO,
        G502_HERO_HIDPP_INTERFACE,
        DirectReport::ShortLong,
        "Logitech G502 Hero",
        DeviceType::Other, // authoritative type is in DEVICE_PROFILES
    ),
    (
        WIRED_PID_GPROX_TKL,
        WIRED_HIDPP_INTERFACE,
        DirectReport::ShortLong,
        "Logitech G PRO X TKL",
        DeviceType::Other, // authoritative type is in DEVICE_PROFILES
    ),
    // LIGHTSPEED headsets — direct USB, long-only, on the audio interface. These
    // don't advertise DEVICE_NAME, so the label below is the displayed name.
    // No profile row exists for these; the type here is the sole declaration.
    (
        0x0ABA,
        COMPOSITE_HIDPP_INTERFACE,
        DirectReport::LongOnly,
        "Logitech PRO X Wireless Gaming Headset",
        DeviceType::Headset,
    ),
    (
        0x0AF7,
        COMPOSITE_HIDPP_INTERFACE,
        DirectReport::LongOnly,
        "Logitech PRO X 2 LIGHTSPEED",
        DeviceType::Headset,
    ),
    (
        0x0AB5,
        COMPOSITE_HIDPP_INTERFACE,
        DirectReport::LongOnly,
        "Logitech G733 LIGHTSPEED",
        DeviceType::Headset,
    ),
    (
        0x0AFE,
        COMPOSITE_HIDPP_INTERFACE,
        DirectReport::LongOnly,
        "Logitech G733 LIGHTSPEED",
        DeviceType::Headset,
    ),
    (
        0x0AC4,
        COMPOSITE_HIDPP_INTERFACE,
        DirectReport::LongOnly,
        "Logitech G535 LIGHTSPEED",
        DeviceType::Headset,
    ),
    (
        0x0A87,
        COMPOSITE_HIDPP_INTERFACE,
        DirectReport::LongOnly,
        "Logitech G935",
        DeviceType::Headset,
    ),
    (
        0x0A66,
        COMPOSITE_HIDPP_INTERFACE,
        DirectReport::LongOnly,
        "Logitech G533",
        DeviceType::Headset,
    ),
];

/// The declared device type for a wireless `wpid`, or `Other` when unlisted.
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
        } if DIRECT_PIDS.iter().any(|(p, i, _, _, _)| p == pid && i == iface)),
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, pid, idx, interface_number, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            let entry = DIRECT_PIDS
                .iter()
                .find(|(p, i, _, _, _)| *p == pid && Some(*i) == interface_number)
                .ok_or_else(|| anyhow::anyhow!("no DIRECT_PIDS entry for pid {pid:#06x}"))?;
            let (_, iface, report, name, fallback_type) = entry;
            // Profile is authoritative for device type; fallback_type covers
            // devices (e.g. headsets) that have no DEVICE_PROFILES row.
            let device_type = find_profile(pid)
                .map(|p| p.device_type)
                .unwrap_or(*fallback_type);
            Ok(Arc::new(LogitechDevice::new_direct(
                path, serial, pid, idx, *iface, *report, name, device_type,
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

static GPROX_TKL_LAYOUT: KeyLayoutSpec<'static> = KeyLayoutSpec {
    base: StandardLayout::Tkl,
    edits: GPROX_TKL_EDITS,
    cid_map: GPROX_TKL_CID_MAP,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::generic::devices::common::tkl_key_positions;
    use crate::drivers::vendors::logitech::protocols::hidpp::v2::rgb::find_native_effect;

    // ── Discovery tables ─────────────────────────────────────────────────────

    #[test]
    fn direct_pids_have_no_duplicate_pid_interface() {
        let mut seen: Vec<(u16, i32)> =
            DIRECT_PIDS.iter().map(|(p, i, _, _, _)| (*p, *i)).collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "duplicate (pid, interface) in DIRECT_PIDS");
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

    #[test]
    fn tkl_layout_includes_iso_extra_keys() {
        let ids: Vec<u32> = tkl_key_positions(&GPROX_TKL_LAYOUT)
            .iter()
            .map(|p| p.id)
            .collect();
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
