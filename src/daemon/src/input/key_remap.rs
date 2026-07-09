// SPDX-License-Identifier: GPL-3.0-or-later
/// Key remap engine — subscribes to ButtonEvents from diverted HID++ buttons,
/// resolves the configured action for each CID, and executes it.
///
/// Layer Shift: one button can be designated as the global modifier. When held,
/// all other button events use their `shifted` action instead of `base`.
///
/// MomentaryDpi: holds the previous DPI until button release, then restores it.
use std::collections::HashMap;
use std::sync::Arc;

use halod_shared::types::{ButtonAction, ButtonMapping, DpiMode};
use tokio::sync::broadcast::error::RecvError;

use crate::input::action_executor::ActionExecutor;
use crate::state::{AppState, ButtonEvent};

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
        tokio::spawn(async move {
            // (device_id, cid) → DPI saved before a momentary press
            let mut held_momentary: HashMap<(String, u16), u16> = HashMap::new();
            loop {
                let event = match rx.recv().await {
                    Ok(e) => e,
                    Err(RecvError::Lagged(n)) => {
                        log::debug!("KeyRemapEngine: lagged {n} events");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                };
                engine.process_event(event, &mut held_momentary).await;
            }
            log::debug!("KeyRemapEngine: stopped");
        })
    }

    async fn process_event(&self, event: ButtonEvent, held: &mut HashMap<(String, u16), u16>) {
        let Some(mappings) = self.get_mappings(&event.device_id).await else {
            return;
        };

        for &cid in &event.released {
            let layer_shift = self
                .app
                .input
                .layer_shift_active
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0;
            let action = Self::resolve(mappings.iter().find(|m| m.cid == cid), layer_shift);
            self.handle_button(action, false, cid, &event.device_id, held)
                .await;
        }
        for &cid in &event.pressed {
            let layer_shift = self
                .app
                .input
                .layer_shift_active
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0;
            let action = Self::resolve(mappings.iter().find(|m| m.cid == cid), layer_shift);
            self.handle_button(action, true, cid, &event.device_id, held)
                .await;
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
        held: &mut HashMap<(String, u16), u16>,
    ) {
        match (action, pressed) {
            (ButtonAction::LayerShift, pressed) => {
                if pressed {
                    self.app
                        .input
                        .layer_shift_active
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                } else {
                    self.app
                        .input
                        .layer_shift_active
                        .fetch_update(
                            std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,
                            |v| if v > 0 { Some(v - 1) } else { Some(0) },
                        )
                        .ok();
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
                        // Restore point is captured now, not re-read from hardware on
                        // release (a double-firing button could shift it in between).
                        if status.mode != DpiMode::Host {
                            // Momentary DPI is host-mode only; in onboard mode
                            // the device's own profiles govern DPI.
                            log::debug!(
                                "KeyRemapEngine: momentary DPI press: not host mode, ignoring"
                            );
                        } else if status.current_dpi == 0 {
                            // No known DPI — leave it alone rather than risk
                            // stranding the device at `dpi`.
                            log::warn!(
                                "KeyRemapEngine: momentary DPI press: no known current DPI; skipping"
                            );
                        } else {
                            held.insert((device_id.to_string(), cid), status.current_dpi);
                            if let Err(e) = sw.set_dpi_direct(*dpi).await {
                                log::warn!("KeyRemapEngine: momentary DPI press: {e}");
                            } else {
                                crate::ipc::broadcast_state(&self.app).await;
                            }
                        }
                    }
                }
            }
            (ButtonAction::MomentaryDpi { .. }, false) => {
                if let Some(saved) = held.remove(&(device_id.to_string(), cid)) {
                    if let Some(device) = self.app.find_device_by_id(device_id).await {
                        if let Some(sw) = device.as_dpi() {
                            if let Err(e) = sw.set_dpi_direct(saved).await {
                                log::warn!("KeyRemapEngine: momentary DPI release: {e}");
                            } else {
                                crate::ipc::broadcast_state(&self.app).await;
                            }
                        }
                    }
                }
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
    use std::sync::atomic::Ordering;

    fn mapping(cid: u16, base: ButtonAction, shifted: ButtonAction) -> ButtonMapping {
        ButtonMapping { cid, base, shifted }
    }

    /// Construct an engine with no ActionExecutor (safe for tests that only
    /// exercise LayerShift and MomentaryDpi paths, which never reach the executor).
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

        assert_eq!(
            app.input.layer_shift_active.load(Ordering::Relaxed),
            1,
            "layer_shift_active must be 1 after press"
        );
    }

    #[tokio::test]
    async fn layer_shift_release_clears_active_flag() {
        let app = Arc::new(AppState::new(Config::default()));
        app.input.layer_shift_active.store(1, Ordering::Relaxed);
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        engine
            .handle_button(&ButtonAction::LayerShift, false, 0, "dev", &mut held)
            .await;

        assert_eq!(
            app.input.layer_shift_active.load(Ordering::Relaxed),
            0,
            "layer_shift_active must be 0 after release"
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
            Some(&800),
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
        held.insert(("dev1".to_string(), 0x10u16), 800u16);

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
        // Simulate a double-firing button: the "hardware" DPI changes to 3200
        // behind the engine's back between press and release. Release must
        // restore the app-state-held 800, not read the live 3200.
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
    async fn process_event_releases_before_presses_in_same_notification() {
        // Layer Shift (CID 0x50) releases in the same notification that a
        // shifted MediaKey (CID 0x51) presses. If releases weren't processed
        // first, the press would still see layer_shift_active=true and pick
        // `shifted` instead of `base`.
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
        app.input.layer_shift_active.store(1, Ordering::Relaxed);
        let dev = Arc::new(MockDevice::new("dev1").with_key_remap_mappings(mappings));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();
        let event = ButtonEvent {
            device_id: "dev1".to_string(),
            pressed: vec![0x51],
            released: vec![0x50],
        };
        engine.process_event(event, &mut held).await;

        assert_eq!(
            app.input.layer_shift_active.load(Ordering::Relaxed),
            0,
            "layer shift release in the same notification must be processed"
        );
    }

    #[tokio::test]
    async fn layer_shift_counter_stays_positive_when_another_device_still_held() {
        // Two LayerShift buttons on different devices: releasing one must not
        // clear the global flag while the other is still held.
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        // Press LayerShift on device A
        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev_a", &mut held)
            .await;
        assert_eq!(app.input.layer_shift_active.load(Ordering::Relaxed), 1);

        // Press LayerShift on device B
        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev_b", &mut held)
            .await;
        assert_eq!(app.input.layer_shift_active.load(Ordering::Relaxed), 2);

        // Release device A's LayerShift — the global shift must still be active
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev_a", &mut held)
            .await;
        assert_eq!(app.input.layer_shift_active.load(Ordering::Relaxed), 1);

        // Release device B's LayerShift — now it should clear
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev_b", &mut held)
            .await;
        assert_eq!(app.input.layer_shift_active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn layer_shift_release_never_drops_below_zero() {
        // A spurious release (e.g. from a double-disconnect synthetic event)
        // must not underflow the counter.
        let app = Arc::new(AppState::new(Config::default()));
        let engine = test_engine(Arc::clone(&app));
        let mut held = HashMap::new();

        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev", &mut held)
            .await;
        assert_eq!(app.input.layer_shift_active.load(Ordering::Relaxed), 0);

        // Press then release twice — second release must still be 0.
        engine
            .handle_button(&ButtonAction::LayerShift, true, 0x50, "dev", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev", &mut held)
            .await;
        engine
            .handle_button(&ButtonAction::LayerShift, false, 0x50, "dev", &mut held)
            .await;
        assert_eq!(app.input.layer_shift_active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cross_device_layer_shift_affects_other_device() {
        // Device A holds LayerShift → Device B's button must resolve to its
        // shifted action. This is the "Layer shift not global" scenario.
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

        // Step 1: press LayerShift on device A
        let event_a = ButtonEvent {
            device_id: "dev_a".to_string(),
            pressed: vec![0x50],
            released: vec![],
        };
        engine.process_event(event_a, &mut held).await;
        assert_eq!(
            app.input.layer_shift_active.load(Ordering::Relaxed),
            1,
            "layer shift must be active after device A press"
        );

        // Step 2: press button on device B — must see layer_shift=true
        let event_b = ButtonEvent {
            device_id: "dev_b".to_string(),
            pressed: vec![0x51],
            released: vec![],
        };
        engine.process_event(event_b, &mut held).await;
        // The counter must still be 1 — device B's press didn't change it
        assert_eq!(
            app.input.layer_shift_active.load(Ordering::Relaxed),
            1,
            "layer shift must remain active after device B press"
        );

        // Step 3: release LayerShift on device A
        let event_a_rel = ButtonEvent {
            device_id: "dev_a".to_string(),
            pressed: vec![],
            released: vec![0x50],
        };
        engine.process_event(event_a_rel, &mut held).await;
        assert_eq!(
            app.input.layer_shift_active.load(Ordering::Relaxed),
            0,
            "layer shift must clear after device A release"
        );
    }
}
