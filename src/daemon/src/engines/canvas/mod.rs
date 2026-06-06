mod effects;
mod sampler;
pub(crate) mod screen_capture;

use std::{collections::{HashMap, HashSet}, sync::Arc, time::Instant};

use base64::Engine as _;
use halod_protocol::types::{
    Animation, CanvasFrame, EffectParamValue, LedFrameEntry, RgbColor, RgbState,
};
use tiny_skia::Pixmap;
use tokio::sync::{watch, Mutex};

use crate::{
    config::{CanvasState, PlacedZone},
    drivers::Device,
    state::{AppState, EngineRunConfig},
};
use effects::FrameSource;
use sampler::Sampler;

const CANVAS_W: u32 = 400;
const CANVAS_H: u32 = 300;

type RgbDeviceMap = HashMap<String, Arc<dyn Device>>;
type FrameTx = tokio::sync::broadcast::Sender<Arc<CanvasFrame>>;

pub struct CanvasEngine {
    app_state: Arc<AppState>,
    current_effect: Mutex<Box<dyn FrameSource>>,
    frame_tx: FrameTx,
}

fn build_effect_from_canvas_state(canvas_state: &CanvasState) -> Box<dyn FrameSource> {
    canvas_state
        .active_effect
        .as_ref()
        // Never auto-start screen_sampler on boot — it requires explicit user action.
        .filter(|(id, _)| id != "screen_sampler")
        .and_then(|(id, params)| effects::build(id, params))
        .unwrap_or_else(effects::default_source)
}

impl CanvasEngine {

    pub async fn new(app_state: Arc<AppState>) -> Arc<Self> {
        let config = app_state.config.read().await;
        let canvas_state = config.active_profile_data().canvas_state.clone();
        drop(config);

        let initial_effect = build_effect_from_canvas_state(&canvas_state);

        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        Arc::new(Self {
            app_state,
            current_effect: Mutex::new(initial_effect),
            frame_tx,
        })
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<CanvasFrame>> {
        self.frame_tx.subscribe()
    }

    pub fn available_effect_descriptors() -> Vec<Animation> {
        effects::all_descriptors()
    }

    pub async fn set_effect(
        &self,
        effect_id: &str,
        params: &HashMap<String, EffectParamValue>,
    ) -> bool {
        let Some(source) = effects::build(effect_id, params) else {
            return false;
        };
        *self.current_effect.lock().await = source;
        log::info!("Active effect set to: {effect_id}");
        true
    }

    pub async fn start(
        self: Arc<Self>,
        cfg_rx: watch::Receiver<EngineRunConfig>,
    ) -> tokio::task::JoinHandle<()> {
        // Pre-seed the active effect key from config so the first tick does not
        // treat a persisted "screen_sampler" entry as a newly-changed effect.
        let initial_effect_key = {
            let config = self.app_state.config.read().await;
            config
                .active_profile_data()
                .canvas_state
                .active_effect
                .as_ref()
                .map(|(id, _)| id.clone())
        };

        tokio::spawn(async move {
            struct CanvasTickState {
                start: Instant,
                last: Instant,
                frame_id: u64,
                pixmap: Pixmap,
                active_effect_key: Option<String>,
            }
            let tick_state = Arc::new(tokio::sync::Mutex::new(CanvasTickState {
                start: Instant::now(),
                last: Instant::now(),
                frame_id: 0u64,
                pixmap: Pixmap::new(CANVAS_W, CANVAS_H).expect("failed to create pixmap"),
                active_effect_key: initial_effect_key,
            }));

            crate::engines::engine_run_loop(
                "Canvas",
                cfg_rx,
                tokio::time::MissedTickBehavior::Skip,
                |_cfg| {
                    let this = Arc::clone(&self);
                    let tick_state = Arc::clone(&tick_state);
                    async move {
                        let mut s = tick_state.lock().await;
                        let now = Instant::now();
                        let t = now.duration_since(s.start).as_secs_f32();
                        let dt = now.duration_since(s.last).as_secs_f32();
                        s.last = now;

                        let (canvas_state, devices) =
                            this.sync_tick_state(&mut s.active_effect_key).await;
                        this.reconcile_engine_mode(&canvas_state).await;
                        let sampler = Sampler::new(canvas_state.sample_radius);

                        this.tick_render(&mut s.pixmap, t, dt).await;

                        let mut join_set: tokio::task::JoinSet<Vec<LedFrameEntry>> =
                            tokio::task::JoinSet::new();
                        for zone in &canvas_state.placed_zones {
                            if let Some(dev) = devices.get(&zone.device_id) {
                                if let Some(rgb) = dev.as_rgb() {
                                    let descriptor = rgb.descriptor();
                                    if let Some(rgb_zone) =
                                        descriptor.zones.iter().find(|z| z.id == zone.zone_id)
                                    {
                                        let colors = sampler.sample_zone(&s.pixmap, zone, rgb_zone);
                                        join_set.spawn(Self::flush_zone_colors(
                                            Arc::clone(dev),
                                            zone.clone(),
                                            colors,
                                        ));
                                    }
                                }
                            } else {
                                log::warn!(
                                    "Canvas: device {} not found for zone {}",
                                    zone.device_id,
                                    zone.zone_id
                                );
                            }
                        }
                        let mut led_colors: Vec<LedFrameEntry> = Vec::new();
                        while let Some(res) = join_set.join_next().await {
                            if let Ok(entries) = res {
                                led_colors.extend(entries);
                            }
                        }

                        this.publish_frame(&sampler, &s.pixmap, s.frame_id, led_colors);
                        s.frame_id += 1;
                    }
                },
            )
            .await;
        })
    }

    /// Detect config-driven effect changes and return the current canvas state with its RGB devices.
    ///
    /// `placed_zones` are now collected from connected devices via `as_canvas_engine_state()`
    /// rather than from the global config. This means offline devices are silently skipped
    /// instead of generating "device not found" warnings.
    async fn sync_tick_state(
        &self,
        active_effect_key: &mut Option<String>,
    ) -> (CanvasState, RgbDeviceMap) {
        // Read effect config (active_effect + sample_radius) from the profile config.
        let (active_effect, sample_radius) = {
            let config = self.app_state.config.read().await;
            let cs = &config.active_profile_data().canvas_state;
            (cs.active_effect.clone(), cs.sample_radius)
        };

        let new_key = active_effect.as_ref().map(|(id, _)| id.clone());
        if new_key != *active_effect_key {
            *active_effect_key = new_key;
            if let Some((id, params)) = &active_effect {
                if let Some(source) = effects::build(id, params) {
                    *self.current_effect.lock().await = source;
                    log::info!("Effect reloaded from config: {id}");
                }
            }
        }

        // Collect placed zones from all connected devices.
        let devices_guard = self.app_state.devices.lock().await;
        let placed_zones: Vec<PlacedZone> = devices_guard
            .iter()
            .filter_map(|d| d.as_rgb())
            .flat_map(|s| s.canvas_zones())
            .collect();

        let devices: RgbDeviceMap = placed_zones
            .iter()
            .filter_map(|p| {
                devices_guard
                    .iter()
                    .find(|d| d.id() == p.device_id && d.as_rgb().is_some())
                    .cloned()
                    .map(|dev| (p.device_id.clone(), dev))
            })
            .collect();

        let canvas_state = CanvasState { placed_zones, active_effect, sample_radius };

        (canvas_state, devices)
    }

    /// Ensure every RGB device's persisted mode matches whether it has placed canvas zones.
    /// Devices with zones that aren't in Engine mode are promoted; devices with no zones
    /// that are stuck in Engine mode are reverted to Static (black).
    async fn reconcile_engine_mode(&self, canvas_state: &CanvasState) {
        let engine_ids: HashSet<String> = canvas_state
            .placed_zones
            .iter()
            .map(|z| z.device_id.clone())
            .collect();

        let devices = self.app_state.devices.lock().await.clone();
        for device in &devices {
            let Some(rgb) = device.as_rgb() else { continue };
            let in_engine = matches!(rgb.current_state(), Some(RgbState::Engine));
            let should_engine = engine_ids.contains(&device.id());

            if should_engine && !in_engine {
                let _ = rgb.apply(RgbState::Engine).await;
                crate::usecases::persist_device_state(&self.app_state, device.as_ref()).await;
            } else if !should_engine && in_engine {
                let _ = rgb
                    .apply(RgbState::Static { color: RgbColor { r: 0, g: 0, b: 0 } })
                    .await;
                crate::usecases::persist_device_state(&self.app_state, device.as_ref()).await;
            }
        }
    }

    async fn tick_render(&self, pixmap: &mut Pixmap, t: f32, dt: f32) {
        let mut effect = self.current_effect.lock().await;
        if effect.render(pixmap, t, dt).is_err() {
            log::warn!("[Canvas] effect render failed; frame may be stale");
        }
    }

    async fn flush_zone_colors(
        dev: Arc<dyn Device>,
        zone: PlacedZone,
        colors: Vec<RgbColor>,
    ) -> Vec<LedFrameEntry> {
        let Some(rgb) = dev.as_rgb() else {
            return Vec::new();
        };
        let descriptor = rgb.descriptor();
        let Some(rgb_zone) = descriptor.zones.iter().find(|z| z.id == zone.zone_id) else {
            return Vec::new();
        };

        // Apply the device's per-zone LED-content transform before output, so
        // the engine path honours it just like static / per-LED output.
        let transform = dev
            .as_rgb()
            .map(|s| s.transform_for(&zone.zone_id))
            .unwrap_or_default();
        let colors =
            halod_protocol::zone_transform::transform_colors(&colors, rgb_zone, &transform);

        if let Err(e) = rgb.write_frame(&zone.zone_id, &colors).await {
            log::warn!(
                "write_frame failed for {}/{}: {e}",
                zone.device_id,
                zone.zone_id
            );
        }

        rgb_zone
            .leds
            .iter()
            .zip(colors.iter())
            .map(|(led, color)| LedFrameEntry {
                device_id: zone.device_id.clone(),
                zone_id: zone.zone_id.clone(),
                led_id: led.id,
                color: *color,
            })
            .collect()
    }

    fn publish_frame(
        &self,
        sampler: &Sampler,
        pixmap: &Pixmap,
        frame_id: u64,
        led_colors: Vec<LedFrameEntry>,
    ) {
        let srgb = sampler.pixmap_to_srgb_rgba(pixmap);
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let frame = Arc::new(CanvasFrame {
            frame_id,
            timestamp_ms,
            canvas_srgb_b64: base64::engine::general_purpose::STANDARD.encode(&srgb),
            canvas_w: CANVAS_W,
            canvas_h: CANVAS_H,
            led_colors,
        });
        let _ = self.frame_tx.send(frame);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, RgbCapability, RgbStateSlot};
    use anyhow::Result;
    use async_trait::async_trait;
    use halod_protocol::types::{
        LedPosition, RgbDescriptor, RgbState, RgbStatus, RgbZone, ZoneTopology,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    // ── MockRgbDevice ─────────────────────────────────────────────────────────

    struct MockRgbDevice {
        device_id: String,
        descriptor: RgbDescriptor,
        fail_write: bool,
        write_count: AtomicUsize,
        rgb: RgbStateSlot,
        /// Last RGB state passed to `apply`, modelling the real device contract:
        /// recorded even when the hardware write fails.
        rgb_state: StdMutex<Option<RgbState>>,
        apply_count: AtomicUsize,
        /// When set, `apply(Static)` returns an error (still recording state).
        fail_static_apply: AtomicBool,
    }

    impl MockRgbDevice {
        fn new(device_id: &str, zone_id: &str, led_count: usize, fail_write: bool) -> Arc<Self> {
            let leds = (0..led_count as u32)
                .map(|i| LedPosition {
                    id: i,
                    x: i as f32 / led_count as f32,
                    y: 0.5,
                })
                .collect();
            Arc::new(Self {
                device_id: device_id.to_string(),
                descriptor: RgbDescriptor {
                    zones: vec![RgbZone {
                        id: zone_id.to_string(),
                        name: zone_id.to_string(),
                        topology: ZoneTopology::Linear,
                        leds,
                    }],
                    native_effects: vec![],
                },
                fail_write,
                write_count: AtomicUsize::new(0),
                rgb: RgbStateSlot::default(),
                rgb_state: StdMutex::new(None),
                apply_count: AtomicUsize::new(0),
                fail_static_apply: AtomicBool::new(false),
            })
        }

        /// Build a device already in `Engine` mode with no placed zones — the
        /// state the canvas mode reconciler must revert. `fail_revert` makes the
        /// `apply(Static)` hardware write fail (the state is still recorded).
        fn new_engine_mode(device_id: &str, zone_id: &str, fail_revert: bool) -> Arc<Self> {
            let dev = Self::new(device_id, zone_id, 3, false);
            *dev.rgb_state.lock().unwrap() = Some(RgbState::Engine);
            dev.fail_static_apply.store(fail_revert, Ordering::SeqCst);
            dev
        }

        /// Create a device and pre-populate its canvas slot with `zones`.
        fn new_with_zones(
            device_id: &str,
            zone_id: &str,
            led_count: usize,
            fail_write: bool,
            zones: Vec<PlacedZone>,
        ) -> Arc<Self> {
            let dev = Self::new(device_id, zone_id, led_count, fail_write);
            dev.rgb.set_canvas_zones(zones);
            dev
        }
    }

    #[async_trait]
    impl Device for MockRgbDevice {
        fn id(&self) -> String {
            self.device_id.clone()
        }
        fn name(&self) -> &str {
            "mock"
        }
        fn vendor(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> { vec![CapabilityRef::Rgb(self)] }
    }

    #[async_trait]
    impl RgbCapability for MockRgbDevice {
        fn descriptor(&self) -> &RgbDescriptor {
            &self.descriptor
        }
        fn rgb_state(&self) -> &RgbStateSlot {
            &self.rgb
        }
        async fn apply(&self, state: RgbState) -> Result<()> {
            self.apply_count.fetch_add(1, Ordering::SeqCst);
            let fail = matches!(state, RgbState::Static { .. })
                && self.fail_static_apply.load(Ordering::SeqCst);
            // Record the requested state regardless of the write outcome —
            // mirrors `LogitechDevice::apply`.
            *self.rgb_state.lock().unwrap() = Some(state);
            if fail {
                anyhow::bail!("simulated static apply error");
            }
            Ok(())
        }
        fn current_state(&self) -> Option<RgbState> {
            self.rgb_state.lock().unwrap().clone()
        }
        async fn write_frame(&self, _zone_id: &str, _colors: &[RgbColor]) -> Result<()> {
            self.write_count.fetch_add(1, Ordering::SeqCst);
            if self.fail_write {
                anyhow::bail!("simulated write error");
            }
            Ok(())
        }
        fn serialize_rgb(&self) -> RgbStatus {
            RgbStatus {
                descriptor: self.descriptor.clone(),
                state: None,
                zone_transforms: std::collections::HashMap::new(),
                chainable_channels: Vec::new(),
            }
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    fn make_zone(device_id: &str, zone_id: &str) -> PlacedZone {
        PlacedZone {
            device_id: device_id.to_string(),
            zone_id: zone_id.to_string(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            rotation: 0.0,
        }
    }

    fn solid_colors(n: usize) -> Vec<RgbColor> {
        vec![RgbColor { r: 255, g: 0, b: 0 }; n]
    }

    // ── set_effect ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_effect_returns_true_for_known_id() {
        let engine = CanvasEngine::new(make_app()).await;
        assert!(engine.set_effect("static_color", &HashMap::new()).await);
        assert!(engine.set_effect("breathing", &HashMap::new()).await);
    }

    #[tokio::test]
    async fn set_effect_returns_false_for_unknown_id() {
        let engine = CanvasEngine::new(make_app()).await;
        assert!(!engine.set_effect("not_an_effect", &HashMap::new()).await);
    }

    #[tokio::test]
    async fn set_effect_changes_render_output() {
        let engine = CanvasEngine::new(make_app()).await;
        let mut pixmap = Pixmap::new(CANVAS_W, CANVAS_H).unwrap();

        let red_params: HashMap<String, EffectParamValue> = [(
            "color".to_string(),
            EffectParamValue::Color(RgbColor { r: 255, g: 0, b: 0 }),
        )]
        .into_iter()
        .collect();
        let blue_params: HashMap<String, EffectParamValue> = [(
            "color".to_string(),
            EffectParamValue::Color(RgbColor { r: 0, g: 0, b: 255 }),
        )]
        .into_iter()
        .collect();

        engine.set_effect("static_color", &red_params).await;
        engine.tick_render(&mut pixmap, 0.0, 0.016).await;
        let red_data = pixmap.data().to_vec();

        engine.set_effect("static_color", &blue_params).await;
        engine.tick_render(&mut pixmap, 0.0, 0.016).await;
        let blue_data = pixmap.data().to_vec();

        assert_ne!(
            red_data, blue_data,
            "different colors must produce different pixmaps"
        );
    }

    // ── tick_render ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn tick_render_does_not_panic_at_edge_times() {
        let engine = CanvasEngine::new(make_app()).await;
        engine.set_effect("breathing", &HashMap::new()).await;
        let mut pixmap = Pixmap::new(CANVAS_W, CANVAS_H).unwrap();
        // t=0 → sin=0 (all black); t=0.5/speed → peak brightness
        engine.tick_render(&mut pixmap, 0.0, 0.016).await;
        engine.tick_render(&mut pixmap, 1.0, 0.016).await;
        engine.tick_render(&mut pixmap, 1000.0, 0.016).await;
    }

    // ── sync_tick_state ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn sync_tick_state_rebuilds_effect_on_profile_switch() {
        let app = make_app();
        let engine = CanvasEngine::new(app.clone()).await;
        let mut pixmap = Pixmap::new(CANVAS_W, CANVAS_H).unwrap();

        // Render with default effect (static red).
        engine.tick_render(&mut pixmap, 0.0, 0.016).await;
        let before = pixmap.data().to_vec();

        // Switch config to a green static color.
        {
            let mut cfg = app.config.write().await;
            cfg.active_profile_data_mut().canvas_state.active_effect = Some((
                "static_color".to_string(),
                [(
                    "color".to_string(),
                    EffectParamValue::Color(RgbColor { r: 0, g: 255, b: 0 }),
                )]
                .into_iter()
                .collect(),
            ));
        }

        // Trigger sync with a stale key so it detects the change.
        let mut active_effect_key: Option<String> = Some("old_effect_id".to_string());
        engine.sync_tick_state(&mut active_effect_key).await;

        engine.tick_render(&mut pixmap, 0.0, 0.016).await;
        let after = pixmap.data().to_vec();

        assert_ne!(before, after, "effect should have been rebuilt from config");
    }

    #[tokio::test]
    async fn sync_tick_state_collects_zones_from_device_slots() {
        let app = make_app();
        let zone = make_zone("dev0", "ring");
        let dev: Arc<dyn Device> = MockRgbDevice::new_with_zones(
            "dev0",
            "ring",
            3,
            false,
            vec![zone.clone()],
        );
        app.devices.lock().await.push(dev);

        let engine = CanvasEngine::new(app).await;
        let mut active_effect_key: Option<String> = None;
        let (canvas_state, rgb_devices) = engine.sync_tick_state(&mut active_effect_key).await;

        assert_eq!(canvas_state.placed_zones.len(), 1);
        assert_eq!(canvas_state.placed_zones[0].device_id, "dev0");
        assert_eq!(canvas_state.placed_zones[0].zone_id, "ring");
        assert!(rgb_devices.contains_key("dev0"), "device should be in RgbDeviceMap");
    }

    #[tokio::test]
    async fn sync_tick_state_excludes_offline_device_zones() {
        let app = make_app();
        // Device not added to app.devices — simulates offline device.
        let zone = make_zone("offline_dev", "ring");
        // Add a connected device whose slot references itself only.
        let online: Arc<dyn Device> =
            MockRgbDevice::new_with_zones("online_dev", "ring", 2, false, vec![zone.clone()]);
        // Deliberately add a zone for "offline_dev" into online_dev's slot to test filtering.
        // In practice a device slot only contains its own zones, but this exercises the path.
        app.devices.lock().await.push(online);

        let engine = CanvasEngine::new(app).await;
        let mut key = None;
        let (canvas_state, rgb_devices) = engine.sync_tick_state(&mut key).await;

        // The zone device_id is "offline_dev" which is not in app.devices.
        assert!(!rgb_devices.contains_key("offline_dev"), "offline device should not appear");
        // The zone itself is still collected (it came from online_dev's slot).
        assert_eq!(canvas_state.placed_zones.len(), 1);
    }

    // ── reconcile_engine_mode ─────────────────────────────────────────────────

    #[tokio::test]
    async fn reconcile_engine_mode_reverts_zoneless_engine_device() {
        let app = make_app();
        let dev = MockRgbDevice::new_engine_mode("dev0", "ring", false);
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let engine = CanvasEngine::new(app).await;
        let empty = CanvasState { placed_zones: vec![], active_effect: None, sample_radius: 3.0 };
        engine.reconcile_engine_mode(&empty).await;

        assert!(
            matches!(dev.current_state(), Some(RgbState::Static { .. })),
            "device with no zones must be reverted out of Engine mode",
        );
        assert_eq!(dev.apply_count.load(Ordering::SeqCst), 1);
    }

    // Regression: a device whose blackout SetEffect fails on revert must still
    // leave Engine mode. Before the fix, LogitechDevice::apply only recorded the
    // new state on a successful hardware write, so a failing revert left the
    // device stuck in Engine — and reconcile_engine_mode re-issued the failing
    // write on every tick (the canvas-removal loop).
    #[tokio::test]
    async fn reconcile_engine_mode_does_not_loop_when_revert_write_fails() {
        let app = make_app();
        let dev = MockRgbDevice::new_engine_mode("dev0", "ring", true /* fail revert */);
        app.devices.lock().await.push(dev.clone() as Arc<dyn Device>);

        let engine = CanvasEngine::new(app).await;
        let empty = CanvasState { placed_zones: vec![], active_effect: None, sample_radius: 3.0 };

        // First tick: revert attempted; hardware write fails but state is recorded.
        engine.reconcile_engine_mode(&empty).await;
        assert!(matches!(dev.current_state(), Some(RgbState::Static { .. })));
        // Second tick: device already out of Engine mode → no further apply.
        engine.reconcile_engine_mode(&empty).await;
        assert_eq!(
            dev.apply_count.load(Ordering::SeqCst),
            1,
            "reconcile must not re-apply every tick once the device left Engine mode",
        );
    }

    // ── flush_zone_colors ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn flush_zone_colors_returns_empty_when_zone_not_in_descriptor() {
        let dev = MockRgbDevice::new("dev0", "ring", 3, false);
        let zone = make_zone("dev0", "not_a_real_zone");
        let colors = solid_colors(3);

        let dev_dyn: Arc<dyn Device> = dev.clone();
        let entries = CanvasEngine::flush_zone_colors(dev_dyn, zone, colors).await;
        assert!(entries.is_empty());
        assert_eq!(dev.write_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn flush_zone_colors_still_returns_entries_when_write_fails() {
        let dev = MockRgbDevice::new("dev0", "ring", 3, true /* fail */);
        let zone = make_zone("dev0", "ring");
        let colors = solid_colors(3);

        let dev_dyn: Arc<dyn Device> = dev.clone();
        let entries = CanvasEngine::flush_zone_colors(dev_dyn, zone, colors).await;
        assert_eq!(
            entries.len(),
            3,
            "led entries returned even when write_frame errors"
        );
        assert_eq!(dev.write_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn flush_zone_colors_returns_correct_entries_on_success() {
        let dev = MockRgbDevice::new("dev0", "ring", 4, false);
        let zone = make_zone("dev0", "ring");
        let colors = solid_colors(4);

        let dev_dyn: Arc<dyn Device> = dev.clone();
        let entries = CanvasEngine::flush_zone_colors(dev_dyn, zone, colors).await;
        assert_eq!(entries.len(), 4);
        assert!(entries
            .iter()
            .all(|e| e.device_id == "dev0" && e.zone_id == "ring"));
        assert_eq!(
            entries.iter().map(|e| e.led_id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(dev.write_count.load(Ordering::SeqCst), 1);
    }

    // ── subscribe / publish_frame ─────────────────────────────────────────────

    #[tokio::test]
    async fn publish_frame_delivers_frame_to_subscriber() {
        let engine = CanvasEngine::new(make_app()).await;
        let mut rx = engine.subscribe();
        let sampler = Sampler::new(3.0);
        let pixmap = Pixmap::new(CANVAS_W, CANVAS_H).unwrap();

        engine.publish_frame(&sampler, &pixmap, 42, vec![]);

        let frame = rx
            .try_recv()
            .expect("frame should be available immediately");
        assert_eq!(frame.frame_id, 42);
        assert_eq!(frame.canvas_w, CANVAS_W);
        assert_eq!(frame.canvas_h, CANVAS_H);
    }
}
