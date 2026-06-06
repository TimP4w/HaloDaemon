mod templates;

use std::{collections::HashMap, sync::Arc, time::Instant};

use base64::Engine as _;
use halod_protocol::types::{
    EffectParamValue, LcdEngineFrame, LcdEngineTemplateDescriptor, Sensor, WireLcdEngineState,
};
use tokio::sync::{watch, Mutex};

use crate::state::{AppState, EngineRunConfig};
use templates::{LcdTemplate, TemplateCtx};

type FrameTx = tokio::sync::broadcast::Sender<Arc<LcdEngineFrame>>;

/// A template id paired with the parameter values it was built from.
type TemplateSpec = (String, HashMap<String, EffectParamValue>);

struct DeviceSlot {
    template_id: String,
    params: HashMap<String, EffectParamValue>,
    template: Box<dyn LcdTemplate>,
    frame_id: u64,
}

pub struct LcdEngine {
    app_state: Arc<AppState>,
    /// Per-device live template instances, keyed by device_id.
    device_slots: Mutex<HashMap<String, DeviceSlot>>,
    frame_tx: FrameTx,
}

impl LcdEngine {
    pub fn new(app_state: Arc<AppState>) -> Arc<Self> {
        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        Arc::new(Self {
            app_state,
            device_slots: Mutex::new(HashMap::new()),
            frame_tx,
        })
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<LcdEngineFrame>> {
        self.frame_tx.subscribe()
    }

    /// Hot-swap the template (and its params) for a device without waiting for
    /// the next tick.
    pub async fn set_template_active(
        &self,
        device_id: &str,
        template_id: &str,
        params: &HashMap<String, EffectParamValue>,
    ) {
        if let Some(tmpl) = templates::build(template_id, params) {
            let mut slots = self.device_slots.lock().await;
            let frame_id = slots.get(device_id).map(|s| s.frame_id).unwrap_or(0);
            slots.insert(
                device_id.to_string(),
                DeviceSlot {
                    template_id: template_id.to_string(),
                    params: params.clone(),
                    template: tmpl,
                    frame_id,
                },
            );
        }
    }

    /// Remove a device from the engine (called when deactivating engine mode).
    pub async fn remove_device(&self, device_id: &str) {
        self.device_slots.lock().await.remove(device_id);
    }

    pub fn available_template_descriptors() -> Vec<LcdEngineTemplateDescriptor> {
        templates::all_descriptors()
    }

    pub fn template_exists(id: &str) -> bool {
        templates::build(id, &HashMap::new()).is_some()
    }

    pub fn wire_state(device_templates: HashMap<String, String>) -> WireLcdEngineState {
        WireLcdEngineState {
            available_templates: templates::all_descriptors(),
            device_templates,
        }
    }

    pub async fn start(
        self: Arc<Self>,
        cfg_rx: watch::Receiver<EngineRunConfig>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let start = Instant::now();

            crate::engines::engine_run_loop(
                "LCD",
                cfg_rx,
                tokio::time::MissedTickBehavior::Skip,
                |_cfg| {
                    let this = Arc::clone(&self);
                    let t = start.elapsed().as_secs_f64();
                    async move { this.tick(t).await }
                },
            )
            .await;
        })
    }

    async fn tick(&self, t: f64) {
        // Collect live sensor readings once per tick.
        let sensors = self.collect_sensors().await;

        let devices = self.app_state.devices.lock().await.clone();

        // Build the set of (device_id → template id + params) from connected devices' traits.
        let device_templates: HashMap<String, TemplateSpec> = devices
            .iter()
            .filter_map(|d| {
                let lcd = d.as_lcd()?;
                let template_id = lcd.lcd_template_id()?;
                Some((d.id(), (template_id, lcd.lcd_template_params())))
            })
            .collect();

        let mut slots = self.device_slots.lock().await;

        // Ensure slots are up-to-date with device trait state.
        // Add missing slots; replace slots whose template_id or params changed.
        for (device_id, (template_id, params)) in &device_templates {
            let needs_insert = match slots.get(device_id) {
                Some(slot) => slot.template_id != *template_id || slot.params != *params,
                None => true,
            };
            if needs_insert {
                if let Some(tmpl) = templates::build(template_id, params) {
                    let frame_id = slots.get(device_id.as_str()).map(|s| s.frame_id).unwrap_or(0);
                    slots.insert(
                        device_id.clone(),
                        DeviceSlot {
                            template_id: template_id.clone(),
                            params: params.clone(),
                            template: tmpl,
                            frame_id,
                        },
                    );
                }
            }
        }

        // Remove slots for devices whose template was cleared or who are no longer connected.
        // `device_templates` is already built from `devices`, so any key not in it is offline or cleared.
        slots.retain(|id, _| device_templates.contains_key(id));

        // Render and push each device.
        for (device_id, slot) in slots.iter_mut() {
            let Some(device) = devices.iter().find(|d| d.id() == *device_id) else {
                log::debug!("LCD engine: device {device_id} not found, skipping");
                continue;
            };
            let Some(lcd) = device.as_lcd() else {
                log::warn!("LCD engine: device {device_id} has no LCD capability");
                continue;
            };

            let descriptor = lcd.lcd_descriptor();
            let ctx = TemplateCtx {
                width: descriptor.width,
                height: descriptor.height,
                t,
                frame: slot.frame_id,
                sensors: &sensors,
            };

            let tr = std::time::Instant::now();
            let img = match tokio::task::block_in_place(|| slot.template.render(&ctx)) {
                Ok(img) => img,
                Err(e) => {
                    log::warn!("LCD engine: template render error for {device_id}: {e}");
                    continue;
                }
            };
            let (frame_w, frame_h) = img.dimensions();
            log::trace!(
                "[LCD engine timing] template render: {}ms ({frame_w}x{frame_h})",
                tr.elapsed().as_millis()
            );

            slot.frame_id += 1;

            // Broadcast the preview immediately so the UI updates at render rate,
            // not at device-push rate (the stream_frame call below blocks the tick).
            let preview_b64 = match templates::encode_png(&img) {
                Ok(png) => base64::engine::general_purpose::STANDARD.encode(&png),
                Err(e) => {
                    log::warn!("LCD engine: preview encode error for {device_id}: {e}");
                    String::new()
                }
            };
            let frame = Arc::new(LcdEngineFrame {
                device_id: device_id.clone(),
                frame_id: slot.frame_id,
                preview_b64,
            });
            let _ = self.frame_tx.send(frame);

            // Stream the raw RGBA frame straight to the panel — no GIF, no
            // bucket pipeline. This is the type-0x08 path NZXT CAM uses.
            let ts = std::time::Instant::now();
            let raw = img.into_raw();
            if let Err(e) = lcd.stream_frame(&raw, frame_w, frame_h).await {
                log::warn!("LCD engine: stream_frame failed for {device_id}: {e}");
            }
            log::trace!("[LCD engine timing] stream_frame: {}ms", ts.elapsed().as_millis());
        }
    }

    async fn collect_sensors(&self) -> HashMap<String, Sensor> {
        let mut map = HashMap::new();
        let devices = self.app_state.devices.lock().await.clone();
        for device in &devices {
            if let Some(cap) = device.as_sensor_capability() {
                if let Ok(sensors) = cap.get_sensors().await {
                    for s in sensors {
                        map.insert(s.id.clone(), s);
                    }
                }
            }
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[tokio::test]
    async fn new_engine_has_no_slots() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = LcdEngine::new(app);
        assert!(engine.device_slots.lock().await.is_empty());
    }
}
