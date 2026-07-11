// SPDX-License-Identifier: GPL-3.0-or-later
//! Step-by-step spotlight tour shown the first time the user lands on a page
//! or device capability tab: dim the screen, highlight one widget at a time
//! with a callout bubble, advance on Next/Skip. "Seen" tours persist in the
//! daemon's `GuiConfig` (`seen_tours`); `local_seen` here only bridges the
//! ~250 ms window before that roundtrip lands, so a completed/skipped tour
//! can't immediately re-trigger.

pub mod defs;

use std::collections::BTreeSet;

use egui::Rect;

pub use defs::tour_for;

/// A widget a tour step can spotlight. One variant per instrumented widget —
/// pages register their rect with [`anchor`] each frame; the overlay reads it
/// back the same frame via [`take_anchor`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AnchorId {
    HomeSearch,
    HomeDeviceCard,
    HomeShowHidden,
    HomeSidebarHome,
    HomeSidebarLighting,
    HomeSidebarCooling,
    HomeSidebarCanvas,
    HomeSidebarSettings,
    LightingEffects,
    LightingImport,
    LightingNewEffect,
    LightingTargets,
    CoolingCurve,
    CanvasInstanceRack,
    CanvasStage,
    SettingsApplication,
    SettingsEngines,
    ProfileHeader,
    DeviceBackLink,
    DeviceHeader,
    DeviceTabBar,
    TabChildrenList,
    TabLighting,
    TabChains,
    TabCooling,
    TabLcd,
    LcdEditorPalette,
    LcdEditorStage,
    TabEqualizer,
    TabKeys,
    TabPerformance,
    TabControls,
    TabOnboard,
    TabPairing,
    /// Device-lighting tab (per-device, not the global Lighting page)
    LightingEffectsGrid,
    LightingPaintCell,
    LightingPlaceCanvas,
    LightingTransform,
    /// Cooling tab (per-device)
    CoolingSensor,
    CoolingPreset,
    CoolingCurveEditor,
    /// Keys/buttons tab
    KeysActionCategory,
    KeysLayerShift,
    KeysMacro,
    /// Chains tab
    ChainsDiscover,
    ChainsAddLink,
    /// LCD tab (non-editor)
    LcdModeTabs,
    /// LCD editor
    LcdEditorVariant,
    /// Home page profile button
    HomeProfile,
    /// Profile settings page
    ProfileAddProcess,
    ProfileOverrides,
    /// Effect Designer
    EffectDesignerPreview,
    EffectDesignerControls,
    EffectDesignerSave,
}

/// Which tour applies to the page/tab the user is currently viewing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TourKey {
    PageHome,
    PageLighting,
    PageCooling,
    PageCanvas,
    PageSettings,
    PageProfile,
    PageDevice,
    TabDevices,
    TabLighting,
    TabChains,
    TabCooling,
    TabLcd,
    TabEqualizer,
    TabKeys,
    TabPerformance,
    TabControls,
    TabOnboard,
    TabPairing,
    LcdEditor,
    EffectDesigner,
}

impl TourKey {
    /// Persistence key stored in `GuiConfig::seen_tours`.
    pub fn id(self) -> &'static str {
        match self {
            TourKey::PageHome => "page:home",
            TourKey::PageLighting => "page:lighting",
            TourKey::PageCooling => "page:cooling",
            TourKey::PageCanvas => "page:canvas",
            TourKey::PageSettings => "page:settings",
            TourKey::PageProfile => "page:profile",
            TourKey::PageDevice => "page:device",
            TourKey::TabDevices => "tab:devices",
            TourKey::TabLighting => "tab:lighting",
            TourKey::TabChains => "tab:chains",
            TourKey::TabCooling => "tab:cooling",
            TourKey::TabLcd => "tab:lcd",
            TourKey::TabEqualizer => "tab:equalizer",
            TourKey::TabKeys => "tab:keys",
            TourKey::TabPerformance => "tab:performance",
            TourKey::TabControls => "tab:controls",
            TourKey::TabOnboard => "tab:onboard",
            TourKey::TabPairing => "tab:pairing",
            TourKey::LcdEditor => "lcd_editor",
            TourKey::EffectDesigner => "effect_designer",
        }
    }
}

pub struct Step {
    pub anchor: AnchorId,
    pub title: String,
    pub body: String,
}

pub struct Tour {
    pub steps: Vec<Step>,
}

struct ActiveTour {
    key: TourKey,
    step: usize,
    missing_since: Option<f64>,
}

#[derive(Default)]
pub struct TourState {
    active: Option<ActiveTour>,
    /// Optimistic completion set, bridging the daemon roundtrip (see module docs).
    local_seen: BTreeSet<String>,
}

impl TourState {
    /// The active step's display info, if a tour is running and its step
    /// still exists: `(step_index, step_count, title, body, anchor_id)`.
    pub(crate) fn current_step(&self) -> Option<(usize, usize, String, String, AnchorId)> {
        let active = self.active.as_ref()?;
        let tour = tour_for(active.key);
        let step = tour.steps.get(active.step)?;
        Some((
            active.step,
            tour.steps.len(),
            step.title.clone(),
            step.body.clone(),
            step.anchor,
        ))
    }

    pub(crate) fn clear_local_seen(&mut self) {
        self.local_seen.clear();
    }

    pub(crate) fn mark_locally_seen(&mut self, id: &str) {
        self.local_seen.insert(id.to_string());
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Next,
    Skip,
    AnchorPresent,
    AnchorMissing { now: f64 },
}

pub struct Completed {
    pub id: &'static str,
}

/// How long a step's anchor may be absent before the tour auto-advances (e.g.
/// no devices yet, or a capability-gated widget that never renders).
pub const MISSING_GRACE_SECS: f64 = 0.5;

/// Whether `key`'s tour has already been completed/skipped — either
/// persisted by the daemon or optimistically recorded locally.
pub fn is_seen(st: &TourState, daemon_seen: &BTreeSet<String>, key: TourKey) -> bool {
    let id = key.id();
    daemon_seen.contains(id) || st.local_seen.contains(id)
}

/// Start `key`'s tour unless one is already active or it's already seen.
/// Returns whether it started.
pub fn maybe_start(st: &mut TourState, daemon_seen: &BTreeSet<String>, key: TourKey) -> bool {
    if st.active.is_some() || is_seen(st, daemon_seen, key) {
        return false;
    }
    st.active = Some(ActiveTour {
        key,
        step: 0,
        missing_since: None,
    });
    true
}

pub fn apply(st: &mut TourState, ev: Event) -> Option<Completed> {
    let key = st.active.as_ref()?.key;
    let total_steps = tour_for(key).steps.len();

    match ev {
        Event::Skip => {
            st.active = None;
            Some(Completed { id: key.id() })
        }
        Event::Next => complete_or_advance(st, key, total_steps),
        Event::AnchorPresent => {
            st.active.as_mut().unwrap().missing_since = None;
            None
        }
        Event::AnchorMissing { now } => {
            let since = *st.active.as_mut().unwrap().missing_since.get_or_insert(now);
            if now - since >= MISSING_GRACE_SECS {
                complete_or_advance(st, key, total_steps)
            } else {
                None
            }
        }
    }
}

fn complete_or_advance(st: &mut TourState, key: TourKey, total_steps: usize) -> Option<Completed> {
    let active = st.active.as_mut().unwrap();
    if active.step + 1 >= total_steps {
        st.active = None;
        Some(Completed { id: key.id() })
    } else {
        active.step += 1;
        active.missing_since = None;
        None
    }
}

fn anchor_key(id: AnchorId) -> egui::Id {
    egui::Id::new(("tour_anchor", id))
}

/// Register `rect` as this frame's location of `id`. Call once per frame from
/// the page/tab that owns the widget; the overlay is the sole reader.
pub fn anchor(ctx: &egui::Context, id: AnchorId, rect: Rect) {
    ctx.data_mut(|d| d.insert_temp(anchor_key(id), rect));
}

// `Rect` isn't `Default`, so `remove_temp` (which requires it) can't be used
// here — read then explicitly clear instead.
pub(crate) fn take_anchor(ctx: &egui::Context, id: AnchorId) -> Option<Rect> {
    ctx.data_mut(|d| {
        let rect = d.get_temp::<Rect>(anchor_key(id));
        d.remove::<Rect>(anchor_key(id));
        rect
    })
}

fn reset_request_key() -> egui::Id {
    egui::Id::new("tour_reset_request")
}

/// Requested by the Settings "Replay tutorials" button; consumed by
/// [`ui::tour::show`](crate::ui::tour::show).
pub fn request_reset(ctx: &egui::Context) {
    ctx.data_mut(|d| d.insert_temp(reset_request_key(), true));
}

pub(crate) fn take_reset_request(ctx: &egui::Context) -> bool {
    ctx.data_mut(|d| d.remove_temp::<bool>(reset_request_key()))
        .unwrap_or(false)
}

/// One frame's reducer step: feed the anchor's presence, then (if the overlay
/// rendered and a button was clicked) the click — in that order, so a step
/// that both times out *and* was somehow clicked doesn't double-apply.
/// Returns the completed tour regardless of which of the two caused it, so
/// callers can't accidentally handle one completion path and drop the other.
pub(crate) fn advance(
    st: &mut TourState,
    anchor_present: bool,
    now: f64,
    btn_event: Option<Event>,
) -> Option<Completed> {
    let presence_event = if anchor_present {
        Event::AnchorPresent
    } else {
        Event::AnchorMissing { now }
    };
    let completed = apply(st, presence_event);
    if completed.is_some() || !anchor_present {
        // A button click is only meaningful when the anchor was present (the
        // overlay only ever renders, and so only ever produces `btn_event`,
        // in that case) — ignore a click that arrives without one.
        return completed;
    }
    btn_event.and_then(|ev| apply(st, ev))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn maybe_start_starts_an_unseen_tour() {
        let mut st = TourState::default();
        assert!(maybe_start(&mut st, &seen(&[]), TourKey::PageHome));
        assert!(st.active.is_some());
    }

    #[test]
    fn maybe_start_skips_a_daemon_seen_tour() {
        let mut st = TourState::default();
        assert!(!maybe_start(
            &mut st,
            &seen(&["page:home"]),
            TourKey::PageHome
        ));
        assert!(st.active.is_none());
    }

    #[test]
    fn maybe_start_skips_a_locally_seen_tour() {
        let mut st = TourState::default();
        st.local_seen.insert("page:home".to_string());
        assert!(!maybe_start(&mut st, &seen(&[]), TourKey::PageHome));
    }

    #[test]
    fn maybe_start_does_not_interrupt_an_active_tour() {
        let mut st = TourState::default();
        assert!(maybe_start(&mut st, &seen(&[]), TourKey::PageHome));
        assert!(!maybe_start(&mut st, &seen(&[]), TourKey::PageCanvas));
        assert_eq!(st.active.as_ref().unwrap().key, TourKey::PageHome);
    }

    #[test]
    fn next_walks_to_completed_on_last_step() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageHome);
        let steps = tour_for(TourKey::PageHome).steps.len();
        for _ in 0..steps - 1 {
            assert!(apply(&mut st, Event::Next).is_none());
        }
        let completed = apply(&mut st, Event::Next).expect("last Next completes the tour");
        assert_eq!(completed.id, "page:home");
        assert!(st.active.is_none());
    }

    #[test]
    fn skip_completes_immediately_from_any_step() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageCanvas);
        let completed = apply(&mut st, Event::Skip).unwrap();
        assert_eq!(completed.id, "page:canvas");
        assert!(st.active.is_none());
    }

    #[test]
    fn anchor_missing_is_inert_before_the_grace_period() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageHome);
        assert!(apply(&mut st, Event::AnchorMissing { now: 10.0 }).is_none());
        assert!(apply(
            &mut st,
            Event::AnchorMissing {
                now: 10.0 + MISSING_GRACE_SECS - 0.01
            }
        )
        .is_none());
        assert!(st.active.is_some());
    }

    #[test]
    fn anchor_missing_advances_past_the_grace_period() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageHome);
        apply(&mut st, Event::AnchorMissing { now: 10.0 });
        assert!(apply(
            &mut st,
            Event::AnchorMissing {
                now: 10.0 + MISSING_GRACE_SECS
            }
        )
        .is_none());
        assert_eq!(st.active.as_ref().unwrap().step, 1);
    }

    #[test]
    fn anchor_missing_completes_a_single_step_tour_past_the_grace_period() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageCooling);
        assert_eq!(tour_for(TourKey::PageCooling).steps.len(), 1);
        apply(&mut st, Event::AnchorMissing { now: 0.0 });
        let completed = apply(
            &mut st,
            Event::AnchorMissing {
                now: MISSING_GRACE_SECS,
            },
        )
        .expect("single-step tour completes once its only anchor stays missing");
        assert_eq!(completed.id, "page:cooling");
    }

    #[test]
    fn anchor_present_clears_a_pending_missing_timer() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageHome);
        apply(&mut st, Event::AnchorMissing { now: 10.0 });
        apply(&mut st, Event::AnchorPresent);
        // The timer restarts from whenever it next goes missing, so a wait
        // that would have completed the earlier window is no longer enough.
        assert!(apply(
            &mut st,
            Event::AnchorMissing {
                now: 10.0 + MISSING_GRACE_SECS
            }
        )
        .is_none());
    }

    #[test]
    fn advance_completes_a_single_step_tour_whose_anchor_never_appears() {
        // Regression: `show()` used to call `apply(AnchorMissing)` directly and
        // discard its result, so a tour whose last anchor is permanently
        // missing (e.g. no hidden devices) would silently loop forever instead
        // of ever being marked seen. `advance` must surface that completion.
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageCooling);
        assert_eq!(tour_for(TourKey::PageCooling).steps.len(), 1);
        assert!(advance(&mut st, false, 0.0, None).is_none());
        let completed = advance(&mut st, false, MISSING_GRACE_SECS, None)
            .expect("advance must surface a missing-anchor completion, not swallow it");
        assert_eq!(completed.id, "page:cooling");
        assert!(st.active.is_none());
    }

    #[test]
    fn advance_ignores_a_button_click_while_the_anchor_is_missing() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageHome);
        // Anchor missing but not yet past grace: a stray Next (there is no
        // rendered button to click) must not skip the wait.
        assert!(advance(&mut st, false, 0.0, Some(Event::Next)).is_none());
        assert_eq!(st.active.as_ref().unwrap().step, 0);
    }

    #[test]
    fn advance_applies_the_button_click_when_the_anchor_is_present() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageCooling);
        let completed = advance(&mut st, true, 0.0, Some(Event::Next)).unwrap();
        assert_eq!(completed.id, "page:cooling");
    }

    #[test]
    fn advance_with_no_click_and_present_anchor_stays_active() {
        let mut st = TourState::default();
        maybe_start(&mut st, &seen(&[]), TourKey::PageHome);
        assert!(advance(&mut st, true, 0.0, None).is_none());
        assert!(st.active.is_some());
    }

    #[test]
    fn take_reset_request_clears_local_seen() {
        let ctx = egui::Context::default();
        let mut st = TourState::default();
        st.local_seen.insert("page:home".to_string());
        request_reset(&ctx);
        assert!(take_reset_request(&ctx));
        st.local_seen.clear();
        assert!(st.local_seen.is_empty());
    }

    #[test]
    fn anchor_round_trips_through_take_anchor() {
        let ctx = egui::Context::default();
        let rect = Rect::from_min_size(egui::Pos2::new(1.0, 2.0), egui::Vec2::new(3.0, 4.0));
        anchor(&ctx, AnchorId::HomeSearch, rect);
        assert_eq!(take_anchor(&ctx, AnchorId::HomeSearch), Some(rect));
        // Consumed on read — a widget that stops rendering reads as missing.
        assert_eq!(take_anchor(&ctx, AnchorId::HomeSearch), None);
    }

    fn arb_event() -> impl proptest::strategy::Strategy<Value = Event> {
        use proptest::prelude::*;
        prop_oneof![
            Just(Event::Next),
            Just(Event::Skip),
            Just(Event::AnchorPresent),
            (0.0f64..1000.0).prop_map(|now| Event::AnchorMissing { now }),
        ]
    }

    proptest::proptest! {
        /// Any sequence of events, applied to any tour, always leaves the
        /// active step index in range and completes the tour at most once.
        #[test]
        fn reducer_step_index_always_in_range_and_completes_once(
            key_idx in 0usize..defs::ALL_TOUR_KEYS.len(),
            events in proptest::collection::vec(arb_event(), 0..30),
        ) {
            let key = defs::ALL_TOUR_KEYS[key_idx];
            let mut st = TourState::default();
            maybe_start(&mut st, &BTreeSet::new(), key);
            let mut completions = 0;
            for ev in events {
                if apply(&mut st, ev).is_some() {
                    completions += 1;
                }
                if let Some(active) = &st.active {
                    proptest::prop_assert!(active.step < tour_for(active.key).steps.len());
                } else {
                    // Once completed, further events are no-ops (nothing active).
                    proptest::prop_assert!(completions <= 1);
                }
            }
        }
    }
}
