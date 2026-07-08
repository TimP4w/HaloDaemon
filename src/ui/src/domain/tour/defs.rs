// SPDX-License-Identifier: GPL-3.0-or-later
//! Tour content: copy is kept separate from rendering ([`overlay`](super::overlay))
//! and from the state machine ([`super`]). `tour_for` is an exhaustive match
//! over [`TourKey`] — a new key fails to compile until it's added here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::{AnchorId, Step, Tour, TourKey};

fn step(anchor: AnchorId, title: impl Into<String>, body: impl Into<String>) -> Step {
    Step {
        anchor,
        title: title.into(),
        body: body.into(),
    }
}

fn make_tour(steps: Vec<Step>) -> Tour {
    Tour { steps }
}

/// Build a fully-localized `Tour` for `key` against the active locale.
fn build_tour(key: TourKey) -> Tour {
    match key {
        TourKey::PageHome => make_tour(vec![
            step(
                AnchorId::HomeSearch,
                t!("tour.page_home.find_device.title"),
                t!("tour.page_home.find_device.body"),
            ),
            step(
                AnchorId::HomeDeviceCard,
                t!("tour.page_home.manage_device.title"),
                t!("tour.page_home.manage_device.body"),
            ),
            step(
                AnchorId::HomeShowHidden,
                t!("tour.page_home.hidden_devices.title"),
                t!("tour.page_home.hidden_devices.body"),
            ),
            step(
                AnchorId::HomeSidebarHome,
                t!("tour.page_home.sidebar_home.title"),
                t!("tour.page_home.sidebar_home.body"),
            ),
            step(
                AnchorId::HomeSidebarLighting,
                t!("tour.page_home.sidebar_lighting.title"),
                t!("tour.page_home.sidebar_lighting.body"),
            ),
            step(
                AnchorId::HomeSidebarCooling,
                t!("tour.page_home.sidebar_cooling.title"),
                t!("tour.page_home.sidebar_cooling.body"),
            ),
            step(
                AnchorId::HomeSidebarSettings,
                t!("tour.page_home.sidebar_settings.title"),
                t!("tour.page_home.sidebar_settings.body"),
            ),
            step(
                AnchorId::HomeProfile,
                t!("tour.page_home.switch_profiles.title"),
                t!("tour.page_home.switch_profiles.body"),
            ),
        ]),
        TourKey::PageLighting => make_tour(vec![
            step(
                AnchorId::LightingEffects,
                t!("tour.page_lighting.pick_effect.title"),
                t!("tour.page_lighting.pick_effect.body"),
            ),
            step(
                AnchorId::LightingNewEffect,
                t!("tour.page_lighting.create_effect.title"),
                t!("tour.page_lighting.create_effect.body"),
            ),
            step(
                AnchorId::LightingImport,
                t!("tour.page_lighting.import_effect.title"),
                t!("tour.page_lighting.import_effect.body"),
            ),
            step(
                AnchorId::LightingTargets,
                t!("tour.page_lighting.choose_targets.title"),
                t!("tour.page_lighting.choose_targets.body"),
            ),
        ]),
        TourKey::PageCooling => make_tour(vec![step(
            AnchorId::CoolingCurve,
            t!("tour.page_cooling.overview.title"),
            t!("tour.page_cooling.overview.body"),
        )]),
        TourKey::PageCanvas => make_tour(vec![
            step(
                AnchorId::CanvasInstanceRack,
                t!("tour.page_canvas.create_instance.title"),
                t!("tour.page_canvas.create_instance.body"),
            ),
            step(
                AnchorId::CanvasStage,
                t!("tour.page_canvas.arrange_remove.title"),
                t!("tour.page_canvas.arrange_remove.body"),
            ),
        ]),
        TourKey::PageSettings => make_tour(vec![
            step(
                AnchorId::SettingsApplication,
                t!("tour.page_settings.application.title"),
                t!("tour.page_settings.application.body"),
            ),
            step(
                AnchorId::SettingsEngines,
                t!("tour.page_settings.engines.title"),
                t!("tour.page_settings.engines.body"),
            ),
        ]),
        TourKey::PageProfile => make_tour(vec![
            step(
                AnchorId::ProfileHeader,
                t!("tour.page_profile.settings.title"),
                t!("tour.page_profile.settings.body"),
            ),
            step(
                AnchorId::ProfileAddProcess,
                t!("tour.page_profile.auto_activate.title"),
                t!("tour.page_profile.auto_activate.body"),
            ),
            step(
                AnchorId::ProfileOverrides,
                t!("tour.page_profile.overrides.title"),
                t!("tour.page_profile.overrides.body"),
            ),
        ]),
        TourKey::PageDevice => make_tour(vec![
            step(
                AnchorId::DeviceBackLink,
                t!("tour.page_device.back.title"),
                t!("tour.page_device.back.body"),
            ),
            step(
                AnchorId::DeviceHeader,
                t!("tour.page_device.rename.title"),
                t!("tour.page_device.rename.body"),
            ),
            step(
                AnchorId::DeviceTabBar,
                t!("tour.page_device.tabs.title"),
                t!("tour.page_device.tabs.body"),
            ),
        ]),
        TourKey::TabDevices => make_tour(vec![step(
            AnchorId::TabChildrenList,
            t!("tour.tab_devices.connected.title"),
            t!("tour.tab_devices.connected.body"),
        )]),
        TourKey::TabLighting => make_tour(vec![
            step(
                AnchorId::LightingEffectsGrid,
                t!("tour.tab_lighting.choose_effect.title"),
                t!("tour.tab_lighting.choose_effect.body"),
            ),
            step(
                AnchorId::LightingPaintCell,
                t!("tour.tab_lighting.paint.title"),
                t!("tour.tab_lighting.paint.body"),
            ),
            step(
                AnchorId::LightingPlaceCanvas,
                t!("tour.tab_lighting.place_canvas.title"),
                t!("tour.tab_lighting.place_canvas.body"),
            ),
            step(
                AnchorId::LightingTransform,
                t!("tour.tab_lighting.transform.title"),
                t!("tour.tab_lighting.transform.body"),
            ),
        ]),
        TourKey::TabChains => make_tour(vec![
            step(
                AnchorId::TabChains,
                t!("tour.tab_chains.led_chains.title"),
                t!("tour.tab_chains.led_chains.body"),
            ),
            step(
                AnchorId::ChainsDiscover,
                t!("tour.tab_chains.discover.title"),
                t!("tour.tab_chains.discover.body"),
            ),
            step(
                AnchorId::ChainsAddLink,
                t!("tour.tab_chains.add_link.title"),
                t!("tour.tab_chains.add_link.body"),
            ),
        ]),
        TourKey::TabCooling => make_tour(vec![
            step(
                AnchorId::CoolingSensor,
                t!("tour.tab_cooling.pick_sensor.title"),
                t!("tour.tab_cooling.pick_sensor.body"),
            ),
            step(
                AnchorId::CoolingPreset,
                t!("tour.tab_cooling.apply_preset.title"),
                t!("tour.tab_cooling.apply_preset.body"),
            ),
            step(
                AnchorId::CoolingCurveEditor,
                t!("tour.tab_cooling.edit_curve.title"),
                t!("tour.tab_cooling.edit_curve.body"),
            ),
        ]),
        TourKey::TabLcd => make_tour(vec![
            step(
                AnchorId::LcdModeTabs,
                t!("tour.tab_lcd.choose_mode.title"),
                t!("tour.tab_lcd.choose_mode.body"),
            ),
            step(
                AnchorId::TabLcd,
                t!("tour.tab_lcd.preview.title"),
                t!("tour.tab_lcd.preview.body"),
            ),
        ]),
        TourKey::TabEqualizer => make_tour(vec![step(
            AnchorId::TabEqualizer,
            t!("tour.tab_equalizer.equalizer.title"),
            t!("tour.tab_equalizer.equalizer.body"),
        )]),
        TourKey::TabKeys => make_tour(vec![
            step(
                AnchorId::KeysActionCategory,
                t!("tour.tab_keys.select_action.title"),
                t!("tour.tab_keys.select_action.body"),
            ),
            step(
                AnchorId::KeysMacro,
                t!("tour.tab_keys.record_macro.title"),
                t!("tour.tab_keys.record_macro.body"),
            ),
            step(
                AnchorId::KeysLayerShift,
                t!("tour.tab_keys.layer_shift.title"),
                t!("tour.tab_keys.layer_shift.body"),
            ),
        ]),
        TourKey::TabPerformance => make_tour(vec![step(
            AnchorId::TabPerformance,
            t!("tour.tab_performance.dpi_stages.title"),
            t!("tour.tab_performance.dpi_stages.body"),
        )]),
        TourKey::TabControls => make_tour(vec![step(
            AnchorId::TabControls,
            t!("tour.tab_controls.device_controls.title"),
            t!("tour.tab_controls.device_controls.body"),
        )]),
        TourKey::TabOnboard => make_tour(vec![step(
            AnchorId::TabOnboard,
            t!("tour.tab_onboard.onboard_profiles.title"),
            t!("tour.tab_onboard.onboard_profiles.body"),
        )]),
        TourKey::TabPairing => make_tour(vec![step(
            AnchorId::TabPairing,
            t!("tour.tab_pairing.pairing.title"),
            t!("tour.tab_pairing.pairing.body"),
        )]),
        TourKey::LcdEditor => make_tour(vec![
            step(
                AnchorId::LcdEditorPalette,
                t!("tour.lcd_editor.palette.title"),
                t!("tour.lcd_editor.palette.body"),
            ),
            step(
                AnchorId::LcdEditorStage,
                t!("tour.lcd_editor.arrange.title"),
                t!("tour.lcd_editor.arrange.body"),
            ),
            step(
                AnchorId::LcdEditorVariant,
                t!("tour.lcd_editor.variant.title"),
                t!("tour.lcd_editor.variant.body"),
            ),
        ]),
        TourKey::EffectDesigner => make_tour(vec![
            step(
                AnchorId::EffectDesignerSave,
                t!("tour.page_effect_designer.save.title"),
                t!("tour.page_effect_designer.save.body"),
            ),
            step(
                AnchorId::EffectDesignerPreview,
                t!("tour.page_effect_designer.preview.title"),
                t!("tour.page_effect_designer.preview.body"),
            ),
            step(
                AnchorId::EffectDesignerControls,
                t!("tour.page_effect_designer.tune.title"),
                t!("tour.page_effect_designer.tune.body"),
            ),
        ]),
    }
}

/// Every [`TourKey`] variant, for exhaustive testing/proptesting over "any tour".
pub const ALL_TOUR_KEYS: &[TourKey] = &[
    TourKey::PageHome,
    TourKey::PageLighting,
    TourKey::PageCooling,
    TourKey::PageCanvas,
    TourKey::PageSettings,
    TourKey::PageProfile,
    TourKey::PageDevice,
    TourKey::TabDevices,
    TourKey::TabLighting,
    TourKey::TabChains,
    TourKey::TabCooling,
    TourKey::TabLcd,
    TourKey::TabEqualizer,
    TourKey::TabKeys,
    TourKey::TabPerformance,
    TourKey::TabControls,
    TourKey::TabOnboard,
    TourKey::TabPairing,
    TourKey::LcdEditor,
    TourKey::EffectDesigner,
];

/// Localized tours, interned per `(locale, tour)` so the `&'static Tour` shape
/// the tour engine expects is preserved while still honouring a runtime locale
/// switch: the first request for a locale builds and leaks that locale's copy;
/// later requests return the cached reference.
fn cache() -> &'static Mutex<HashMap<(String, &'static str), Arc<Tour>>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), Arc<Tour>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn tour_for(key: TourKey) -> Arc<Tour> {
    let cache_key = (rust_i18n::locale().to_string(), key.id());
    let mut map = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tour) = map.get(&cache_key) {
        return Arc::clone(tour);
    }
    let tour = Arc::new(build_tour(key));
    map.insert(cache_key, Arc::clone(&tour));
    tour
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_tour_has_one_to_ten_steps() {
        for key in ALL_TOUR_KEYS {
            let steps = tour_for(*key).steps.len();
            assert!((1..=10).contains(&steps), "{} has {steps} steps", key.id());
        }
    }

    #[test]
    fn every_tour_id_is_unique_and_non_empty() {
        let mut ids = HashSet::new();
        for key in ALL_TOUR_KEYS {
            assert!(!key.id().is_empty());
            assert!(ids.insert(key.id()), "duplicate tour id: {}", key.id());
        }
    }

    #[test]
    fn every_step_has_non_empty_copy() {
        for key in ALL_TOUR_KEYS {
            for step in &tour_for(*key).steps {
                assert!(!step.title.is_empty());
                assert!(!step.body.is_empty());
            }
        }
    }
}
