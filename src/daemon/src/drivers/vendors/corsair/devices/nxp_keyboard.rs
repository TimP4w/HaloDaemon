// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project

//! Generic Corsair NXP keyboard driver — one `Device` impl over [`corsair_nxp`],
//! parameterised by a per-model [`ModelSpec`]. Adding another NXP keyboard is a
//! new table row, not a new file.
//!
//! Per-key RGB: each LED maps to a device key-id via the model `keys` table;
//! colors are scattered into a wire-slot buffer streamed by the protocol.
//! The physical layout (ANSI vs ISO) is user-selectable.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::{
    drivers::{
        transports::Transport,
        vendors::corsair::protocols::corsair_nxp::{
            CorsairNxp, CLASS_KEYBOARD, SKIP_ANSI, SKIP_ISO_K70_MK2,
        },
        vendors::generic::devices::common::{build_device_id, stable_serial},
        CapabilityRef, ChoiceCapability, ChoiceStateCache, Device, RgbCapability, RgbStateSlot,
        VisibilitySlot,
    },
    registry::discovery::{DeviceDescriptor, DiscoveryHandle},
};
use halod_shared::types::{
    Choice, ChoiceDisplay, ChoiceOption, DeviceCapability, DeviceType, KeyboardFormFactor,
    KeyboardLayout, LedPosition, RgbColor, RgbDescriptor, RgbState, RgbZone, ZoneTopology,
};

const CORSAIR_VID: u16 = 0x1B1C;
const NA: i16 = -1;

/// K70 MK.2 LED order → device key-id. 116 entries.
#[rustfmt::skip]
const K70_MK2_KEYS: &[u8] = &[
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0C, 0x0D, 0x0E, 0x0F, 0x11, 0x12,
    0x14, 0x15, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20, 0x21, 0x24, 0x25, 0x26,
    0x27, 0x28, 0x2A, 0x2B, 0x2C, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
    0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x42, 0x43, 0x44, 0x45, 0x48, 73, 74, 75, 76, 78,
    79, 80, 81, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 96, 97,
    98, 99, 100, 101, 102, 103, 104, 105, 108, 109, 110, 111, 112, 113, 115,
    116, 117, 120, 121, 122, 123, 124, 126, 127, 128, 129, 132, 133, 134, 135,
    136, 137, 139, 140, 141, 16, 114, 47, 59, 125,
];

/// Physical key grid (7×23). Cells hold LED indices into `K70_MK2_KEYS`; `NA` is empty.
#[rustfmt::skip]
const K70_MK2_MATRIX: &[[i16; 23]] = &[
    [ NA, NA, NA, 115, 107,  8, NA, NA, NA, NA, NA, 113, 114, NA, NA, NA, NA, NA, NA,  16, NA, NA,  NA],
    [  0, NA, 10,  18,  28, 36, NA, 46, 55, 64, 74,  NA,  84, 93, 102,  6, 15, 24, 33,  26, 35, 44,  53],
    [  1, 11, 19,  29,  37, 47, 56, 65, 75, 85, 94,  NA, 103,  7,  25, NA, 42, 51, 60,  62, 72, 82,  91],
    [  2, NA, 12,  20,  30, 38, NA, 48, 57, 66, 76,  86,  95, 104, 70, 80, 34, 43, 52,   9, 17, 27, 100],
    [  3, NA, 13,  21,  31, 39, NA, 49, 58, 67, 77,  87,  96, 105, 98, 112, NA, NA, NA, 45, 54, 63,  NA],
    [  4, 111, 22, 32,  40, 50, NA, 59, NA, 68, 78,  88,  97, 106, 61, NA, NA, 81, NA,  73, 83, 92, 109],
    [  5, 14, 23,  NA,  NA, NA, NA, 41, NA, NA, NA,  NA,  69, 79,  89, 71, 90, 99, 108, 101, NA, 110, NA],
];

/// Selectable physical layout.
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    SwissIso,
    ItalianIso,
    UsAnsi,
}

const LAYOUTS: &[(Layout, &str, &str)] = &[
    (Layout::SwissIso, "ch", "Swiss (ISO)"),
    (Layout::ItalianIso, "it", "Italian (ISO)"),
    (Layout::UsAnsi, "us", "US (ANSI)"),
];

impl Layout {
    fn from_index(i: usize) -> Layout {
        LAYOUTS
            .get(i)
            .map(|(l, _, _)| *l)
            .unwrap_or(Layout::SwissIso)
    }

    /// Physical-layout skip list for the layout-setup burst.
    fn skip_list(self) -> &'static [u8] {
        match self {
            Layout::UsAnsi => SKIP_ANSI,
            Layout::SwissIso | Layout::ItalianIso => SKIP_ISO_K70_MK2,
        }
    }

    /// Legend hint for the GUI keyboard renderer.
    fn keyboard_layout(self) -> KeyboardLayout {
        match self {
            Layout::SwissIso => KeyboardLayout::CH,
            Layout::ItalianIso => KeyboardLayout::IT,
            Layout::UsAnsi => KeyboardLayout::US,
        }
    }
}

struct ModelSpec {
    pid: u16,
    name: &'static str,
    model: &'static str,
    class: u8,
    form_factor: KeyboardFormFactor,
    keys: &'static [u8],
    matrix: &'static [[i16; 23]],
    /// Bytes streamed per color plane.
    wire_len: usize,
}

const MODELS: &[ModelSpec] = &[ModelSpec {
    pid: 0x1B55,
    name: "K70 RGB MK.2 Low Profile",
    model: "K70 RGB MK.2 Low Profile",
    class: CLASS_KEYBOARD,
    form_factor: KeyboardFormFactor::FullSize,
    keys: K70_MK2_KEYS,
    matrix: K70_MK2_MATRIX,
    wire_len: 144,
}];

fn spec_for(pid: u16) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|m| m.pid == pid)
}

inventory::submit! {
    DeviceDescriptor {
        matches: |h| match h {
            DiscoveryHandle::Hid { vid: CORSAIR_VID, pid, interface_number: Some(1), usage_page, .. } => {
                spec_for(*pid).is_some() && (*usage_page == 0xFFC2 || *usage_page == 0)
            }
            _ => false,
        },
        make: |h| {
            let DiscoveryHandle::Hid { path, serial, idx, pid, .. } = h else {
                anyhow::bail!("descriptor matched non-HID handle");
            };
            let spec = spec_for(pid)
                .ok_or_else(|| anyhow::anyhow!("no Corsair NXP spec for pid {pid:#06x}"))?;
            Ok(Arc::new(CorsairNxpKeyboard::new(path, serial, idx, spec)?))
        },
    }
}

fn led_positions(spec: &ModelSpec) -> Vec<LedPosition> {
    let rows = spec.matrix.len();
    let cols = spec.matrix.first().map(|r| r.len()).unwrap_or(0);
    let mut positions = vec![
        LedPosition {
            id: 0,
            x: 0.5,
            y: 0.5
        };
        spec.keys.len()
    ];
    for (id, p) in positions.iter_mut().enumerate() {
        p.id = id as u32;
    }
    for (r, row) in spec.matrix.iter().enumerate() {
        for (col, &cell) in row.iter().enumerate() {
            if cell < 0 {
                continue;
            }
            if let Some(p) = positions.get_mut(cell as usize) {
                p.x = if cols > 1 {
                    col as f32 / (cols - 1) as f32
                } else {
                    0.5
                };
                p.y = if rows > 1 {
                    r as f32 / (rows - 1) as f32
                } else {
                    0.5
                };
            }
        }
    }
    positions
}

fn build_descriptor(spec: &ModelSpec, layout: Layout) -> RgbDescriptor {
    RgbDescriptor {
        zones: vec![RgbZone {
            id: "keyboard".to_string(),
            name: "Keyboard".to_string(),
            topology: ZoneTopology::Keyboard {
                form_factor: spec.form_factor.clone(),
                layout: layout.keyboard_layout(),
            },
            leds: led_positions(spec),
        }],
        native_effects: vec![],
    }
}

pub struct CorsairNxpKeyboard {
    id: String,
    serial_number: Option<String>,
    proto: CorsairNxp<crate::drivers::transports::hid::HidTransport>,
    spec: &'static ModelSpec,
    descriptor: RgbDescriptor,
    rgb: RgbStateSlot,
    visibility: VisibilitySlot,
    layout: AtomicU8,
    choice_cache: ChoiceStateCache,
}

impl CorsairNxpKeyboard {
    fn new(path: &str, serial: Option<&str>, idx: usize, spec: &'static ModelSpec) -> Result<Self> {
        let default_layout = Layout::from_index(0);
        Ok(Self {
            id: build_device_id("corsair_nxp", serial, idx),
            serial_number: stable_serial(serial),
            proto: CorsairNxp::open(path)?,
            spec,
            descriptor: build_descriptor(spec, default_layout),
            rgb: RgbStateSlot::default(),
            visibility: VisibilitySlot::default(),
            layout: AtomicU8::new(0),
            choice_cache: ChoiceStateCache::default(),
        })
    }

    fn current_layout(&self) -> Layout {
        Layout::from_index(self.layout.load(Ordering::Relaxed) as usize)
    }

    fn wire_buffer(&self, led_colors: &[RgbColor]) -> Vec<RgbColor> {
        let mut buf = vec![RgbColor { r: 0, g: 0, b: 0 }; self.spec.wire_len];
        for (i, &key) in self.spec.keys.iter().enumerate() {
            if let (Some(&color), Some(slot)) = (led_colors.get(i), buf.get_mut(key as usize)) {
                *slot = color;
            }
        }
        buf
    }

    async fn write_leds(&self, led_colors: &[RgbColor]) -> Result<()> {
        self.proto.write_colors(&self.wire_buffer(led_colors)).await
    }

    async fn apply_state(&self, state: &RgbState) -> Result<()> {
        let led_count = self.spec.keys.len();
        match state {
            RgbState::Static { color } => self.write_leds(&vec![*color; led_count]).await?,
            RgbState::PerLed { zones } => {
                if let Some(map) = zones.get("keyboard") {
                    let black = RgbColor { r: 0, g: 0, b: 0 };
                    let colors: Vec<RgbColor> = (0..led_count)
                        .map(|i| map.get(&i.to_string()).copied().unwrap_or(black))
                        .collect();
                    self.write_leds(&colors).await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn send_layout_setup(&self) -> Result<()> {
        self.proto
            .send_layout_setup(self.current_layout().skip_list())
            .await
    }
}

#[async_trait]
impl Device for CorsairNxpKeyboard {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        self.spec.name
    }
    fn vendor(&self) -> &str {
        "Corsair"
    }
    fn model(&self) -> &str {
        self.spec.model
    }

    async fn initialize(&self) -> Result<bool> {
        self.proto.enter_software_mode(self.spec.class).await?;
        self.send_layout_setup().await?;
        log::info!("[CorsairNxpKeyboard] Initialized (id={})", self.id);
        Ok(true)
    }

    async fn close(&self) {
        if let Err(e) = self.proto.leave_software_mode().await {
            log::debug!("[CorsairNxpKeyboard] leave_software_mode: {e:#}");
        }
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Keyboard
    }

    fn wire_serial_number(&self) -> Option<String> {
        self.serial_number.clone()
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Rgb(self), CapabilityRef::Choice(self)]
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        Some(self.proto.transport.rate_status())
    }
}

#[async_trait]
impl RgbCapability for CorsairNxpKeyboard {
    fn descriptor(&self) -> &RgbDescriptor {
        &self.descriptor
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb
    }

    async fn apply(&self, state: RgbState) -> Result<()> {
        self.apply_state(&state).await?;
        self.rgb.set_state(Some(state));
        Ok(())
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        if zone_id != "keyboard" {
            anyhow::bail!("unknown zone: {zone_id}");
        }
        self.write_leds(colors).await
    }
}

#[async_trait]
impl ChoiceCapability for CorsairNxpKeyboard {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.choice_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Choice(vec![Choice {
            key: "layout".into(),
            label: "Physical Layout".into(),
            options: LAYOUTS
                .iter()
                .map(|(_, id, label)| ChoiceOption {
                    id: id.to_string(),
                    label: label.to_string(),
                })
                .collect(),
            selected: self.layout.load(Ordering::Relaxed) as usize,
            category: "Keyboard".into(),
            display: ChoiceDisplay::List,
            visible_when: None,
        }]))
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        if key != "layout" {
            anyhow::bail!("unknown choice key: {key}");
        }
        if selected >= LAYOUTS.len() {
            anyhow::bail!("layout index {selected} out of range");
        }
        self.choice_cache.record(key, selected);
        self.layout.store(selected as u8, Ordering::Relaxed);
        self.send_layout_setup().await?;
        if let Some(state) = self.rgb.current_state() {
            self.apply_state(&state).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k70() -> &'static ModelSpec {
        spec_for(0x1B55).expect("K70 MK.2 LP registered")
    }

    #[test]
    fn k70_low_profile_pid_resolves() {
        let spec = k70();
        assert_eq!(spec.name, "K70 RGB MK.2 Low Profile");
        assert_eq!(spec.class, CLASS_KEYBOARD);
        assert!(spec_for(0x0000).is_none());
    }

    #[test]
    fn key_map_fits_wire_buffer_and_is_unique() {
        let spec = k70();
        assert_eq!(spec.keys.len(), 116);
        let max = *spec.keys.iter().max().unwrap() as usize;
        assert!(max < spec.wire_len, "key-id {max} exceeds wire_len");
        let mut seen = spec.keys.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), spec.keys.len(), "duplicate key-ids in map");
    }

    #[test]
    fn every_led_has_a_matrix_position() {
        let spec = k70();
        let mut placed = vec![false; spec.keys.len()];
        for row in spec.matrix {
            for &cell in row {
                if cell >= 0 {
                    placed[cell as usize] = true;
                }
            }
        }
        assert!(placed.iter().all(|&p| p), "some LED missing from matrix");
    }

    #[test]
    fn positions_stay_in_unit_square() {
        let leds = led_positions(k70());
        assert_eq!(leds.len(), 116);
        assert!(leds.iter().all(|l| (0.0..=1.0).contains(&l.x)));
        assert!(leds.iter().all(|l| (0.0..=1.0).contains(&l.y)));
        assert!(leds.iter().enumerate().all(|(i, l)| l.id == i as u32));
    }

    #[test]
    fn wire_buffer_scatters_leds_to_key_slots() {
        let dev_spec = k70();
        let led_count = dev_spec.keys.len();
        let colors: Vec<RgbColor> = (0..led_count)
            .map(|i| RgbColor {
                r: i as u8,
                g: 0,
                b: 0,
            })
            .collect();
        let mut buf = vec![RgbColor { r: 0, g: 0, b: 0 }; dev_spec.wire_len];
        for (i, &key) in dev_spec.keys.iter().enumerate() {
            buf[key as usize] = colors[i];
        }
        // LED 0 → key-id 0x00, LED 56 → key-id 73.
        assert_eq!(buf[0x00].r, 0);
        assert_eq!(buf[73].r, 56);
    }

    #[test]
    fn layouts_map_to_expected_skip_lists() {
        assert_eq!(Layout::from_index(0).skip_list(), SKIP_ISO_K70_MK2);
        assert_eq!(Layout::from_index(2).skip_list(), SKIP_ANSI);
        assert!(matches!(Layout::from_index(99), Layout::SwissIso));
    }
}
