// SPDX-License-Identifier: GPL-3.0-or-later
/// Key remap engine: resolves ButtonEvents into actions, tracking Layer Shift and MomentaryDpi state.
use std::collections::HashMap;
use std::sync::Arc;

use halod_shared::types::{ButtonAction, ButtonMapping};
use tokio::sync::broadcast::error::RecvError;

use crate::input::action_executor::ActionExecutor;
use crate::state::{AppState, ButtonEvent};

#[derive(Debug)]
struct PressedAction {
    device_id: String,
    cid: u16,
    resolved_action: ButtonAction,
    pressed_at: std::time::Instant,
}

/// Momentary-DPI lifecycle for one (device_id, cid); no entry means Idle.
#[derive(Debug, Clone, Copy, PartialEq)]
enum MomentaryDpiState {
    Applied {
        original: u16,
        temporary: u16,
    },
    /// A release-time restore failed; retried opportunistically.
    RestorePending {
        original: u16,
    },
}

#[derive(Debug)]
enum ReleaseOutcome {
    /// A real release event arrived for this cid.
    Released,
    /// Force-flushed with no matching release (lag or shutdown).
    Cancelled,
}

pub struct KeyRemapEngine {
    executor: Option<Arc<ActionExecutor>>,
    app: Arc<AppState>,
}

impl KeyRemapEngine {
    pub fn new(executor: Arc<ActionExecutor>, app: Arc<AppState>) -> Self {
        Self {
            executor: Some(executor),
            app,
        }
    }

    pub fn start(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let engine = self;
        let mut rx = engine.app.input.button_event_tx.subscribe();
        let mut shutdown_rx = engine.app.input.shutdown_rx();
        tokio::spawn(async move {
            // (device_id, cid) → DPI saved before a momentary press
            let mut held_momentary: HashMap<(String, u16), MomentaryDpiState> = HashMap::new();
            let mut pressed: HashMap<(String, u16), PressedAction> = HashMap::new();
            loop {
                let event = match tokio::select! {
                    event = rx.recv() => Some(event),
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() { None } else { continue }
                    }
                } {
                    None => break,
                    Some(event) => match event {
                        Ok(e) => e,
                        Err(RecvError::Lagged(n)) => {
                            log::debug!("KeyRemapEngine: lagged {n} events");
                            // Missed events may include releases — cancel all held.
                            engine.release_all(&mut pressed, &mut held_momentary).await;
                            continue;
                        }
                        Err(RecvError::Closed) => break,
                    },
                };
                engine
                    .process_event(event, &mut held_momentary, &mut pressed)
                    .await;
            }
            // On shutdown, cancel anything still held.
            engine.release_all(&mut pressed, &mut held_momentary).await;
            log::debug!("KeyRemapEngine: stopped");
        })
    }

    fn layer_shift_active(&self) -> bool {
        self.app.input.layer_shift_active()
    }

    /// Caller must remove `pa` from `pressed` first — transitions exactly once.
    async fn release_pressed(
        &self,
        pa: PressedAction,
        held: &mut HashMap<(String, u16), MomentaryDpiState>,
        outcome: ReleaseOutcome,
    ) {
        log::trace!(
            "KeyRemapEngine: {outcome:?} cid={} device={} action={:?} held_for={:?}",
            pa.cid,
            pa.device_id,
            pa.resolved_action,
            pa.pressed_at.elapsed()
        );
        self.handle_button(&pa.resolved_action, false, pa.cid, &pa.device_id, held)
            .await;
    }

    /// Cancel every held action and any in-flight macro (lag recovery, shutdown).
    async fn release_all(
        &self,
        pressed: &mut HashMap<(String, u16), PressedAction>,
        held: &mut HashMap<(String, u16), MomentaryDpiState>,
    ) {
        let drained: Vec<PressedAction> = pressed.drain().map(|(_, pa)| pa).collect();
        for pa in drained {
            self.release_pressed(pa, held, ReleaseOutcome::Cancelled)
                .await;
        }
        if let Some(executor) = &self.executor {
            executor.cancel_macro();
        }
    }

    async fn process_event(
        &self,
        event: ButtonEvent,
        held: &mut HashMap<(String, u16), MomentaryDpiState>,
        pressed: &mut HashMap<(String, u16), PressedAction>,
    ) {
        self.retry_pending_dpi_restores(&event.device_id, held)
            .await;
        let mappings = self.get_mappings(&event.device_id).await;
        if mappings.is_none() {
            log::trace!(
                "KeyRemapEngine: no mappings for device={} event pressed={:?} released={:?}",
                event.device_id,
                event.pressed,
                event.released
            );
        }
        let mappings = mappings.unwrap_or_default();

        for &cid in &event.released {
            let key = (event.device_id.clone(), cid);
            match pressed.remove(&key) {
                Some(pa) => {
                    self.release_pressed(pa, held, ReleaseOutcome::Released)
                        .await
                }
                None => log::trace!(
                    "KeyRemapEngine: ignoring release without a recorded press cid={cid} device={}",
                    event.device_id
                ),
            }
        }
        for &cid in &event.pressed {
            let key = (event.device_id.clone(), cid);
            if pressed.contains_key(&key) {
                // Idempotent press: ignore duplicates without a release between.
                continue;
            }
            let layer_shift = self.layer_shift_active();
            let action = Self::resolve(mappings.iter().find(|m| m.cid == cid), layer_shift).clone();
            log::trace!(
                "KeyRemapEngine: press cid={cid} device={} layer_shift={layer_shift} action={action:?}",
                event.device_id
            );
            pressed.insert(
                key,
                PressedAction {
                    device_id: event.device_id.clone(),
                    cid,
                    resolved_action: action.clone(),
                    pressed_at: std::time::Instant::now(),
                },
            );
            self.handle_button(&action, true, cid, &event.device_id, held)
                .await;
        }
    }

    /// Retry any `RestorePending` momentary-DPI entries for `device_id`.
    async fn retry_pending_dpi_restores(
        &self,
        device_id: &str,
        held: &mut HashMap<(String, u16), MomentaryDpiState>,
    ) {
        let pending: Vec<u16> = held
            .iter()
            .filter(|(k, v)| {
                k.0 == device_id && matches!(v, MomentaryDpiState::RestorePending { .. })
            })
            .map(|(k, _)| k.1)
            .collect();
        for cid in pending {
            self.restore_momentary_dpi(device_id, cid, held).await;
        }
    }

    /// On failure, stays `RestorePending` for a later opportunistic retry.
    async fn restore_momentary_dpi(
        &self,
        device_id: &str,
        cid: u16,
        held: &mut HashMap<(String, u16), MomentaryDpiState>,
    ) {
        let key = (device_id.to_string(), cid);
        let original = match held.get(&key) {
            Some(
                MomentaryDpiState::Applied { original, .. }
                | MomentaryDpiState::RestorePending { original },
            ) => *original,
            None => return,
        };
        let Some(device) = self.app.find_device_by_id(device_id).await else {
            held.insert(key, MomentaryDpiState::RestorePending { original });
            return;
        };
        let Some(sw) = device.as_dpi() else {
            held.insert(key, MomentaryDpiState::RestorePending { original });
            return;
        };
        if let Err(e) = sw.set_dpi_direct(original).await {
            log::warn!("KeyRemapEngine: momentary DPI restore failed, will retry: {e}");
            held.insert(key, MomentaryDpiState::RestorePending { original });
        } else {
            held.remove(&key);
            crate::ipc::broadcast_delta(&self.app, &[crate::ipc::Domain::Devices]).await;
        }
    }

    async fn get_mappings(&self, device_id: &str) -> Option<Vec<ButtonMapping>> {
        let device = self.app.find_device_by_id(device_id).await?;
        Some(device.as_key_remap()?.get_key_remap_status().await.mappings)
    }

    fn resolve(mapping: Option<&ButtonMapping>, layer_shift: bool) -> &ButtonAction {
        mapping.map_or(&ButtonAction::Native, |m| {
            if matches!(m.base, ButtonAction::LayerShift) {
                &m.base
            } else if layer_shift {
                &m.shifted
            } else {
                &m.base
            }
        })
    }

    async fn handle_button(
        &self,
        action: &ButtonAction,
        pressed: bool,
        cid: u16,
        device_id: &str,
        held: &mut HashMap<(String, u16), MomentaryDpiState>,
    ) {
        match (action, pressed) {
            (ButtonAction::LayerShift, pressed) => {
                if pressed {
                    self.app.input.layer_shift_press(device_id, cid);
                } else {
                    self.app.input.layer_shift_release(device_id, cid);
                }
                log::debug!(
                    "KeyRemapEngine: Layer Shift {}",
                    if pressed { "engaged" } else { "released" }
                );
            }
            (ButtonAction::MomentaryDpi { dpi }, true) => {
                if let Some(device) = self.app.find_device_by_id(device_id).await {
                    if let Some(sw) = device.as_dpi() {
                        let status = sw.dpi_status().await;
                        // Restore point is captured now, not re-read on release.
                        if status.current_dpi == 0 {
                            // No known DPI — leave it alone rather than strand the device.
                            log::warn!(
                                "KeyRemapEngine: momentary DPI press: no known current DPI; skipping"
                            );
                        } else {
                            if let Err(e) = sw.set_dpi_direct(*dpi).await {
                                log::warn!("KeyRemapEngine: momentary DPI press: {e}");
                            } else {
                                held.insert(
                                    (device_id.to_string(), cid),
                                    MomentaryDpiState::Applied {
                                        original: status.current_dpi,
                                        temporary: *dpi,
                                    },
                                );
                                crate::ipc::broadcast_delta(&self.app, &[crate::ipc::Domain::Devices]).await;
                            }
                        }
                    }
                }
            }
            (ButtonAction::MomentaryDpi { .. }, false) => {
                self.restore_momentary_dpi(device_id, cid, held).await;
            }
            (other, pressed) => {
                if let Some(ex) = &self.executor {
                    ex.execute(other, pressed, device_id, Arc::clone(&self.app))
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, drivers::Device, test_support::MockDevice};

    fn mapping(cid: u16, base: ButtonAction, shifted: ButtonAction) -> ButtonMapping {
        ButtonMapping { cid, base, shifted }
    }

    /// Engine with no ActionExecutor, for tests that never reach it.
    fn test_engine(app: Arc<AppState>) -> KeyRemapEngine {
        KeyRemapEngine {
            executor: None,
            app,
        }
    }

    #[test]
    fn layer_shift_button_resolves_to_base_while_engaged() {
        let m = mapping(0x50, ButtonAction::LayerShift, ButtonAction::Native);
        assert_eq!(
            KeyRemapEngine::resolve(Some(&m), true),
            &ButtonAction::LayerShift
        );
        assert_eq!(
            KeyRemapEngine::resolve(Some(&m), false),
            &ButtonAction::LayerShift
        );
    }

    #[test]
    fn non_modifier_button_honours_layer_shift_state() {
        let m = mapping(
            0x51,
            ButtonAction::MediaKey {
                key: halod_shared::types::MediaAction::Play,
            },
            ButtonAction::MediaKey {
                key: halod_shared::types::MediaAction::Next,
            },
        );
        assert_eq!(KeyRemapEngine::resolve(Some(&m), false), &m.base);
        assert_eq!(KeyRemapEngine::resolve(Some(&m), true), &m.shifted);
    }

    #[test]
    fn missing_mapping_resolves_native() {
        assert_eq!(KeyRemapEngine::resolve(None, false), &ButtonAction::Native);
        assert_eq!(KeyRemapEngine::resolve(None, true), &ButtonAction::Native);
    }

    #[tokio::test]
    async fn layer_shift_press_sets_active_flag() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        engine
            .handle_button(&ButtonAction::LayerShift, true, 0, "dev", &mut held)
            .await;

        assert!(engine.layer_shift_active());
    }

    #[tokio::test]
    async fn layer_shift_release_clears_active_flag() {
        let app = Arc::new(AppState::new(Config::default()));
        app.input.layer_shift_press("dev", 0);
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        engine
            .handle_button(&ButtonAction::LayerShift, false, 0, "dev", &mut held)
            .await;

        assert!(!engine.layer_shift_active());
    }

    #[tokio::test]
    async fn layer_shift_duplicate_press_is_idempotent() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        // Double-fire the same token, then release once.
        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev", &mut held)
            .await;

        assert!(
            !engine.layer_shift_active(),
            "one release must fully clear a duplicated press"
        );
    }

    #[tokio::test]
    async fn momentary_dpi_press_saves_restore_point_and_applies_target() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_dpi_initial(800));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        engine
            .handle_button(
                &ButtonAction::MomentaryDpi { dpi: 1600 },
                true,
                0x10,
                "dev1",
                &mut held,
            )
            .await;

        assert_eq!(
            held.get(&("dev1".to_string(), 0x10)),
            Some(&MomentaryDpiState::Applied {
                original: 800,
                temporary: 1600,
            }),
            "restore point must be the initial DPI"
        );
        assert_eq!(
            *dev.dpi_direct_last.as_ref().unwrap().lock().unwrap(),
            Some(1600),
            "set_dpi_direct must have been called with the target DPI"
        );
    }

    #[tokio::test]
    async fn momentary_dpi_release_restores_saved_dpi() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_dpi_initial(800));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        held.insert(
            ("dev1".to_string(), 0x10u16),
            MomentaryDpiState::Applied {
                original: 800,
                temporary: 1600,
            },
        );

        engine
            .handle_button(
                &ButtonAction::MomentaryDpi { dpi: 1600 },
                false,
                0x10,
                "dev1",
                &mut held,
            )
            .await;

        assert!(held.is_empty(), "held entry must be removed on release");
        assert_eq!(
            *dev.dpi_direct_last.as_ref().unwrap().lock().unwrap(),
            Some(800),
            "set_dpi_direct must restore the saved DPI"
        );
    }

    #[tokio::test]
    async fn momentary_dpi_release_without_prior_press_does_not_panic() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        // No device registered and no entry in held — must be a no-op.
        engine
            .handle_button(
                &ButtonAction::MomentaryDpi { dpi: 1600 },
                false,
                0x10,
                "dev1",
                &mut held,
            )
            .await;

        assert!(held.is_empty());
    }

    #[tokio::test]
    async fn momentary_dpi_release_uses_held_value_not_live_hardware() {
        // Release must restore the app-state-held DPI, not the live hardware value.
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_dpi_initial(800));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        engine
            .handle_button(
                &ButtonAction::MomentaryDpi { dpi: 1600 },
                true,
                0x10,
                "dev1",
                &mut held,
            )
            .await;

        // External change to the "live" hardware DPI after the press.
        *dev.dpi_direct_last.as_ref().unwrap().lock().unwrap() = Some(3200);

        engine
            .handle_button(
                &ButtonAction::MomentaryDpi { dpi: 1600 },
                false,
                0x10,
                "dev1",
                &mut held,
            )
            .await;

        assert_eq!(
            *dev.dpi_direct_last.as_ref().unwrap().lock().unwrap(),
            Some(800),
            "release must restore the held app-state value, not the live hardware value"
        );
    }

    #[tokio::test]
    async fn momentary_dpi_restore_failure_enters_restore_pending_and_retries() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_dpi_initial(800));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        engine
            .handle_button(
                &ButtonAction::MomentaryDpi { dpi: 1600 },
                true,
                0x10,
                "dev1",
                &mut held,
            )
            .await;

        // Device disappears before release — the restore can't reach it.
        app.devices.write().await.clear();
        engine
            .handle_button(
                &ButtonAction::MomentaryDpi { dpi: 1600 },
                false,
                0x10,
                "dev1",
                &mut held,
            )
            .await;
        assert_eq!(
            held.get(&("dev1".to_string(), 0x10)),
            Some(&MomentaryDpiState::RestorePending { original: 800 }),
            "a failed restore must stay pending, not strand silently"
        );

        // Device reappears; the next event for it retries the restore.
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);
        engine
            .process_event(
                ButtonEvent {
                    device_id: "dev1".to_string(),
                    pressed: vec![],
                    released: vec![],
                },
                &mut held,
                &mut HashMap::new(),
            )
            .await;
        assert!(
            held.is_empty(),
            "retry must succeed and clear the pending entry"
        );
        assert_eq!(
            *dev.dpi_direct_last.as_ref().unwrap().lock().unwrap(),
            Some(800)
        );
    }

    #[tokio::test]
    async fn process_event_releases_before_presses_in_same_notification() {
        // Releases must be processed before presses in the same notification.
        use halod_shared::types::MediaAction;
        let mappings = vec![
            mapping(0x50, ButtonAction::LayerShift, ButtonAction::Native),
            mapping(
                0x51,
                ButtonAction::MediaKey {
                    key: MediaAction::Play,
                },
                ButtonAction::MediaKey {
                    key: MediaAction::Next,
                },
            ),
        ];
        let app = Arc::new(AppState::new(Config::default()));
        app.input.layer_shift_press("dev1", 0x50);
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap_mappings(mappings));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        let mut pressed = HashMap::from([(
            ("dev1".to_string(), 0x50),
            PressedAction {
                device_id: "dev1".to_string(),
                cid: 0x50,
                resolved_action: ButtonAction::LayerShift,
                pressed_at: std::time::Instant::now(),
            },
        )]);
        let event = ButtonEvent {
            device_id: "dev1".to_string(),
            pressed: vec![0x51],
            released: vec![0x50],
        };
        engine.process_event(event, &mut held, &mut pressed).await;

        assert!(
            !engine.layer_shift_active(),
            "layer shift release in the same notification must be processed"
        );
    }

    #[tokio::test]
    async fn layer_shift_stays_active_while_another_device_still_holds_it() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev_a", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev_b", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev_a", &mut held)
            .await;
        assert!(engine.layer_shift_active(), "dev_b's token is still held");

        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev_b", &mut held)
            .await;
        assert!(!engine.layer_shift_active());
    }

    #[tokio::test]
    async fn layer_shift_spurious_release_is_a_no_op() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev", &mut held)
            .await;
        assert!(!engine.layer_shift_active());

        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev", &mut held)
            .await;
        assert!(
            !engine.layer_shift_active(),
            "a second release must be a no-op"
        );
    }

    #[tokio::test]
    async fn cross_device_layer_shift_affects_other_device() {
        // Device A holds LayerShift — device B's button must resolve shifted too.
        use halod_shared::types::MediaAction;
        let dev_a_mappings = vec![mapping(
            0x50,
            ButtonAction::LayerShift,
            ButtonAction::Native,
        )];
        let dev_b_mappings = vec![mapping(
            0x51,
            ButtonAction::MediaKey {
                key: MediaAction::Play,
            },
            ButtonAction::MediaKey {
                key: MediaAction::Next,
            },
        )];

        let app = Arc::new(AppState::new(Config::default()));
        let dev_a = Arc::new(MockDevice::new("dev_a").with_key_remap_mappings(dev_a_mappings));
        let dev_b = Arc::new(MockDevice::new("dev_b").with_key_remap_mappings(dev_b_mappings));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev_a) as Arc<dyn Device>);
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev_b) as Arc<dyn Device>);

        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        let mut pressed = HashMap::new();

        // Step 1: press LayerShift on device A
        let event_a = ButtonEvent {
            device_id: "dev_a".to_string(),
            pressed: vec![0x50],
            released: vec![],
        };
        engine.process_event(event_a, &mut held, &mut pressed).await;
        assert!(engine.layer_shift_active());

        // Step 2: press button on device B — must see layer_shift=true
        let event_b = ButtonEvent {
            device_id: "dev_b".to_string(),
            pressed: vec![0x51],
            released: vec![],
        };
        engine.process_event(event_b, &mut held, &mut pressed).await;
        assert!(
            engine.layer_shift_active(),
            "device B's press didn't change it"
        );

        // Step 3: release LayerShift on device A
        let event_a_rel = ButtonEvent {
            device_id: "dev_a".to_string(),
            pressed: vec![],
            released: vec![0x50],
        };
        engine
            .process_event(event_a_rel, &mut held, &mut pressed)
            .await;
        assert!(!engine.layer_shift_active());
    }

    #[tokio::test]
    async fn release_runs_the_action_resolved_at_press_not_current() {
        // Release must replay the shifted chord pressed, even after Layer Shift drops.
        use halod_shared::types::MediaAction;
        let shifted = ButtonAction::KeyChord {
            key: 0x04, // 'a'
            modifiers: vec![],
        };
        let mappings = vec![mapping(
            0x51,
            ButtonAction::MediaKey {
                key: MediaAction::Play,
            },
            shifted.clone(),
        )];
        let app = Arc::new(AppState::new(Config::default()));
        app.input.layer_shift_press("shift-btn", 0x50);
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap_mappings(mappings));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        let mut pressed = HashMap::new();

        // Press 0x51 while shifted → resolves to the shifted chord and is stored.
        engine
            .process_event(
                ButtonEvent {
                    device_id: "dev1".to_string(),
                    pressed: vec![0x51],
                    released: vec![],
                },
                &mut held,
                &mut pressed,
            )
            .await;
        assert_eq!(
            pressed
                .get(&("dev1".to_string(), 0x51))
                .map(|pa| &pa.resolved_action),
            Some(&shifted),
            "press must store the shifted action"
        );

        // Layer Shift drops before the button is released.
        app.input.layer_shift_release("shift-btn", 0x50);

        // Release 0x51 → must use the stored shifted action, and clear it.
        engine
            .process_event(
                ButtonEvent {
                    device_id: "dev1".to_string(),
                    pressed: vec![],
                    released: vec![0x51],
                },
                &mut held,
                &mut pressed,
            )
            .await;
        assert!(
            pressed.is_empty(),
            "release must clear the stored pressed action"
        );
    }

    #[tokio::test]
    async fn duplicate_press_is_ignored() {
        // A repeated press with no release between must not store or fire twice.
        use halod_shared::types::MediaAction;
        let mappings = vec![mapping(
            0x51,
            ButtonAction::MediaKey {
                key: MediaAction::Play,
            },
            ButtonAction::Native,
        )];
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap_mappings(mappings));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        let mut pressed = HashMap::new();
        let press = || ButtonEvent {
            device_id: "dev1".to_string(),
            pressed: vec![0x51],
            released: vec![],
        };
        engine.process_event(press(), &mut held, &mut pressed).await;
        engine.process_event(press(), &mut held, &mut pressed).await;
        assert_eq!(
            pressed.len(),
            1,
            "duplicate press must not add a second entry"
        );
    }

    #[tokio::test]
    async fn duplicate_release_finds_no_record_and_does_not_panic() {
        // A PressedAction transitions away from held exactly once.
        use halod_shared::types::MediaAction;
        let mappings = vec![mapping(
            0x51,
            ButtonAction::MediaKey {
                key: MediaAction::Play,
            },
            ButtonAction::Native,
        )];
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap_mappings(mappings));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        let mut pressed = HashMap::new();
        let release = || ButtonEvent {
            device_id: "dev1".to_string(),
            pressed: vec![],
            released: vec![0x51],
        };

        engine
            .process_event(
                ButtonEvent {
                    device_id: "dev1".to_string(),
                    pressed: vec![0x51],
                    released: vec![],
                },
                &mut held,
                &mut pressed,
            )
            .await;
        assert_eq!(pressed.len(), 1);

        engine
            .process_event(release(), &mut held, &mut pressed)
            .await;
        assert!(pressed.is_empty());

        // A second release with no matching record must not panic.
        engine
            .process_event(release(), &mut held, &mut pressed)
            .await;
        assert!(pressed.is_empty(), "still nothing to remove");
    }

    #[tokio::test]
    async fn release_all_cancels_every_pressed_action() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        let mut pressed = HashMap::new();
        for cid in [1u16, 2] {
            pressed.insert(
                ("dev1".to_string(), cid),
                PressedAction {
                    device_id: "dev1".to_string(),
                    cid,
                    resolved_action: ButtonAction::Native,
                    pressed_at: std::time::Instant::now(),
                },
            );
        }

        engine.release_all(&mut pressed, &mut held).await;

        assert!(
            pressed.is_empty(),
            "release_all must drain every held PressedAction"
        );
    }
}
