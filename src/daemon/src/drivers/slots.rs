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

/// The single authoritative content an LCD panel shows; `mode` derives from this so it can't disagree with the active path/id.
#[derive(Debug, Clone, Default, PartialEq)]
enum LcdActiveContent {
    #[default]
    Default,
    StaticImage {
        filename: String,
    },
    TemplateEngine {
        template_id: String,
        params: HashMap<String, halod_shared::types::EffectParamValue>,
    },
    Video {
        path: String,
    },
}

fn is_gif_filename(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

#[derive(Default)]
pub struct LcdStateSlot(Slot<LcdStateInner>);

#[derive(Default)]
struct LcdStateInner {
    content: LcdActiveContent,
    brightness: u8,
    rotation: halod_shared::types::ScreenRotation,
    raw_streaming: bool,
    latches_last_frame: bool,
}

slot_accessors!(LcdStateSlot {
    brightness / set_brightness : brightness : u8,
    rotation / set_rotation : rotation : halod_shared::types::ScreenRotation,
    raw_streaming / set_raw_streaming : raw_streaming : bool,
});

impl LcdStateSlot {
    pub fn mode(&self) -> halod_shared::types::LcdMode {
        use halod_shared::types::LcdMode;
        match self.0.with(|s| s.content.clone()) {
            LcdActiveContent::Default => LcdMode::Default,
            LcdActiveContent::StaticImage { filename } if is_gif_filename(&filename) => {
                LcdMode::Gif
            }
            LcdActiveContent::StaticImage { .. } => LcdMode::Image,
            LcdActiveContent::TemplateEngine { .. } => LcdMode::Engine,
            LcdActiveContent::Video { .. } => LcdMode::Video,
        }
    }

    /// Reset to `Default`, clearing whichever content was active.
    pub fn set_mode(&self, mode: halod_shared::types::LcdMode) {
        if matches!(mode, halod_shared::types::LcdMode::Default) {
            self.0.update(|s| s.content = LcdActiveContent::Default);
        }
    }

    pub fn active_image(&self) -> Option<String> {
        match self.0.with(|s| s.content.clone()) {
            LcdActiveContent::StaticImage { filename } => Some(filename),
            _ => None,
        }
    }
    /// Setting `Some` makes this the active content, clearing any template or video.
    pub fn set_active_image(&self, filename: Option<String>) {
        self.0.update(|s| {
            s.content = match filename {
                Some(filename) => LcdActiveContent::StaticImage { filename },
                None => LcdActiveContent::Default,
            };
        });
    }

    pub fn lcd_template_id(&self) -> Option<String> {
        match self.0.with(|s| s.content.clone()) {
            LcdActiveContent::TemplateEngine { template_id, .. } => Some(template_id),
            _ => None,
        }
    }
    /// Setting `Some` makes this the active content (preserving existing params), clearing any image or video.
    pub fn set_lcd_template_id(&self, id: Option<String>) {
        self.0.update(|s| {
            s.content = match id {
                Some(template_id) => {
                    let params = match &s.content {
                        LcdActiveContent::TemplateEngine { params, .. } => params.clone(),
                        _ => HashMap::new(),
                    };
                    LcdActiveContent::TemplateEngine {
                        template_id,
                        params,
                    }
                }
                None => LcdActiveContent::Default,
            };
        });
    }
    pub fn lcd_template_params(&self) -> HashMap<String, halod_shared::types::EffectParamValue> {
        match self.0.with(|s| s.content.clone()) {
            LcdActiveContent::TemplateEngine { params, .. } => params,
            _ => HashMap::new(),
        }
    }
    /// A no-op unless a template is already active — params without one make no sense.
    pub fn set_lcd_template_params(
        &self,
        params: HashMap<String, halod_shared::types::EffectParamValue>,
    ) {
        self.0.update(|s| {
            if let LcdActiveContent::TemplateEngine { template_id, .. } = &s.content {
                let template_id = template_id.clone();
                s.content = LcdActiveContent::TemplateEngine {
                    template_id,
                    params,
                };
            }
        });
    }

    pub fn video_path(&self) -> Option<String> {
        match self.0.with(|s| s.content.clone()) {
            LcdActiveContent::Video { path } => Some(path),
            _ => None,
        }
    }
    /// Setting `Some` makes this the active content, clearing any image or template.
    pub fn set_video_path(&self, path: Option<String>) {
        self.0.update(|s| {
            s.content = match path {
                Some(path) => LcdActiveContent::Video { path },
                None => LcdActiveContent::Default,
            };
        });
    }

    pub fn set_latches_last_frame(&self, value: bool) {
        self.0.update(|state| state.latches_last_frame = value);
    }

    #[cfg(test)]
    pub fn latches_last_frame(&self) -> bool {
        self.0.with(|state| state.latches_last_frame)
    }
}

#[cfg(test)]
mod lcd_state_tests {
    use super::*;
    use halod_shared::types::LcdMode;

    #[test]
    fn setting_any_mode_clears_the_others() {
        let slot = LcdStateSlot::default();
        slot.set_lcd_template_id(Some("t1".into()));
        assert_eq!(slot.mode(), LcdMode::Engine);

        slot.set_active_image(Some("pic.png".into()));
        assert_eq!(slot.mode(), LcdMode::Image);
        assert!(
            slot.lcd_template_id().is_none(),
            "image must clear the template"
        );

        slot.set_video_path(Some("clip.mp4".into()));
        assert_eq!(slot.mode(), LcdMode::Video);
        assert!(slot.active_image().is_none(), "video must clear the image");

        slot.set_mode(LcdMode::Default);
        assert_eq!(slot.mode(), LcdMode::Default);
        assert!(slot.video_path().is_none());
    }

    #[test]
    fn gif_extension_reports_gif_mode() {
        let slot = LcdStateSlot::default();
        slot.set_active_image(Some("anim.GIF".into()));
        assert_eq!(slot.mode(), LcdMode::Gif);
    }

    #[test]
    fn template_params_are_a_noop_without_an_active_template() {
        let slot = LcdStateSlot::default();
        slot.set_lcd_template_params(HashMap::from([(
            "x".to_string(),
            halod_shared::types::EffectParamValue::Float(1.0),
        )]));
        assert!(slot.lcd_template_params().is_empty());
    }

    #[test]
    fn template_params_persist_across_reasserting_the_same_template_id() {
        let slot = LcdStateSlot::default();
        slot.set_lcd_template_id(Some("t1".into()));
        slot.set_lcd_template_params(HashMap::from([(
            "x".to_string(),
            halod_shared::types::EffectParamValue::Float(1.0),
        )]));
        slot.set_lcd_template_id(Some("t1".into()));
        assert!(!slot.lcd_template_params().is_empty());
    }
}
