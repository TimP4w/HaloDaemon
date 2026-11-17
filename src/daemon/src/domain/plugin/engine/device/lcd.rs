// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, OnceLock};

use halod_shared::types::{LcdDescriptor, ScreenRotation, ScreenShape};

use super::super::worker::InitLcd;
use crate::domain::device::LcdStateSlot;

/// The panel `initialize` reported and its live rotation/brightness/mode.
#[derive(Default)]
pub(super) struct LcdFacet {
    descriptor: OnceLock<LcdDescriptor>,
    slot: LcdStateSlot,
    needs_rgb_restore: AtomicBool,
}

static NO_PANEL: LazyLock<LcdStateSlot> = LazyLock::new(LcdStateSlot::default);

impl LcdFacet {
    pub(super) fn install(&self, lcd: &InitLcd) {
        self.slot.set_brightness(lcd.brightness);
        self.slot
            .set_rotation(ScreenRotation::from_degrees(lcd.rotation));
        self.slot.set_raw_streaming(lcd.raw_streaming);
        self.slot.set_latches_last_frame(lcd.latches);
        self.needs_rgb_restore
            .store(lcd.needs_rgb_restore, Ordering::Relaxed);
        let _ = self.descriptor.set(descriptor(lcd));
    }

    pub(super) fn descriptor(&self) -> LcdDescriptor {
        self.descriptor.get().cloned().unwrap_or(LcdDescriptor {
            shape: ScreenShape::Circle,
            width: 0,
            height: 0,
            supported_rotations: vec![ScreenRotation::R0],
            supported_image_types: Vec::new(),
            latches_last_frame: false,
        })
    }

    pub(super) fn slot(facet: Option<&Self>) -> &LcdStateSlot {
        facet.map_or(&NO_PANEL, |facet| &facet.slot)
    }

    pub(super) fn needs_rgb_restore(&self) -> bool {
        self.needs_rgb_restore.load(Ordering::Relaxed)
    }
}

fn descriptor(lcd: &InitLcd) -> LcdDescriptor {
    let shape = if lcd.shape.eq_ignore_ascii_case("square") {
        ScreenShape::Square
    } else {
        ScreenShape::Circle
    };
    let supported_rotations = if lcd.rotations.is_empty() {
        vec![ScreenRotation::R0]
    } else {
        lcd.rotations
            .iter()
            .map(|d| ScreenRotation::from_degrees(*d))
            .collect()
    };
    LcdDescriptor {
        shape,
        width: lcd.width,
        height: lcd.height,
        supported_rotations,
        supported_image_types: lcd.image_types.clone(),
        latches_last_frame: lcd.latches,
    }
}
