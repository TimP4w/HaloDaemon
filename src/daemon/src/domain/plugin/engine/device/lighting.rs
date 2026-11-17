// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::{LazyLock, OnceLock};

use halod_shared::keyboard::{KeyId, KeyVariant, KeyboardLayoutStatus, VisualKey};
use halod_shared::types::{
    KeyboardFormFactor, KeyboardLayout, LightingChannel, LightingDescriptor, NativeEffect,
    ZoneTopology,
};

use super::super::manifest::topology_from;
use super::super::worker::{InitKeyboard, InitKeyboardVariant, InitLightingChannel};
use crate::domain::device::lighting_segment::{linear_lighting_channel, ring_led_positions};
use crate::domain::device::KeyboardLayoutSlot;

/// Keyboard geometry and the runtime lighting descriptors `initialize`
/// reported: one for the ANSI layout and, when the keyboard has one, one for ISO.
#[derive(Default)]
pub(super) struct LightingFacet {
    keyboard: OnceLock<InitKeyboard>,
    descriptor: OnceLock<LightingDescriptor>,
    iso_descriptor: OnceLock<LightingDescriptor>,
}

static NO_CHANNELS: LazyLock<LightingDescriptor> = LazyLock::new(|| LightingDescriptor {
    channels: Vec::new(),
    native_effects: Vec::new(),
});

impl LightingFacet {
    pub(super) fn install_keyboard(&self, keyboard: InitKeyboard) {
        let _ = self.keyboard.set(keyboard);
    }

    pub(super) fn install_channels(
        &self,
        channels: Vec<InitLightingChannel>,
        effects: Vec<NativeEffect>,
    ) {
        let keyboard = self.keyboard.get();
        let ansi_keys = keyboard.map(|descriptor| visual_keys(&descriptor.ansi));
        if let Some(iso) = keyboard.and_then(|descriptor| descriptor.iso.as_ref()) {
            let iso_keys = visual_keys(iso);
            let _ = self.iso_descriptor.set(build_dynamic_descriptor(
                channels.clone(),
                effects.clone(),
                Some(&iso_keys),
            ));
        }
        let _ = self.descriptor.set(build_dynamic_descriptor(
            channels,
            effects,
            ansi_keys.as_deref(),
        ));
    }

    /// The descriptor for the layout the user selected.
    pub(super) fn descriptor(&self, layout: &KeyboardLayoutSlot) -> &LightingDescriptor {
        if let Some(keyboard) = self.keyboard.get() {
            let (variant, _) = effective_keyboard_layout(keyboard, layout);
            if variant == KeyVariant::Iso {
                if let Some(descriptor) = self.iso_descriptor.get() {
                    return descriptor;
                }
            }
        }
        self.descriptor.get().unwrap_or(&NO_CHANNELS)
    }

    pub(super) fn layout_status(&self, layout: &KeyboardLayoutSlot) -> KeyboardLayoutStatus {
        let selection = layout.selection();
        let Some(descriptor) = self.keyboard.get() else {
            return KeyboardLayoutStatus {
                keys: Vec::new(),
                variant: selection.variant.unwrap_or(KeyVariant::Ansi),
                language: selection.language.unwrap_or(KeyboardLayout::Unknown),
                detected_language: KeyboardLayout::Unknown,
                selection,
                iso_supported: false,
                languages: Vec::new(),
            };
        };
        let (variant, language) = effective_keyboard_layout(descriptor, layout);
        KeyboardLayoutStatus {
            keys: selected_visual_keys(descriptor, layout),
            variant,
            language,
            detected_language: descriptor.detected_language,
            selection,
            iso_supported: descriptor.iso.is_some(),
            languages: descriptor.languages.clone(),
        }
    }
}

fn visual_keys(variant: &InitKeyboardVariant) -> Vec<VisualKey> {
    let cells = variant.base.cells();
    let mut keys = Vec::new();
    // Preserve the standard layout's physical order, matching the native
    // KeyLayoutSpec resolver rather than the firmware/CID table order.
    for cell in cells {
        if let Some(mapping) = variant
            .keys
            .iter()
            .find(|mapping| mapping.key == Some(cell.id))
        {
            keys.push(VisualKey {
                led_id: mapping.led_id,
                remap_cid: mapping.remap_cid,
                cell: mapping
                    .cell
                    .as_ref()
                    .map(|geometry| halod_shared::keyboard::KeyCell {
                        id: cell.id,
                        col: geometry.col,
                        row: geometry.row,
                        w: geometry.w,
                        h: geometry.h,
                    })
                    .unwrap_or(cell),
            });
        }
    }
    // Model-specific media/Fn/etc. keys have explicit geometry and use their
    // firmware LED id as the stable custom-key identity.
    keys.extend(variant.keys.iter().filter_map(|mapping| {
        let geometry = mapping.cell.as_ref()?;
        Some(VisualKey {
            led_id: mapping.led_id,
            remap_cid: mapping.remap_cid,
            cell: halod_shared::keyboard::KeyCell {
                id: mapping.key.unwrap_or(KeyId::Custom(
                    u16::try_from(mapping.led_id).unwrap_or(u16::MAX),
                )),
                col: geometry.col,
                row: geometry.row,
                w: geometry.w,
                h: geometry.h,
            },
        })
    }));
    keys
}

fn keyboard_led_positions(
    keys: &[VisualKey],
    advertised: &[u32],
) -> Vec<halod_shared::types::LedPosition> {
    let position = |id: u32| {
        let (x, y) = keys
            .iter()
            .find(|key| key.led_id == id)
            .map_or((0.5, 0.5), |key| {
                // Exact projection used by the former native TKL descriptor.
                (
                    (key.cell.col + key.cell.w / 2.0) / 18.0,
                    (key.cell.row + 1.5) / 7.0,
                )
            });
        halod_shared::types::LedPosition { id, x, y }
    };
    // Frames are indexed by the plugin's declared `led_ids` order, so every
    // advertised LED keeps its slot even when the selected layout lacks its key.
    if advertised.is_empty() {
        keys.iter().map(|key| position(key.led_id)).collect()
    } else {
        advertised.iter().map(|&id| position(id)).collect()
    }
}

pub(super) fn effective_keyboard_layout(
    descriptor: &InitKeyboard,
    slot: &KeyboardLayoutSlot,
) -> (KeyVariant, KeyboardLayout) {
    crate::domain::device::effective_layout(
        slot.selection(),
        descriptor.detected_language,
        descriptor
            .languages
            .first()
            .copied()
            .unwrap_or(KeyboardLayout::US),
        descriptor.iso.is_some(),
    )
}

fn selected_visual_keys(descriptor: &InitKeyboard, slot: &KeyboardLayoutSlot) -> Vec<VisualKey> {
    let (variant, _) = effective_keyboard_layout(descriptor, slot);
    let layout = match variant {
        KeyVariant::Iso => descriptor.iso.as_ref().unwrap_or(&descriptor.ansi),
        KeyVariant::Ansi => &descriptor.ansi,
    };
    visual_keys(layout)
}

/// Build an `LightingDescriptor` from `initialize`-reported channels, computing LED
/// positions from the declared topology + count (as static accessory channels do).
pub(super) fn build_dynamic_descriptor(
    channels: Vec<InitLightingChannel>,
    native_effects: Vec<NativeEffect>,
    keyboard_keys: Option<&[VisualKey]>,
) -> LightingDescriptor {
    let channels = channels
        .into_iter()
        .map(|z| {
            let topology = if z.topology == "keyboard" {
                ZoneTopology::Keyboard {
                    form_factor: z.keyboard_form_factor.unwrap_or(KeyboardFormFactor::TKL),
                    layout: z.keyboard_layout.unwrap_or(KeyboardLayout::Unknown),
                }
            } else {
                topology_from(&z.topology, z.rings)
            };
            let led_count = if z.led_ids.is_empty() {
                z.led_count
            } else {
                z.led_ids.len() as u32
            };
            // `ring_led_positions` only lays out ring topologies; linear channels use
            // the evenly-spaced strip layout (as the former built-in drivers did).
            if !z.leds.is_empty() {
                LightingChannel {
                    leds: z.leds,
                    id: z.id,
                    name: z.name,
                    topology,
                    color_order: Default::default(),
                    division: Default::default(),
                    visibility: Default::default(),
                }
            } else if let (ZoneTopology::Keyboard { .. }, Some(keys)) =
                (&topology, keyboard_keys.filter(|keys| !keys.is_empty()))
            {
                LightingChannel {
                    leds: keyboard_led_positions(keys, &z.led_ids),
                    id: z.id,
                    name: z.name,
                    topology,
                    color_order: Default::default(),
                    division: Default::default(),
                    visibility: Default::default(),
                }
            } else if !z.led_ids.is_empty() {
                let columns = if matches!(topology, ZoneTopology::Keyboard { .. }) {
                    18
                } else {
                    z.led_ids.len().max(1)
                };
                let rows = z.led_ids.len().div_ceil(columns).max(1);
                LightingChannel {
                    leds: z
                        .led_ids
                        .iter()
                        .enumerate()
                        .map(|(i, id)| halod_shared::types::LedPosition {
                            id: *id,
                            x: if columns <= 1 {
                                0.0
                            } else {
                                (i % columns) as f32 / (columns - 1) as f32
                            },
                            y: if rows <= 1 {
                                0.5
                            } else {
                                (i / columns) as f32 / (rows - 1) as f32
                            },
                        })
                        .collect(),
                    id: z.id,
                    name: z.name,
                    topology,
                    color_order: Default::default(),
                    division: Default::default(),
                    visibility: Default::default(),
                }
            } else if matches!(topology, ZoneTopology::Linear) {
                linear_lighting_channel(&z.id, &z.name, led_count as usize)
            } else {
                LightingChannel {
                    leds: ring_led_positions(&topology, led_count),
                    id: z.id,
                    name: z.name,
                    topology,
                    color_order: Default::default(),
                    division: Default::default(),
                    visibility: Default::default(),
                }
            }
        })
        .collect();
    LightingDescriptor {
        channels,
        native_effects,
    }
}

#[cfg(test)]
mod dynamic_rgb_descriptor_tests {
    use super::*;
    use halod_shared::types::LedPosition;

    #[test]
    fn runtime_zone_preserves_explicit_led_geometry() {
        let leds = vec![
            LedPosition {
                id: 7,
                x: 0.98,
                y: 1.0,
            },
            LedPosition {
                id: 8,
                x: 0.5,
                y: 0.05,
            },
        ];
        let descriptor = build_dynamic_descriptor(
            vec![InitLightingChannel {
                id: "ambiglow".into(),
                name: "Ambiglow".into(),
                topology: "grid".into(),
                led_count: 2,
                led_ids: Vec::new(),
                leds: leds.clone(),
                rings: 0,
                keyboard_form_factor: None,
                keyboard_layout: None,
                chainable: false,
                max_leds: 0,
                color_order: Default::default(),
                cooling_channel: None,
            }],
            Vec::new(),
            None,
        );

        let zone = &descriptor.channels[0];
        assert!(matches!(zone.topology, ZoneTopology::Grid));
        assert_eq!(zone.leds.len(), 2);
        for (actual, expected) in zone.leds.iter().zip(leds) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.x, expected.x);
            assert_eq!(actual.y, expected.y);
        }
    }

    #[test]
    fn dynamic_rgb_descriptor_uses_reported_led_ids() {
        let descriptor = build_dynamic_descriptor(
            vec![InitLightingChannel {
                id: "ring".into(),
                name: "Ring".into(),
                topology: "linear".into(),
                led_count: 2,
                led_ids: vec![10, 20],
                leds: Vec::new(),
                rings: 0,
                keyboard_form_factor: None,
                keyboard_layout: None,
                chainable: false,
                max_leds: 0,
                color_order: Default::default(),
                cooling_channel: None,
            }],
            Vec::new(),
            None,
        );
        assert_eq!(descriptor.channels.len(), 1);
        assert_eq!(
            descriptor.channels[0]
                .leds
                .iter()
                .map(|led| led.id)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
    }

    #[test]
    fn keyboard_leds_follow_advertised_frame_order() {
        use halod_shared::keyboard::KeyCell;
        let key = |led_id, id, col| VisualKey {
            led_id,
            remap_cid: None,
            cell: KeyCell::new(id, col, 0.0),
        };
        let keys = [key(1, KeyId::A, 0.0), key(2, KeyId::B, 1.0)];
        let leds = keyboard_led_positions(&keys, &[2, 9, 1]);
        assert_eq!(leds.iter().map(|l| l.id).collect::<Vec<_>>(), [2, 9, 1]);
        assert_eq!(leds[0].x, 1.5 / 18.0);
        assert_eq!(leds[2].x, 0.5 / 18.0);
    }
}
