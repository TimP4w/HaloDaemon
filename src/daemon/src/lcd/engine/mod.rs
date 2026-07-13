// SPDX-License-Identifier: GPL-3.0-or-later
pub(crate) mod custom;
mod templates;
pub mod video;

use std::{collections::HashMap, sync::Arc, time::Instant};

use halod_shared::types::{EffectParamValue, LcdEngineFrame, WireLcdEngineState};
use image::RgbaImage;
use tokio::sync::{watch, Mutex};

use crate::state::{AppState, EngineRunConfig};
use custom::CustomTemplate;
use templates::TemplateCtx;

/// An LCD preview frame already encoded to its framed `lcd_engine_frame` wire
/// message. Built once per broadcast so fanning out to N clients is N cheap
/// `Arc` clones instead of N full JSON serializations of the base64 payload.
pub struct LcdWireFrame {
    pub wire: Arc<Vec<u8>>,
}

/// Wire-encode one preview frame; `None` if it exceeds the frame size cap.
pub(crate) fn encode_wire_frame(
    device_id: &str,
    frame_id: u64,
    preview_b64: &str,
) -> Option<Arc<LcdWireFrame>> {
    let frame = LcdEngineFrame {
        device_id: device_id.to_string(),
        frame_id,
        preview_b64: preview_b64.to_string(),
    };
    let wire = halod_shared::frames::encode_json_frame(
        &serde_json::json!({"type": "lcd_engine_frame", "data": frame}),
    )?;
    Some(Arc::new(LcdWireFrame {
        wire: Arc::new(wire),
    }))
}

pub type FrameTx = tokio::sync::broadcast::Sender<Arc<LcdWireFrame>>;

/// A template id paired with the parameter values it was built from.
type TemplateSpec = (String, HashMap<String, EffectParamValue>);

/// Whether replacing a device's slot should `malloc_trim`: only when an old
/// template instance (and its caches) is actually dropped — never on the first
/// insert for a device, where there's nothing to reclaim.
fn should_trim_on_swap(had_existing_slot: bool) -> bool {
    had_existing_slot
}

struct DeviceSlot {
    template_id: String,
    params: HashMap<String, EffectParamValue>,
    template: CustomTemplate,
    frame_id: u64,
    /// Consecutive `stream_frame` failures. Used to back off (and stop log
    /// flooding) when a device has gone away but hotplug hasn't removed it yet.
    fail_streak: u8,
    last_sig: Option<u64>,
    /// Reusable RGBA frame buffer, swapped with the template's render output
    /// each tick to avoid per-frame allocation.
    frame_buf: RgbaImage,
    /// Reusable PNG encode scratch buffer, cleared each preview tick.
    png_buf: Vec<u8>,
    /// Reusable base64 encode scratch buffer, cleared each preview tick.
    b64_buf: String,
}

const FAIL_BACKOFF_THRESHOLD: u8 = 3;
const BACKOFF_TICKS: u8 = 30;

fn build_template(id: &str, params: &HashMap<String, EffectParamValue>) -> Option<CustomTemplate> {
    (id == "custom").then(|| CustomTemplate::from_params(params, &crate::config::lcd_images_dir()))
}

/// Poll interval for evicting idle editor sessions; independent of the render tick.
const EDITOR_SESSION_EVICT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub struct LcdEngine {
    app_state: Arc<AppState>,
    /// Per-device live template instances, keyed by device_id.
    device_slots: Mutex<HashMap<String, DeviceSlot>>,
    frame_tx: FrameTx,
    /// Wakes the render loop the instant a template becomes active, so an idle
    /// engine starts rendering without waiting for the next poll.
    wake: tokio::sync::Notify,
    prev_receiver_count: std::sync::atomic::AtomicUsize,
}

impl LcdEngine {
    pub fn new(app_state: Arc<AppState>) -> Arc<Self> {
        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        Arc::new(Self {
            app_state,
            device_slots: Mutex::new(HashMap::new()),
            frame_tx,
            wake: tokio::sync::Notify::new(),
            prev_receiver_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<LcdWireFrame>> {
        self.frame_tx.subscribe()
    }

    pub fn frame_sender(&self) -> FrameTx {
        self.frame_tx.clone()
    }

    /// Hot-swap the template (and its params) for a device without waiting for
    /// the next tick.
    pub async fn set_template_active(
        &self,
        device_id: &str,
        template_id: &str,
        params: &HashMap<String, EffectParamValue>,
    ) {
        if let Some(tmpl) = build_template(template_id, params) {
            let mut slots = self.device_slots.lock().await;
            let existed = slots.contains_key(device_id);
            let frame_id = slots.get(device_id).map(|s| s.frame_id).unwrap_or(0);
            slots.insert(
                device_id.to_string(),
                DeviceSlot {
                    template_id: template_id.to_string(),
                    params: params.clone(),
                    template: tmpl,
                    frame_id,
                    fail_streak: 0,
                    last_sig: None,
                    frame_buf: RgbaImage::new(0, 0),
                    png_buf: Vec::new(),
                    b64_buf: String::new(),
                },
            );
            drop(slots);
            if should_trim_on_swap(existed) {
                #[cfg(target_os = "linux")]
                unsafe {
                    libc::malloc_trim(0);
                }
            }
            self.wake.notify_one();
        }
    }

    /// True when at least one device has an active LCD template to render.
    async fn has_active_content(&self) -> bool {
        self.app_state
            .devices
            .read()
            .await
            .iter()
            .any(|d| d.as_lcd().and_then(|l| l.lcd_template_id()).is_some())
    }

    /// Whether `device_id` currently has a live template slot in the engine.
    #[cfg(test)]
    pub(crate) async fn has_slot(&self, device_id: &str) -> bool {
        self.device_slots.lock().await.contains_key(device_id)
    }

    /// Remove a device from the engine (called when deactivating engine mode).
    pub async fn remove_device(&self, device_id: &str) {
        let mut slots = self.device_slots.lock().await;
        slots.remove(device_id);
        if slots.is_empty() {
            #[cfg(target_os = "linux")]
            unsafe {
                libc::malloc_trim(0);
            }
        }
    }

    /// Drop the editor session if it's been idle past the timeout.
    fn evict_idle_editor_session(&self) {
        if let Ok(mut session) = self.app_state.lcd.editor_session.try_lock() {
            if session
                .as_ref()
                .is_some_and(|s| s.is_idle(Instant::now(), custom::EDITOR_SESSION_IDLE_TIMEOUT))
            {
                *session = None;
                // Dropping the session only returns its buffers to the
                // allocator; ask glibc to hand free pages back to the OS so
                // the daemon's RSS actually shrinks after heavy editor use.
                #[cfg(target_os = "linux")]
                unsafe {
                    libc::malloc_trim(0);
                }
            }
        }
    }

    pub fn template_exists(id: &str) -> bool {
        id == "custom"
    }

    pub fn wire_state(
        device_templates: HashMap<String, String>,
        device_template_params: HashMap<String, HashMap<String, EffectParamValue>>,
    ) -> WireLcdEngineState {
        WireLcdEngineState {
            available_templates: vec![CustomTemplate::descriptor()],
            device_templates,
            device_template_params,
        }
    }

    /// Poll `evict_idle_editor_session` on its own timer, independent of the render loop.
    async fn run_editor_session_evictor(&self, interval: std::time::Duration) {
        let mut interval = tokio::time::interval(interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            self.evict_idle_editor_session();
        }
    }

    pub async fn start(
        self: Arc<Self>,
        cfg_rx: watch::Receiver<EngineRunConfig>,
    ) -> tokio::task::JoinHandle<()> {
        let evictor = Arc::clone(&self);
        tokio::spawn(async move {
            evictor
                .run_editor_session_evictor(EDITOR_SESSION_EVICT_INTERVAL)
                .await;
        });
        tokio::spawn(async move {
            let start = Instant::now();
            // Idle whenever no device has an active template; wake instantly when
            // one is set. Everything else (master switch, fps, config changes)
            // routes through the shared engine loop.
            let (self_wait, self_has) = (Arc::clone(&self), Arc::clone(&self));
            crate::run_loop::engine_run_loop_idle(
                "LCD",
                cfg_rx,
                tokio::time::MissedTickBehavior::Skip,
                move |_cfg| {
                    let this = Arc::clone(&self);
                    let t = start.elapsed().as_secs_f64();
                    async move { this.tick(t).await }
                },
                move || {
                    let this = Arc::clone(&self_wait);
                    async move { this.wake.notified().await }
                },
                move || {
                    let this = Arc::clone(&self_has);
                    async move { this.has_active_content().await }
                },
            )
            .await;
        })
    }

    async fn tick(&self, t: f64) {
        self.evict_idle_editor_session();
        let sensors = self.app_state.snapshot_sensors().await;

        let receiver_count = self.frame_tx.receiver_count();
        let prev_receiver_count = self
            .prev_receiver_count
            .swap(receiver_count, std::sync::atomic::Ordering::Relaxed);
        let preview_just_subscribed = receiver_count > 0 && prev_receiver_count == 0;

        let devices = self.app_state.devices.read().await.clone();

        let device_templates: HashMap<String, TemplateSpec> = devices
            .iter()
            .filter_map(|d| {
                let lcd = d.as_lcd()?;
                let template_id = lcd.lcd_template_id()?;
                Some((d.id().to_owned(), (template_id, lcd.lcd_template_params())))
            })
            .collect();

        let mut slots = self.device_slots.lock().await;

        // Add missing slots; replace slots whose template_id or params changed.
        let mut swapped_slot = false;
        for (device_id, (template_id, params)) in &device_templates {
            let needs_insert = match slots.get(device_id) {
                Some(slot) => slot.template_id != *template_id || slot.params != *params,
                None => true,
            };
            if needs_insert {
                if let Some(tmpl) = build_template(template_id, params) {
                    let existed = slots.contains_key(device_id.as_str());
                    let frame_id = slots
                        .get(device_id.as_str())
                        .map(|s| s.frame_id)
                        .unwrap_or(0);
                    slots.insert(
                        device_id.clone(),
                        DeviceSlot {
                            template_id: template_id.clone(),
                            params: params.clone(),
                            template: tmpl,
                            frame_id,
                            fail_streak: 0,
                            last_sig: None,
                            frame_buf: RgbaImage::new(0, 0),
                            png_buf: Vec::new(),
                            b64_buf: String::new(),
                        },
                    );
                    swapped_slot |= should_trim_on_swap(existed);
                }
            }
        }

        // Any key not in `device_templates` (built from `devices`) is offline or cleared.
        slots.retain(|id, _| device_templates.contains_key(id));

        // Return freed pages to the OS when every slot is gone (template
        // deactivated for all devices) or when a swap just dropped an old
        // template and its caches — otherwise the freed memory only reaches
        // glibc's free lists and RSS ratchets up across mode switches.
        if slots.is_empty() || swapped_slot {
            #[cfg(target_os = "linux")]
            unsafe {
                libc::malloc_trim(0);
            }
        }

        for (device_id, slot) in slots.iter_mut() {
            let Some(device) = devices.iter().find(|d| d.id() == *device_id) else {
                log::debug!("LCD engine: device {device_id} not found, skipping");
                continue;
            };
            let Some(lcd) = device.as_lcd() else {
                log::warn!("LCD engine: device {device_id} has no LCD capability");
                continue;
            };

            // Back off a device that keeps failing (disconnected but not yet
            // hotplug-removed) instead of rendering+pushing at full FPS.
            if slot.fail_streak >= FAIL_BACKOFF_THRESHOLD && slot.fail_streak % BACKOFF_TICKS != 0 {
                slot.fail_streak = slot.fail_streak.saturating_add(1);
                continue;
            }

            let descriptor = lcd.lcd_descriptor();
            let ctx = TemplateCtx {
                width: descriptor.width,
                height: descriptor.height,
                t,
                frame: slot.frame_id,
                sensors: &sensors,
            };

            let force = preview_just_subscribed || slot.last_sig.is_none() || slot.fail_streak > 0;
            let sig = slot.template.content_signature(&ctx);
            if !force && descriptor.latches_last_frame && sig.is_some() && sig == slot.last_sig {
                continue;
            }

            let tr = std::time::Instant::now();
            if let Err(e) = slot.template.render(&ctx, &mut slot.frame_buf) {
                log::warn!("LCD engine: template render error for {device_id}: {e}");
                continue;
            };
            slot.last_sig = sig;
            let (frame_w, frame_h) = slot.frame_buf.dimensions();
            log::trace!(
                "[LCD engine timing] template render: {}ms ({frame_w}x{frame_h})",
                tr.elapsed().as_millis()
            );

            slot.frame_id += 1;

            // Broadcast the preview before the (blocking) device push, at render rate.
            if receiver_count > 0 {
                if templates::encode_png_into(&slot.frame_buf, &mut slot.png_buf).is_ok() {
                    use base64::Engine as _;
                    slot.b64_buf.clear();
                    base64::engine::general_purpose::STANDARD
                        .encode_string(&slot.png_buf, &mut slot.b64_buf);
                    if let Some(frame) = encode_wire_frame(device_id, slot.frame_id, &slot.b64_buf)
                    {
                        let _ = self.frame_tx.send(frame);
                    }
                } else {
                    log::warn!("LCD engine: preview encode error for {device_id}");
                }
            }

            let ts = std::time::Instant::now();
            match lcd
                .stream_frame(slot.frame_buf.as_raw(), frame_w, frame_h)
                .await
            {
                Ok(()) => slot.fail_streak = 0,
                Err(e) => {
                    // Log only the first failure and once per backoff window so a
                    // dead device doesn't flood the log.
                    if slot.fail_streak % BACKOFF_TICKS == 0 {
                        log::warn!("LCD engine: stream_frame failed for {device_id}: {e}");
                    }
                    slot.fail_streak = slot.fail_streak.saturating_add(1);
                }
            }
            log::trace!(
                "[LCD engine timing] stream_frame: {}ms",
                ts.elapsed().as_millis()
            );
        }
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

    #[tokio::test]
    async fn has_active_content_tracks_template_id() {
        use crate::drivers::Device;
        use crate::test_support::MockDevice;

        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("lcd0").with_lcd());
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app);
        assert!(!engine.has_active_content().await);

        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("clock".into()));
        assert!(engine.has_active_content().await);

        dev.lcd.as_ref().unwrap().set_lcd_template_id(None);
        assert!(!engine.has_active_content().await);
    }

    #[tokio::test]
    async fn set_template_active_inserts_slot_and_preserves_frame_id() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = LcdEngine::new(app);

        engine
            .set_template_active("dev1", "custom", &HashMap::new())
            .await;
        assert!(
            engine.device_slots.lock().await.contains_key("dev1"),
            "slot must be inserted"
        );

        engine
            .device_slots
            .lock()
            .await
            .get_mut("dev1")
            .unwrap()
            .frame_id = 7;

        // Hot-swap with the same template — frame_id must be preserved.
        engine
            .set_template_active("dev1", "custom", &HashMap::new())
            .await;
        assert_eq!(
            engine.device_slots.lock().await["dev1"].frame_id,
            7,
            "frame_id must survive hot-swap"
        );
    }

    #[test]
    fn should_trim_on_swap_only_when_existing_slot_replaced() {
        assert!(
            !should_trim_on_swap(false),
            "first insert has nothing to reclaim"
        );
        assert!(
            should_trim_on_swap(true),
            "replacing a slot drops the old template"
        );
    }

    #[tokio::test]
    async fn set_template_active_swap_replaces_params() {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = LcdEngine::new(app);

        engine
            .set_template_active("dev1", "custom", &HashMap::new())
            .await;
        let params: HashMap<String, EffectParamValue> = HashMap::from([(
            halod_shared::lcd_custom::WIDGETS_JSON_PARAM.to_string(),
            EffectParamValue::Str("{}".into()),
        )]);
        engine.set_template_active("dev1", "custom", &params).await;

        let slots = engine.device_slots.lock().await;
        assert_eq!(slots["dev1"].params, params, "swap must install new params");
    }

    /// Install every domain engine so usecases that go through them (e.g.
    /// `load_active_profile`) reach `engine`.
    async fn install_engines(app: &Arc<AppState>, engine: &Arc<LcdEngine>) {
        use crate::state::EngineRunConfig;
        app.lighting.set_engine(
            crate::lighting::rgb_engine::RgbEngine::new(Arc::clone(app)).await,
            watch::channel(EngineRunConfig::canvas(&Default::default())).0,
        );
        app.cooling.set_engine(
            watch::channel(EngineRunConfig::fan_curve(&Default::default())).0,
            watch::channel(75).0,
        );
        app.lcd.set_engine(
            Arc::clone(engine),
            video::VideoEngine::new(Arc::clone(app), engine.frame_sender()),
            watch::channel(EngineRunConfig::lcd(&Default::default())).0,
        );
    }

    /// App with one LCD MockDevice whose default-profile lcd state is `state`.
    async fn app_with_lcd_state(
        state: serde_json::Value,
    ) -> (Arc<AppState>, Arc<crate::test_support::MockDevice>) {
        use crate::drivers::Device;
        use crate::test_support::MockDevice;
        let mut cfg = Config::default();
        cfg.profiles
            .get_mut(halod_shared::types::DEFAULT_PROFILE_NAME)
            .unwrap()
            .device_states
            .insert("lcd0".into(), serde_json::json!({ "lcd": state }));
        let app = Arc::new(AppState::new(cfg));
        let dev = Arc::new(MockDevice::new("lcd0").with_lcd());
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);
        (app, dev)
    }

    /// Restoring a profile's lcd_template_id must re-activate the engine slot.
    #[tokio::test]
    async fn load_active_profile_reactivates_restored_template_slot() {
        let (app, dev) = app_with_lcd_state(serde_json::json!({ "template_id": "custom" })).await;
        let engine = LcdEngine::new(Arc::clone(&app));
        install_engines(&app, &engine).await;

        crate::profiles::usecases::profiles::load_active_profile(Arc::clone(&app)).await;

        assert_eq!(
            dev.lcd.as_ref().unwrap().lcd_template_id().as_deref(),
            Some("custom"),
            "restore must put the template back on the device"
        );
        assert!(
            engine.device_slots.lock().await.contains_key("lcd0"),
            "profile load must re-activate the engine slot"
        );
    }

    /// The reverse switch: leaving a template profile for one without a
    /// template must drop the slot so the engine stops driving the panel.
    #[tokio::test]
    async fn load_active_profile_clears_slot_when_profile_has_no_template() {
        let (app, dev) = app_with_lcd_state(serde_json::json!({ "template_id": null })).await;
        let engine = LcdEngine::new(Arc::clone(&app));
        install_engines(&app, &engine).await;

        // Device currently engine-driven (previous profile).
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("custom".into()));
        engine
            .set_template_active("lcd0", "custom", &HashMap::new())
            .await;

        crate::profiles::usecases::profiles::load_active_profile(Arc::clone(&app)).await;

        assert!(
            dev.lcd.as_ref().unwrap().lcd_template_id().is_none(),
            "template must be cleared on the device"
        );
        assert!(
            !engine.device_slots.lock().await.contains_key("lcd0"),
            "slot must be removed when the new profile has no template"
        );
    }

    /// End-to-end regression for the parked loop: with the engine started and
    /// idle (no active content), a profile load alone must get frames flowing.
    #[tokio::test]
    async fn load_active_profile_wakes_a_parked_engine_loop() {
        use crate::state::EngineRunConfig;

        let (app, _dev) = app_with_lcd_state(serde_json::json!({ "template_id": "custom" })).await;
        let engine = LcdEngine::new(Arc::clone(&app));
        install_engines(&app, &engine).await;

        let mut frames = engine.subscribe();
        let (_cfg_tx, cfg_rx) =
            watch::channel(EngineRunConfig::lcd(&crate::config::LcdConfig::default()));
        let handle = Arc::clone(&engine).start(cfg_rx).await;
        // Let the engine park idle before the profile load.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        crate::profiles::usecases::profiles::load_active_profile(Arc::clone(&app)).await;

        let frame = tokio::time::timeout(std::time::Duration::from_secs(5), frames.recv())
            .await
            .expect("engine must wake and render after profile load")
            .expect("frame channel must stay open");
        assert!(!frame.wire.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn tick_removes_slot_when_device_clears_template() {
        use crate::drivers::Device;
        use crate::test_support::MockDevice;

        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("lcd1").with_lcd());
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("custom".into()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(Arc::clone(&app));

        // First tick: device has template → slot is created.
        engine.tick(0.0).await;
        assert!(
            engine.device_slots.lock().await.contains_key("lcd1"),
            "slot must exist after first tick"
        );

        dev.lcd.as_ref().unwrap().set_lcd_template_id(None);

        // Second tick: device has no template → slot is removed.
        engine.tick(0.0).await;
        assert!(
            !engine.device_slots.lock().await.contains_key("lcd1"),
            "slot must be removed when template is cleared"
        );
    }

    #[tokio::test]
    async fn fail_streak_backoff_limits_render_attempts() {
        use crate::drivers::Device;
        use crate::test_support::MockDevice;

        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("lcd2").with_lcd());
        // MockDevice.stream_frame returns Err by default (device does not support streaming).
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("custom".into()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app);

        // Run FAIL_BACKOFF_THRESHOLD + 1 ticks so the streak passes the threshold.
        let total_ticks = FAIL_BACKOFF_THRESHOLD + 1;
        for _ in 0..total_ticks {
            engine.tick(0.0).await;
        }

        // After the threshold is hit, frame_id increments only on actual render
        // attempts (stream_frame is called but slot.frame_id++ happens before it).
        // The first FAIL_BACKOFF_THRESHOLD ticks all attempt a render; the +1th tick
        // enters the backoff branch (fail_streak >= threshold && streak % BACKOFF_TICKS != 0)
        // and skips the render entirely.
        let frame_id = engine.device_slots.lock().await["lcd2"].frame_id;
        assert_eq!(
            frame_id, FAIL_BACKOFF_THRESHOLD as u64,
            "only FAIL_BACKOFF_THRESHOLD renders must fire before backoff kicks in"
        );
    }

    fn static_custom_params() -> HashMap<String, EffectParamValue> {
        use halod_shared::lcd_custom::{
            BgKind, CustomTemplateDef, ScreenStyle, WIDGETS_JSON_PARAM,
        };
        let def = CustomTemplateDef {
            widgets: vec![],
            style: ScreenStyle {
                background: BgKind::Solid,
                ..Default::default()
            },
        };
        let mut p = HashMap::new();
        p.insert(
            WIDGETS_JSON_PARAM.to_string(),
            EffectParamValue::Str(serde_json::to_string(&def).unwrap()),
        );
        p
    }

    #[tokio::test]
    async fn latching_device_skips_stream_when_signature_unchanged() {
        use crate::drivers::Device;
        use crate::test_support::MockDevice;

        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(
            MockDevice::new("lcd3")
                .with_lcd()
                .with_lcd_latches_last_frame()
                .with_lcd_stream_ok(),
        );
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("custom".into()));
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_params(static_custom_params());
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app);
        engine.tick(0.0).await;
        let after_first = engine.device_slots.lock().await["lcd3"].frame_id;
        engine.tick(1.0).await;
        let after_second = engine.device_slots.lock().await["lcd3"].frame_id;

        assert_eq!(
            after_first, after_second,
            "unchanged signature on a latching device must not re-render"
        );
    }

    #[tokio::test]
    async fn non_latching_device_renders_every_tick_regardless_of_signature() {
        use crate::drivers::Device;
        use crate::test_support::MockDevice;

        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("lcd4").with_lcd().with_lcd_stream_ok());
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("custom".into()));
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_params(static_custom_params());
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app);
        engine.tick(0.0).await;
        let after_first = engine.device_slots.lock().await["lcd4"].frame_id;
        engine.tick(1.0).await;
        let after_second = engine.device_slots.lock().await["lcd4"].frame_id;

        assert_eq!(
            after_second,
            after_first + 1,
            "a device that does not declare latches_last_frame must never be skipped"
        );
    }

    #[tokio::test]
    async fn preview_forced_on_the_tick_a_subscriber_appears() {
        use crate::drivers::Device;
        use crate::test_support::MockDevice;

        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("lcd5").with_lcd().with_lcd_stream_ok());
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("custom".into()));
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_params(static_custom_params());
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = LcdEngine::new(app);
        engine.tick(0.0).await;

        let mut rx = engine.subscribe();
        engine.tick(1.0).await;
        let frame = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("preview must be sent once a subscriber is present")
            .expect("channel must stay open");
        assert!(!frame.wire.is_empty());
    }

    /// Regression for the parked-engine leak: the render tick never runs while
    /// the engine has no active content, so eviction must not depend on it.
    #[tokio::test]
    async fn editor_session_evictor_runs_independent_of_the_render_tick() {
        let app = Arc::new(AppState::new(Config::default()));
        *app.lcd.editor_session() = Some(custom::EditorSession::new_idle_for_test("dev1"));
        let engine = LcdEngine::new(Arc::clone(&app));
        assert!(
            !engine.has_active_content().await,
            "engine must be idle/parked"
        );

        let handle = tokio::spawn({
            let engine = Arc::clone(&engine);
            async move {
                engine
                    .run_editor_session_evictor(std::time::Duration::from_millis(5))
                    .await;
            }
        });

        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                if app.lcd.editor_session().is_none() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("evictor must drop the idle session without any render tick running");

        handle.abort();
    }
}
