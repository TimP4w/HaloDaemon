/// Key remap engine — subscribes to ButtonEvents from diverted HID++ buttons,
/// resolves the configured action for each CID, and executes it.
///
/// Layer Shift: one button can be designated as the global modifier. When held,
/// all other button events use their `shifted` action instead of `base`.
///
/// MomentaryDpi: holds the previous DPI until button release, then restores it.
use std::collections::HashMap;
use std::sync::Arc;

use halod_protocol::types::{ButtonAction, ButtonMapping, DpiMode};
use tokio::sync::broadcast::error::RecvError;

use crate::engines::action_executor::ActionExecutor;
use crate::state::{AppState, ButtonEvent};

pub struct KeyRemapEngine {
    executor: Arc<ActionExecutor>,
    app: Arc<AppState>,
}

impl KeyRemapEngine {
    pub fn new(executor: Arc<ActionExecutor>, app: Arc<AppState>) -> Self {
        Self { executor, app }
    }

    pub fn start(self: Arc<Self>) {
        let engine = self;
        let mut rx = engine.app.button_event_tx.subscribe();
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
        });
    }

    async fn process_event(&self, event: ButtonEvent, held: &mut HashMap<(String, u16), u16>) {
        let Some(mappings) = self.get_mappings(&event.device_id).await else {
            return;
        };
        log::trace!(
            "KeyRemapEngine: event pressed={:?} released={:?} mappings={:?}",
            event.pressed,
            event.released,
            mappings
        );

        // Layer Shift is re-read per control: when the modifier button and a
        // target button arrive in the same notification, handling the modifier
        // first must change how the target resolves. Releases are processed
        // first so momentary DPI is restored before the next press.
        for &cid in &event.released {
            let layer_shift = *self.app.layer_shift_active.lock().await;
            let action = Self::resolve(mappings.iter().find(|m| m.cid == cid), layer_shift);
            log::trace!(
                "KeyRemapEngine: release cid={cid} layer_shift={layer_shift} -> {action:?}"
            );
            self.handle_button(action, false, cid, &event.device_id, held)
                .await;
        }
        for &cid in &event.pressed {
            let layer_shift = *self.app.layer_shift_active.lock().await;
            let action = Self::resolve(mappings.iter().find(|m| m.cid == cid), layer_shift);
            log::trace!("KeyRemapEngine: press cid={cid} layer_shift={layer_shift} -> {action:?}");
            self.handle_button(action, true, cid, &event.device_id, held)
                .await;
        }
    }

    async fn get_mappings(&self, device_id: &str) -> Option<Vec<ButtonMapping>> {
        let device = self.app.find_device_by_id(device_id).await?;
        Some(device.as_key_remap()?.get_key_remap_status().await.mappings)
    }

    fn resolve<'a>(mapping: Option<&'a ButtonMapping>, layer_shift: bool) -> &'a ButtonAction {
        mapping.map_or(&ButtonAction::Native, |m| {
            // The Layer Shift modifier button itself must never be shifted: while
            // Layer Shift is engaged its release would otherwise resolve to
            // `shifted` and the modifier would never disengage.
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
                *self.app.layer_shift_active.lock().await = pressed;
                log::debug!(
                    "KeyRemapEngine: Layer Shift {}",
                    if pressed { "engaged" } else { "released" }
                );
            }
            (ButtonAction::MomentaryDpi { dpi }, true) => {
                if let Some(device) = self.app.find_device_by_id(device_id).await {
                    if let Some(sw) = device.as_dpi() {
                        let status = sw.dpi_status().await;
                        // Restore point = the user's selected DPI from app state,
                        // never a live hardware read. The device firmware can
                        // shift the hardware DPI out from under us when a
                        // remapped button also fires its native DPI action
                        // (double-fire); a hardware read here would capture the
                        // already-shifted value and the release would restore
                        // to the wrong (low) DPI.
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
                                crate::ipc::broadcast_state(Arc::clone(&self.app)).await;
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
                                crate::ipc::broadcast_state(Arc::clone(&self.app)).await;
                            }
                        }
                    }
                }
            }
            (other, pressed) => {
                self.executor
                    .execute(other, pressed, device_id, Arc::clone(&self.app))
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(cid: u16, base: ButtonAction, shifted: ButtonAction) -> ButtonMapping {
        ButtonMapping { cid, base, shifted }
    }

    #[test]
    fn layer_shift_button_resolves_to_base_while_engaged() {
        // The modifier button: base = LayerShift, shifted = something else.
        let m = mapping(0x50, ButtonAction::LayerShift, ButtonAction::Native);
        // While Layer Shift is engaged, releasing the modifier must still resolve
        // to LayerShift so the engine disengages it — not to `shifted`.
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
                key: halod_protocol::types::MediaAction::Play,
            },
            ButtonAction::MediaKey {
                key: halod_protocol::types::MediaAction::Next,
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
}
