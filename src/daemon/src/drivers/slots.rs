// SPDX-License-Identifier: GPL-3.0-or-later
//! `Slot`/`KvStateCache` state cells and the per-capability state-slot types.
//! Re-exported from `drivers/mod.rs` so call sites keep using `crate::drivers::*`.

use halod_shared::types::{RgbState, VisibilityState};
use halod_shared::zone_transform::ZoneContentTransform;
use serde::{de::DeserializeOwned, Serialize};
use std::collections::HashMap;

/// A `Mutex`-backed state cell shared by the device state slots below, so each
/// slot type avoids hand-rolling the same lock dance.
#[derive(Default)]
pub struct Slot<T>(std::sync::Mutex<T>);

impl<T> Slot<T> {
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0.lock().unwrap())
    }
    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut self.0.lock().unwrap())
    }
}

impl<T: Clone> Slot<T> {
    pub fn get(&self) -> T {
        self.0.lock().unwrap().clone()
    }
    pub fn set(&self, value: T) {
        *self.0.lock().unwrap() = value;
    }
}

/// Generates clone-on-read / overwrite-on-write accessor pairs for a `Slot`-backed struct's fields.
macro_rules! slot_accessors {
    ($slot:ty { $( $get:ident / $set:ident : $field:ident : $ty:ty ),* $(,)? }) => {
        impl $slot {
            $(
                pub fn $get(&self) -> $ty {
                    self.0.with(|s| s.$field.clone())
                }
                pub fn $set(&self, value: $ty) {
                    self.0.update(|s| s.$field = value);
                }
            )*
        }
    };
}

/// Shared slot for user-controlled device visibility. Embed in device structs and return
/// a reference from `visibility_slot()` to opt in to the enable/disable feature.
#[derive(Default)]
pub struct VisibilitySlot(Slot<VisibilityState>);

impl VisibilitySlot {
    pub fn get(&self) -> VisibilityState {
        self.0.get()
    }
    pub fn set(&self, state: VisibilityState) {
        self.0.set(state);
    }
}

pub struct KvStateCache<V>(std::sync::Mutex<std::collections::HashMap<String, V>>);

impl<V> Default for KvStateCache<V> {
    fn default() -> Self {
        KvStateCache(std::sync::Mutex::new(std::collections::HashMap::new()))
    }
}

impl<V: Clone + Serialize + DeserializeOwned> KvStateCache<V> {
    pub fn record(&self, key: &str, value: V) {
        self.0.lock().unwrap().insert(key.to_string(), value);
    }

    pub fn get(&self, key: &str) -> Option<V>
    where
        V: Copy,
    {
        self.0.lock().unwrap().get(key).copied()
    }

    pub fn save(&self) -> serde_json::Value {
        let map = self.0.lock().unwrap();
        if map.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::to_value(&*map).unwrap_or(serde_json::Value::Null)
    }

    pub fn load_pairs(&self, v: &serde_json::Value) -> Vec<(String, V)> {
        let map: std::collections::HashMap<String, V> =
            serde_json::from_value(v.clone()).unwrap_or_default();
        map.into_iter().collect()
    }
}

pub type RangeStateCache = KvStateCache<i32>;
pub type ChoiceStateCache = KvStateCache<usize>;
pub type BoolStateCache = KvStateCache<bool>;

#[derive(Default)]
pub struct RgbStateSlot(Slot<RgbStateInner>);

#[derive(Default)]
struct RgbStateInner {
    current_state: Option<RgbState>,
    canvas_zones: Vec<crate::config::PlacedZone>,
    zone_transforms: HashMap<String, ZoneContentTransform>,
}

slot_accessors!(RgbStateSlot {
    current_state / set_state : current_state : Option<RgbState>,
    canvas_zones / set_canvas_zones : canvas_zones : Vec<crate::config::PlacedZone>,
    zone_transforms / set_zone_transforms : zone_transforms : HashMap<String, ZoneContentTransform>,
});

impl RgbStateSlot {
    pub fn transform_for(&self, id: &str) -> ZoneContentTransform {
        self.0
            .with(|s| s.zone_transforms.get(id).copied().unwrap_or_default())
    }
    pub fn set_zone_transform(&self, id: String, t: ZoneContentTransform) {
        self.0.update(|s| {
            s.zone_transforms.insert(id, t);
        });
    }
}

#[derive(Default)]
pub struct FanStateSlot(Slot<Option<crate::cooling::config::FanCurveRecord>>);

impl FanStateSlot {
    pub fn fan_curve(&self) -> Option<crate::cooling::config::FanCurveRecord> {
        self.0.get()
    }
    pub fn set_fan_curve(&self, mut c: crate::cooling::config::FanCurveRecord) {
        c.sanitize();
        self.0.set(Some(c));
    }
    pub fn clear_fan_curve(&self) {
        self.0.set(None);
    }
}

#[derive(Default)]
pub struct LcdStateSlot(Slot<LcdStateInner>);

#[derive(Default)]
struct LcdStateInner {
    template_id: Option<String>,
    params: HashMap<String, halod_shared::types::EffectParamValue>,
    brightness: u8,
    rotation: halod_shared::types::ScreenRotation,
    mode: halod_shared::types::LcdMode,
    active_image: Option<String>,
    video_path: Option<String>,
    raw_streaming: bool,
    latches_last_frame: bool,
}

slot_accessors!(LcdStateSlot {
    lcd_template_id / set_lcd_template_id : template_id : Option<String>,
    lcd_template_params / set_lcd_template_params : params : HashMap<String, halod_shared::types::EffectParamValue>,
    brightness / set_brightness : brightness : u8,
    rotation / set_rotation : rotation : halod_shared::types::ScreenRotation,
    mode / set_mode : mode : halod_shared::types::LcdMode,
    active_image / set_active_image : active_image : Option<String>,
    raw_streaming / set_raw_streaming : raw_streaming : bool,
    video_path / set_video_path : video_path : Option<String>,
});

impl LcdStateSlot {
    pub fn set_latches_last_frame(&self, value: bool) {
        self.0.update(|state| state.latches_last_frame = value);
    }

    #[cfg(test)]
    pub fn latches_last_frame(&self) -> bool {
        self.0.with(|state| state.latches_last_frame)
    }
}
