// SPDX-License-Identifier: GPL-3.0-or-later
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
    time::Instant,
};

use base64::Engine as _;
use halod_shared::types::{
    Animation, CanvasFrame, EffectDef, LedFrameEntry, RgbColor, RgbState, RgbZone, ZoneTopology,
};
use halod_shared::zone_transform::ring_slice;
use std::time::Duration;
use tiny_skia::Pixmap;
use tokio::sync::{watch, Mutex};

use super::canvas::{self, FrameSource, Sampler};
use super::color::linear_to_led;
#[cfg(test)]
use super::color::LinearColor;
use super::direct::{self, DirectLedEffect};
use crate::{
    config::{CanvasState, PlacedZone},
    drivers::Device,
    state::{AppState, EngineRunConfig},
};

const CANVAS_W: u32 = 400;
const CANVAS_H: u32 = 300;
const LED_GAMMA: f32 = 2.2;

const DEFAULT_KEY: &str = "__default__";

const IDLE_GRACE: Duration = Duration::from_secs(20);
const IDLE_POLL_MS: u64 = 1000;

fn idle_interval_ms(idle_since: Option<Instant>, now: Instant, base_ms: u64) -> u64 {
    match idle_since {
        Some(since) if now.duration_since(since) >= IDLE_GRACE => IDLE_POLL_MS,
        _ => base_ms,
    }
}

type RgbDeviceMap = HashMap<String, Arc<dyn Device>>;
type FrameTx = tokio::sync::broadcast::Sender<Arc<CanvasFrame>>;
/// Per device: the transformed frames to write this tick, one entry per zone.
type PendingWrites = HashMap<String, (Arc<dyn Device>, Vec<(String, Vec<RgbColor>)>)>;
type DirectDevice = (
    Arc<dyn Device>,
    String,
    HashMap<String, halod_shared::types::EffectParamValue>,
);

struct LivePixmap {
    key: String,
    pixmap: Pixmap,
    runtime: PixmapRuntime,
}

enum PixmapRuntime {
    Native(Box<dyn FrameSource>),
    Plugin(crate::drivers::plugins::PluginEffectHandle),
    Off,
}

struct LiveDirect {
    key: String,
    runtime: DirectRuntime,
}

enum DirectRuntime {
    Native(Box<dyn DirectLedEffect>),
    Plugin(crate::drivers::plugins::PluginEffectHandle),
    Off,
}

pub struct RgbEngine {
    app_state: Arc<AppState>,
    live_pixmap: Mutex<HashMap<String, LivePixmap>>,
    live_direct: Mutex<HashMap<String, LiveDirect>>,
    frame_tx: FrameTx,
    engine_mode_intent: Mutex<HashMap<String, bool>>,
    /// Reusable srgb preview buffer, cleared and repopulated each tick.
    preview_srgb_buf: Mutex<Vec<u8>>,
    /// One in-flight write task per device. A device whose previous write is
    /// still running has its frame dropped this tick rather than queued, so a
    /// slow (e.g. rate-limited) device never paces the rest of the canvas.
    write_slots: std::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
}

fn params_key(params: &HashMap<String, halod_shared::types::EffectParamValue>) -> String {
    let sorted: std::collections::BTreeMap<_, _> = params.iter().collect();
    serde_json::to_string(&sorted).unwrap_or_default()
}

fn instance_key(def: Option<&EffectDef>) -> String {
    match def {
        Some(d) => format!("{}|{}", d.effect_id, params_key(&d.params)),
        None => DEFAULT_KEY.to_string(),
    }
}

fn resolve_instance(zone: &PlacedZone, cs: &CanvasState) -> (String, Option<EffectDef>) {
    let id = zone.effect.clone().or_else(|| cs.default_effect.clone());
    match id.and_then(|i| cs.effects.get(&i).cloned().map(|d| (i, d))) {
        Some((i, def)) => (i, Some(def)),
        None => (DEFAULT_KEY.to_string(), None),
    }
}

fn build_pixmap_effect(app: &Arc<AppState>, def: Option<&EffectDef>) -> PixmapRuntime {
    let Some(d) = def else {
        return PixmapRuntime::Off;
    };
    if let Some(fx) = canvas::build(&d.effect_id, &d.params) {
        return PixmapRuntime::Native(fx);
    }
    app.registry
        .build_pixmap_effect(app.secret_store.as_ref(), &d.effect_id, &d.params)
        .map(PixmapRuntime::Plugin)
        .unwrap_or(PixmapRuntime::Off)
}

/// Number of rings for per-ring motion, or 1 for non-ring / indivisible zones.
fn ring_count_for(zone: &RgbZone) -> usize {
    match zone.topology {
        ZoneTopology::Rings { count } if count > 0 => {
            let count = count as usize;
            if zone.leds.len().is_multiple_of(count) {
                count
            } else {
                1
            }
        }
        _ => 1,
    }
}

/// Per-LED chain/spatial coordinates for a zone — the `(p, p_ring, nx, ny)`
/// tuple every direct effect (native or plugin) computes its color from.
/// Shared by `direct_zone_colors` (native, calls `led_color` inline) and the
/// plugin path (batches these into one `led_colors` round-trip).
fn zone_led_coords(zone: &RgbZone) -> Vec<(f32, f32, f32, f32)> {
    let n = zone.leds.len();
    let last = n.saturating_sub(1).max(1) as f32;
    let ring_count = ring_count_for(zone);
    let per_ring = n / ring_count.max(1);
    let ring_last = per_ring.saturating_sub(1).max(1) as f32;
    zone.leds
        .iter()
        .enumerate()
        .map(|(i, led)| {
            let p = i as f32 / last;
            let p_ring = if ring_count > 1 {
                let (start, _) = ring_slice(n, ring_count, i / per_ring);
                (i - start) as f32 / ring_last
            } else {
                p
            };
            (p, p_ring, led.x, led.y)
        })
        .collect()
}

fn direct_zone_colors(effect: &dyn DirectLedEffect, zone: &RgbZone, t: f32) -> Vec<RgbColor> {
    zone_led_coords(zone)
        .into_iter()
        .map(|(p, p_ring, nx, ny)| {
            let c = effect.led_color(p, p_ring, nx, ny, t);
            RgbColor {
                r: linear_to_led(c.r, LED_GAMMA),
                g: linear_to_led(c.g, LED_GAMMA),
                b: linear_to_led(c.b, LED_GAMMA),
            }
        })
        .collect()
}

/// Apply the per-zone LED-content transform and build the preview entries for a
/// single zone. Returns the transformed colors to write plus the per-LED preview
/// entries, or `None` if the device has no RGB capability or lacks the zone.
fn prepare_zone(
    dev: &Arc<dyn Device>,
    zone_id: &str,
    colors: Vec<RgbColor>,
) -> Option<(Vec<RgbColor>, Vec<LedFrameEntry>)> {
    let rgb = dev.as_rgb()?;
    let rgb_zone = rgb.descriptor().zones.iter().find(|z| z.id == zone_id)?;
    let transform = rgb.transform_for(zone_id);
    let colors = halod_shared::zone_transform::transform_colors(&colors, rgb_zone, &transform);
    let entries = rgb_zone
        .leds
        .iter()
        .zip(colors.iter())
        .map(|(led, color)| LedFrameEntry {
            device_id: dev.id().to_string(),
            zone_id: zone_id.to_string(),
            led_id: led.id,
            color: *color,
        })
        .collect();
    Some((colors, entries))
}

fn black_srgb() -> &'static [u8] {
    static BLACK: LazyLock<Vec<u8>> = LazyLock::new(|| {
        let mut out = vec![0u8; (CANVAS_W * CANVAS_H * 4) as usize];
        for a in out.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        out
    });
    &BLACK
}

impl RgbEngine {
    pub async fn new(app_state: Arc<AppState>) -> Arc<Self> {
        let (frame_tx, _) = tokio::sync::broadcast::channel(2);
        Arc::new(Self {
            app_state,
            live_pixmap: Mutex::new(HashMap::new()),
            live_direct: Mutex::new(HashMap::new()),
            frame_tx,
            engine_mode_intent: Mutex::new(HashMap::new()),
            preview_srgb_buf: Mutex::new(Vec::new()),
            write_slots: std::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<CanvasFrame>> {
        self.frame_tx.subscribe()
    }

    /// Native + plugin-declared pixmap effects. Not memoized (unlike the
    /// pre-plugin `LazyLock`) since plugins can load/unload at runtime; the
    /// merge itself is a cheap `RwLock` read + small-struct clone.
    pub fn available_effect_descriptors(
        registry: &crate::drivers::plugins::Registry,
    ) -> Vec<Animation> {
        let mut v = canvas::all_descriptors();
        v.extend(registry.pixmap_effect_descriptors());
        v
    }

    /// Native + plugin-declared direct effects. See
    /// [`Self::available_effect_descriptors`] for why this isn't memoized.
    pub fn direct_effect_descriptors(
        registry: &crate::drivers::plugins::Registry,
    ) -> Vec<Animation> {
        let mut v = direct::direct_descriptors();
        v.extend(registry.direct_effect_descriptors());
        v
    }

    pub async fn start(
        self: Arc<Self>,
        cfg_rx: watch::Receiver<EngineRunConfig>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let engine_start = Instant::now();
            let mut last = Instant::now();
            let mut frame_id = 0u64;
            let mut cfg_rx = cfg_rx;
            let mut idle_since: Option<Instant> = None;

            log::info!("Starting RGB engine");
            loop {
                let cfg = cfg_rx.borrow_and_update().clone();
                if !cfg.enabled {
                    log::info!("[RGB] Engine disabled, waiting for re-enable");
                    if cfg_rx.changed().await.is_err() {
                        break;
                    }
                    continue;
                }
                let mut interval_ms = idle_interval_ms(idle_since, Instant::now(), cfg.tick_ms);
                let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            let now = Instant::now();
                            let t = now.duration_since(engine_start).as_secs_f32();
                            let dt = now.duration_since(last).as_secs_f32();
                            last = now;
                            let had_work = self.tick(t, dt, frame_id).await;
                            frame_id += 1;
                            if had_work {
                                idle_since = None;
                            } else if idle_since.is_none() {
                                idle_since = Some(now);
                            }

                            let want_ms = idle_interval_ms(idle_since, now, cfg.tick_ms);
                            if want_ms != interval_ms {
                                interval_ms = want_ms;
                                interval = tokio::time::interval(Duration::from_millis(interval_ms));
                                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                            }
                        }
                        _ = cfg_rx.changed() => { break; }
                    }
                }
            }
        })
    }

    async fn tick(&self, t: f32, dt: f32, frame_id: u64) -> bool {
        let (canvas_state, devices, direct_devices) = self.sync_tick_state().await;
        self.reconcile_engine_mode(&canvas_state).await;
        let sampler = Sampler::new(canvas_state.sample_radius);

        let had_work = !canvas_state.placed_zones.is_empty()
            || !direct_devices.is_empty()
            || self.frame_tx.receiver_count() > 0;

        let mut pending: PendingWrites = HashMap::new();
        let mut led_colors: Vec<LedFrameEntry> = Vec::new();

        let preview_srgb = self
            .canvas_pass(
                &canvas_state,
                &devices,
                &sampler,
                t,
                dt,
                &mut pending,
                &mut led_colors,
            )
            .await;
        self.direct_pass(&direct_devices, t, dt, &mut pending, &mut led_colors)
            .await;

        self.dispatch_writes(pending);

        let srgb: &[u8] = match &preview_srgb {
            Some(v) => v,
            None => {
                // No default effect active — drop the reusable buffer to
                // release its 480 KiB allocation back to the allocator.
                if let Ok(mut buf) = self.preview_srgb_buf.try_lock() {
                    if buf.capacity() > 0 {
                        *buf = Vec::new();
                    }
                }
                black_srgb()
            }
        };
        self.publish_frame(srgb, frame_id, led_colors);
        had_work
    }

    #[allow(clippy::too_many_arguments)] // hot-path buffers are borrowed separately to avoid allocation
    async fn canvas_pass(
        &self,
        canvas_state: &CanvasState,
        devices: &RgbDeviceMap,
        sampler: &Sampler,
        t: f32,
        dt: f32,
        pending: &mut PendingWrites,
        led_colors: &mut Vec<LedFrameEntry>,
    ) -> Option<Vec<u8>> {
        let mut groups: HashMap<String, Vec<PlacedZone>> = HashMap::new();
        let mut defs: HashMap<String, Option<EffectDef>> = HashMap::new();
        for zone in &canvas_state.placed_zones {
            let (key, def) = resolve_instance(zone, canvas_state);
            defs.entry(key.clone()).or_insert(def);
            groups.entry(key).or_default().push(zone.clone());
        }
        // Always render the default instance even if no zone is assigned to it,
        // so the preview always shows the user-chosen default, not a HashMap-order guess.
        if let Some(id) = &canvas_state.default_effect {
            if let Some(def) = canvas_state.effects.get(id) {
                defs.entry(id.clone()).or_insert_with(|| Some(def.clone()));
            }
        }

        let pixmap_keys: Vec<String> = defs.keys().cloned().collect();

        let mut live = self.live_pixmap.lock().await;
        live.retain(|k, _| pixmap_keys.contains(k));
        let mut built: Vec<String> = Vec::with_capacity(pixmap_keys.len());
        for key in &pixmap_keys {
            let def = &defs[key];
            let want = instance_key(def.as_ref());
            let stale = live.get(key).map(|lp| lp.key != want).unwrap_or(true);
            if stale {
                let Some(pixmap) = Pixmap::new(CANVAS_W, CANVAS_H) else {
                    log::error!("canvas: failed to allocate pixmap for '{key}', skipping");
                    continue;
                };
                let runtime = build_pixmap_effect(&self.app_state, def.as_ref());
                live.insert(
                    key.clone(),
                    LivePixmap {
                        key: want,
                        pixmap,
                        runtime,
                    },
                );
            }
            built.push(key.clone());
        }
        for key in &built {
            let lp = live.get_mut(key).expect("instance built above");
            let disable = match &mut lp.runtime {
                PixmapRuntime::Plugin(handle) => match handle.render_pixmap(t, dt).await {
                    Ok(bytes) if bytes.len() == lp.pixmap.data().len() => {
                        lp.pixmap.data_mut().copy_from_slice(&bytes);
                        false
                    }
                    Ok(bytes) => {
                        log::warn!(
                            "plugin pixmap effect '{key}' returned {} bytes, expected {}; disabling for this session",
                            bytes.len(),
                            lp.pixmap.data().len()
                        );
                        true
                    }
                    Err(e) => {
                        log::warn!(
                            "plugin pixmap effect '{key}' render failed: {e:#}; disabling for this session"
                        );
                        true
                    }
                },
                PixmapRuntime::Native(effect) => {
                    effect.render(&mut lp.pixmap, t, dt);
                    false
                }
                PixmapRuntime::Off => {
                    lp.pixmap.fill(tiny_skia::Color::TRANSPARENT);
                    false
                }
            };
            if disable {
                lp.runtime = PixmapRuntime::Off;
                lp.pixmap.fill(tiny_skia::Color::TRANSPARENT);
            }
        }

        for (key, zones) in &groups {
            for zone in zones {
                let Some(dev) = devices.get(&zone.device_id) else {
                    continue;
                };
                let Some(rgb) = dev.as_rgb() else { continue };
                let Some(rgb_zone) = rgb.descriptor().zones.iter().find(|z| z.id == zone.zone_id)
                else {
                    continue;
                };
                if let Some(lp) = live.get(key) {
                    let colors = sampler.sample_zone(&lp.pixmap, zone, rgb_zone);
                    if let Some((colors, entries)) = prepare_zone(dev, &zone.zone_id, colors) {
                        led_colors.extend(entries);
                        pending
                            .entry(zone.device_id.clone())
                            .or_insert_with(|| (Arc::clone(dev), Vec::new()))
                            .1
                            .push((zone.zone_id.clone(), colors));
                    }
                }
            }
        }

        canvas_state
            .default_effect
            .as_ref()
            .and_then(|id| live.get(id))
            .map(|lp| {
                let mut buf = self
                    .preview_srgb_buf
                    .try_lock()
                    .expect("single-threaded engine");
                sampler.pixmap_to_srgb_rgba(&lp.pixmap, &mut buf);
                std::mem::take(&mut *buf)
            })
    }

    async fn direct_pass(
        &self,
        direct_devices: &[DirectDevice],
        t: f32,
        dt: f32,
        pending: &mut PendingWrites,
        led_colors: &mut Vec<LedFrameEntry>,
    ) {
        let mut live = self.live_direct.lock().await;
        let active: HashSet<&str> = direct_devices.iter().map(|(d, _, _)| d.id()).collect();
        live.retain(|k, _| active.contains(k.as_str()));

        // Snapshotted lazily: most direct effects don't consume a sensor, and a
        // snapshot walks every device's `SensorCapability`/`FanCapability`.
        let mut sensors: Option<HashMap<String, halod_shared::types::Sensor>> = None;

        for (dev, id, params) in direct_devices {
            let want = format!("{id}|{}", params_key(params));
            let stale = live.get(dev.id()).map(|ld| ld.key != want).unwrap_or(true);
            if stale {
                let runtime = match direct::build_direct(id, params) {
                    Some(fx) => DirectRuntime::Native(fx),
                    None => match self.app_state.registry.build_direct_effect(
                        self.app_state.secret_store.as_ref(),
                        id,
                        params,
                    ) {
                        Some(handle) => DirectRuntime::Plugin(handle),
                        None => {
                            log::warn!("Unknown direct effect id '{id}' on {}; leds off", dev.id());
                            DirectRuntime::Off
                        }
                    },
                };
                live.insert(dev.id().to_string(), LiveDirect { key: want, runtime });
            }
            let ld = live.get_mut(dev.id()).expect("built above");
            let Some(rgb) = dev.as_rgb() else { continue };

            if let DirectRuntime::Plugin(handle) = &ld.runtime {
                let handle = handle.clone();
                let sensor_value = match params.get("sensor") {
                    Some(halod_shared::types::EffectParamValue::Str(sensor_id))
                        if !sensor_id.is_empty() =>
                    {
                        if sensors.is_none() {
                            sensors = Some(self.app_state.snapshot_sensors().await);
                        }
                        sensors
                            .as_ref()
                            .and_then(|m| m.get(sensor_id))
                            .map(|s| s.value)
                    }
                    _ => None,
                };
                for rgb_zone in &rgb.descriptor().zones {
                    let coords = zone_led_coords(rgb_zone);
                    let leds: Vec<crate::drivers::plugins::LedCoord> = coords
                        .iter()
                        .map(|&(p, p_ring, nx, ny)| crate::drivers::plugins::LedCoord {
                            p,
                            p_ring,
                            nx,
                            ny,
                        })
                        .collect();
                    let colors = match handle.led_colors(leds, t, dt, sensor_value).await {
                        Ok(out) if out.len() == coords.len() => out
                            .into_iter()
                            .map(|c| RgbColor {
                                r: linear_to_led(c.r, LED_GAMMA),
                                g: linear_to_led(c.g, LED_GAMMA),
                                b: linear_to_led(c.b, LED_GAMMA),
                            })
                            .collect(),
                        Ok(out) => {
                            log::warn!(
                                "plugin direct effect '{id}' returned {} colors for {} LEDs on {}; disabling for this session",
                                out.len(),
                                coords.len(),
                                dev.id()
                            );
                            ld.runtime = DirectRuntime::Off;
                            continue;
                        }
                        Err(e) => {
                            log::warn!(
                                "plugin direct effect '{id}' failed on {}: {e:#}; disabling for this session",
                                dev.id()
                            );
                            ld.runtime = DirectRuntime::Off;
                            continue;
                        }
                    };
                    if let Some((colors, entries)) = prepare_zone(dev, &rgb_zone.id, colors) {
                        led_colors.extend(entries);
                        pending
                            .entry(dev.id().to_string())
                            .or_insert_with(|| (Arc::clone(dev), Vec::new()))
                            .1
                            .push((rgb_zone.id.clone(), colors));
                    }
                }
                continue;
            }

            let DirectRuntime::Native(effect) = &mut ld.runtime else {
                for rgb_zone in &rgb.descriptor().zones {
                    let colors = vec![RgbColor { r: 0, g: 0, b: 0 }; rgb_zone.leds.len()];
                    if let Some((colors, entries)) = prepare_zone(dev, &rgb_zone.id, colors) {
                        led_colors.extend(entries);
                        pending
                            .entry(dev.id().to_string())
                            .or_insert_with(|| (Arc::clone(dev), Vec::new()))
                            .1
                            .push((rgb_zone.id.clone(), colors));
                    }
                }
                continue;
            };
            if let Some(sensor_id) = effect.sensor_id().map(|s| s.to_string()) {
                if sensors.is_none() {
                    sensors = Some(self.app_state.snapshot_sensors().await);
                }
                let value = sensors
                    .as_ref()
                    .and_then(|m| m.get(&sensor_id))
                    .map(|s| s.value);
                effect.set_sensor_value(value);
            }
            effect.tick(t, dt);
            for rgb_zone in &rgb.descriptor().zones {
                let colors = direct_zone_colors(effect.as_ref(), rgb_zone, t);
                if let Some((colors, entries)) = prepare_zone(dev, &rgb_zone.id, colors) {
                    led_colors.extend(entries);
                    pending
                        .entry(dev.id().to_string())
                        .or_insert_with(|| (Arc::clone(dev), Vec::new()))
                        .1
                        .push((rgb_zone.id.clone(), colors));
                }
            }
        }
    }

    async fn sync_tick_state(&self) -> (CanvasState, RgbDeviceMap, Vec<DirectDevice>) {
        let (sample_radius, effects, default_effect) = {
            let config = self.app_state.config.read().await;
            let cs = config.effective_canvas_state_ref();
            (
                cs.sample_radius,
                cs.effects.clone(),
                cs.default_effect.clone(),
            )
        };

        let devices_guard = self.app_state.devices.read().await;
        // Skip offline devices so the engine never queues a frame for a dead socket.
        let placed_zones: Vec<PlacedZone> = devices_guard
            .iter()
            .filter(|d| d.is_live())
            .filter_map(|d| d.as_rgb())
            .flat_map(|s| s.canvas_zones())
            .collect();

        let devices: RgbDeviceMap = placed_zones
            .iter()
            .filter_map(|p| {
                devices_guard
                    .iter()
                    .find(|d| d.id() == p.device_id && d.is_live() && d.as_rgb().is_some())
                    .cloned()
                    .map(|dev| (p.device_id.clone(), dev))
            })
            .collect();

        let direct_devices: Vec<DirectDevice> = devices_guard
            .iter()
            .filter(|d| d.is_live())
            .filter_map(|d| match d.as_rgb()?.current_state() {
                Some(RgbState::DirectEffect { id, params }) => Some((Arc::clone(d), id, params)),
                _ => None,
            })
            .collect();

        let canvas_state = CanvasState {
            effects,
            default_effect,
            placed_zones,
            sample_radius,
            ..Default::default()
        };
        (canvas_state, devices, direct_devices)
    }

    // Acts only when a device's desired mode differs from the intent last acted
    // on, so a write that fails every tick (e.g. an unplugged device) is retried
    // once, not forever.
    async fn reconcile_engine_mode(&self, canvas_state: &CanvasState) {
        let engine_ids: HashSet<String> = canvas_state
            .placed_zones
            .iter()
            .map(|z| z.device_id.clone())
            .collect();

        let devices = self.app_state.devices.read().await.clone();
        let mut intent = self.engine_mode_intent.lock().await;
        for device in &devices {
            let Some(rgb) = device.as_rgb() else { continue };
            let should_engine = engine_ids.contains(device.id());

            let baseline = intent
                .get(device.id())
                .copied()
                .unwrap_or_else(|| matches!(rgb.current_state(), Some(RgbState::Engine)));

            if should_engine != baseline {
                let target = if should_engine {
                    Some(RgbState::Engine)
                } else if matches!(rgb.current_state(), Some(RgbState::Engine)) {
                    Some(RgbState::Static {
                        color: RgbColor { r: 0, g: 0, b: 0 },
                    })
                } else {
                    None
                };
                if let Some(target) = target {
                    if let Err(e) = rgb.apply(target).await {
                        log::warn!(
                            "reconcile_engine_mode: set {} to {} failed: {e}",
                            device.id(),
                            if should_engine { "Engine" } else { "Static" }
                        );
                    }
                    self.app_state.persistence.notify.notify_one();
                }
            }

            intent.insert(device.id().to_string(), should_engine);
        }
    }

    /// Dispatch this tick's frames, one detached task per device. A device
    /// whose previous write is still in flight has its frame dropped rather
    /// than queued, so awaiting a slow (e.g. rate-limited) device never paces
    /// the rest of the canvas — the freshest frame always wins.
    fn dispatch_writes(&self, pending: PendingWrites) {
        let mut slots = self.write_slots.lock().expect("write slots poisoned");
        for (id, (dev, zones)) in pending {
            // Skip a device that went offline between state sync and dispatch.
            if !dev.is_live() {
                continue;
            }
            if slots.get(&id).is_some_and(|h| !h.is_finished()) {
                continue;
            }
            let handle = tokio::spawn(async move {
                let Some(rgb) = dev.as_rgb() else { return };
                for (zone_id, colors) in zones {
                    if let Err(e) = rgb.write_frame(&zone_id, &colors).await {
                        log::warn!("write_frame failed for {}/{}: {e}", dev.id(), zone_id);
                    }
                }
            });
            slots.insert(id, handle);
        }
    }

    #[cfg(test)]
    async fn drain_writes(&self) {
        let handles: Vec<_> = {
            let mut slots = self.write_slots.lock().expect("write slots poisoned");
            slots.drain().map(|(_, h)| h).collect()
        };
        for h in handles {
            let _ = h.await;
        }
    }

    fn publish_frame(&self, canvas_srgb: &[u8], frame_id: u64, led_colors: Vec<LedFrameEntry>) {
        if self.frame_tx.receiver_count() == 0 {
            return;
        }
        let frame = Arc::new(CanvasFrame {
            frame_id,
            timestamp_ms: crate::util::time::now_ms(),
            canvas_srgb_b64: base64::engine::general_purpose::STANDARD.encode(canvas_srgb),
            canvas_w: CANVAS_W,
            canvas_h: CANVAS_H,
            led_colors,
        });
        let _ = self.frame_tx.send(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{CapabilityRef, RgbCapability, RgbStateSlot};
    use anyhow::Result;
    use async_trait::async_trait;
    use halod_shared::types::{
        EffectParamValue, LedPosition, RgbDescriptor, RgbStatus, RgbZone, ZoneTopology,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    struct MockRgbDevice {
        device_id: String,
        descriptor: RgbDescriptor,
        fail_write: bool,
        write_count: AtomicUsize,
        last_frame: StdMutex<Vec<RgbColor>>,
        rgb: RgbStateSlot,
        rgb_state: StdMutex<Option<RgbState>>,
        apply_count: AtomicUsize,
        fail_static_apply: AtomicBool,
        skip_record_on_fail: AtomicBool,
        last_colors: StdMutex<Option<Vec<RgbColor>>>,
        live: AtomicBool,
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
                last_frame: StdMutex::new(Vec::new()),
                rgb: RgbStateSlot::default(),
                rgb_state: StdMutex::new(None),
                apply_count: AtomicUsize::new(0),
                fail_static_apply: AtomicBool::new(false),
                skip_record_on_fail: AtomicBool::new(false),
                last_colors: StdMutex::new(None),
                live: AtomicBool::new(true),
            })
        }

        fn new_engine_mode(device_id: &str, zone_id: &str, fail_revert: bool) -> Arc<Self> {
            let dev = Self::new(device_id, zone_id, 3, false);
            *dev.rgb_state.lock().unwrap() = Some(RgbState::Engine);
            dev.fail_static_apply.store(fail_revert, Ordering::SeqCst);
            dev
        }

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
        fn id(&self) -> &str {
            &self.device_id
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
        fn is_live(&self) -> bool {
            self.live.load(Ordering::SeqCst)
        }
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![CapabilityRef::Rgb(self)]
        }
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
            if !(fail && self.skip_record_on_fail.load(Ordering::SeqCst)) {
                *self.rgb_state.lock().unwrap() = Some(state);
            }
            if fail {
                anyhow::bail!("simulated static apply error");
            }
            Ok(())
        }
        fn current_state(&self) -> Option<RgbState> {
            self.rgb_state.lock().unwrap().clone()
        }
        async fn write_frame(&self, _zone_id: &str, colors: &[RgbColor]) -> Result<()> {
            self.write_count.fetch_add(1, Ordering::SeqCst);
            *self.last_colors.lock().unwrap() = Some(colors.to_vec());
            *self.last_frame.lock().unwrap() = colors.to_vec();
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
            effect: None,
            sampling_mode: Default::default(),
        }
    }

    fn solid_colors(n: usize) -> Vec<RgbColor> {
        vec![RgbColor { r: 255, g: 0, b: 0 }; n]
    }

    #[test]
    fn resolve_uses_zone_effect_then_default_then_fallback() {
        let mut cs = CanvasState::default();
        cs.effects.insert(
            "bars".into(),
            EffectDef {
                effect_id: "screen_sampler".into(),
                name: None,
                params: Default::default(),
            },
        );
        cs.default_effect = Some("bars".into());

        let mut z = make_zone("d", "z");
        z.effect = Some("bars".into());
        assert_eq!(resolve_instance(&z, &cs).0, "bars");

        z.effect = None;
        assert_eq!(resolve_instance(&z, &cs).0, "bars");

        cs.default_effect = None;
        assert_eq!(resolve_instance(&z, &cs).0, DEFAULT_KEY);

        z.effect = Some("missing".into());
        assert_eq!(resolve_instance(&z, &cs).0, DEFAULT_KEY);
    }

    #[test]
    fn instance_key_changes_with_params() {
        let a = EffectDef {
            effect_id: "static_color".into(),
            name: None,
            params: [("color".into(), EffectParamValue::Float(1.0))]
                .into_iter()
                .collect(),
        };
        let b = EffectDef {
            effect_id: "static_color".into(),
            name: None,
            params: [("color".into(), EffectParamValue::Float(2.0))]
                .into_iter()
                .collect(),
        };
        assert_ne!(instance_key(Some(&a)), instance_key(Some(&b)));
        assert_eq!(instance_key(None), DEFAULT_KEY);
    }

    #[test]
    fn params_key_is_order_independent() {
        let mut a = HashMap::new();
        a.insert("zzz".to_string(), EffectParamValue::Float(1.0));
        a.insert("aaa".to_string(), EffectParamValue::Float(2.0));
        a.insert("mmm".to_string(), EffectParamValue::Float(3.0));
        let mut b = HashMap::new();
        b.insert("aaa".to_string(), EffectParamValue::Float(2.0));
        b.insert("mmm".to_string(), EffectParamValue::Float(3.0));
        b.insert("zzz".to_string(), EffectParamValue::Float(1.0));
        assert_eq!(params_key(&a), params_key(&b));
    }

    /// Position-independent pulse `sin(t*0.5*pi)^2` (black at t=0, peak at
    /// t=1), brightness-scaling a fixed color — a minimal `DirectLedEffect`
    /// fixture for exercising `direct_zone_colors`'s coordinate/gamma math
    /// (breathing itself is now a plugin effect, not native; see
    /// `halo_effects.lua`).
    struct PulseTestEffect;
    impl DirectLedEffect for PulseTestEffect {
        fn led_color(&self, _p: f32, _p_ring: f32, _nx: f32, _ny: f32, t: f32) -> LinearColor {
            let brightness = (t * 0.5 * std::f32::consts::PI).sin().powi(2);
            LinearColor {
                r: 0.0,
                g: brightness,
                b: brightness,
            }
        }
    }

    #[test]
    fn direct_breathing_is_black_at_phase_zero() {
        let fx: Box<dyn DirectLedEffect> = Box::new(PulseTestEffect);
        let zone = RgbZone {
            id: "z".into(),
            name: "z".into(),
            topology: ZoneTopology::Linear,
            leds: vec![
                LedPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.5,
                },
                LedPosition {
                    id: 1,
                    x: 1.0,
                    y: 0.5,
                },
            ],
        };
        let colors = direct_zone_colors(fx.as_ref(), &zone, 0.0);
        assert!(colors.iter().all(|c| *c == RgbColor { r: 0, g: 0, b: 0 }));
    }

    #[test]
    fn direct_breathing_peak_is_uniform_and_lit() {
        let fx: Box<dyn DirectLedEffect> = Box::new(PulseTestEffect);
        let zone = RgbZone {
            id: "z".into(),
            name: "z".into(),
            topology: ZoneTopology::Linear,
            leds: vec![
                LedPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.5,
                },
                LedPosition {
                    id: 1,
                    x: 0.5,
                    y: 0.5,
                },
            ],
        };
        let colors = direct_zone_colors(fx.as_ref(), &zone, 1.0);
        assert_eq!(colors[0], colors[1], "breathing is position-independent");
        assert_ne!(colors[0], RgbColor { r: 0, g: 0, b: 0 });
    }

    #[test]
    fn direct_zone_colors_passes_chain_index_not_spatial_x_as_p() {
        let mut params = HashMap::new();
        params.insert(
            "generator".to_string(),
            EffectParamValue::Str("sawtooth".to_string()),
        );
        params.insert("speed".to_string(), EffectParamValue::Float(0.0));
        params.insert("sharpness".to_string(), EffectParamValue::Float(0.0));
        params.insert("floor".to_string(), EffectParamValue::Float(0.0));
        params.insert(
            "color_mode".to_string(),
            EffectParamValue::Str("solid".to_string()),
        );
        let fx = direct::build_direct(halod_shared::effect_designer::DESIGNER_EFFECT_ID, &params)
            .unwrap();
        let zone = RgbZone {
            id: "ring".into(),
            name: "ring".into(),
            topology: ZoneTopology::Ring,
            leds: vec![
                LedPosition {
                    id: 0,
                    x: 0.5,
                    y: 0.0,
                },
                LedPosition {
                    id: 1,
                    x: 0.5,
                    y: 1.0,
                },
            ],
        };
        let colors = direct_zone_colors(fx.as_ref(), &zone, 0.0);
        assert_ne!(
            colors[0], colors[1],
            "LEDs at the same spatial x but different chain positions must differ"
        );
    }

    #[test]
    fn direct_zone_colors_ring_scope_selects_per_ring_or_whole_zone_motion() {
        let zone = RgbZone {
            id: "triple".into(),
            name: "triple".into(),
            topology: ZoneTopology::Rings { count: 3 },
            leds: (0..6)
                .map(|id| LedPosition { id, x: 0.0, y: 0.0 })
                .collect(),
        };
        let params = |ring_scope: &str| {
            let mut m = HashMap::new();
            m.insert(
                "generator".to_string(),
                EffectParamValue::Str("sawtooth".to_string()),
            );
            m.insert("speed".to_string(), EffectParamValue::Float(0.0));
            m.insert("sharpness".to_string(), EffectParamValue::Float(0.0));
            m.insert("floor".to_string(), EffectParamValue::Float(0.0));
            m.insert(
                "color_mode".to_string(),
                EffectParamValue::Str("solid".to_string()),
            );
            m.insert(
                "ring_scope".to_string(),
                EffectParamValue::Str(ring_scope.to_string()),
            );
            m
        };

        let per_ring = direct::build_direct(
            halod_shared::effect_designer::DESIGNER_EFFECT_ID,
            &params("per_ring"),
        )
        .unwrap();
        let colors = direct_zone_colors(per_ring.as_ref(), &zone, 0.0);
        assert_eq!(
            colors[0], colors[2],
            "per_ring scope must restart the sweep at the first LED of every ring"
        );
        assert_eq!(colors[2], colors[4]);

        let whole_zone = direct::build_direct(
            halod_shared::effect_designer::DESIGNER_EFFECT_ID,
            &params("zone"),
        )
        .unwrap();
        let colors = direct_zone_colors(whole_zone.as_ref(), &zone, 0.0);
        assert_ne!(
            colors[0], colors[2],
            "zone scope sweeps once nose-to-tail across every ring"
        );
    }

    #[tokio::test]
    async fn sync_tick_state_collects_zones_from_device_slots() {
        let app = make_app();
        let zone = make_zone("dev0", "ring");
        let dev: Arc<dyn Device> =
            MockRgbDevice::new_with_zones("dev0", "ring", 3, false, vec![zone.clone()]);
        app.devices.write().await.push(dev);

        let engine = RgbEngine::new(app).await;
        let (canvas_state, rgb_devices, direct) = engine.sync_tick_state().await;

        assert_eq!(canvas_state.placed_zones.len(), 1);
        assert!(rgb_devices.contains_key("dev0"));
        assert!(direct.is_empty());
    }

    #[tokio::test]
    async fn sync_tick_state_excludes_offline_devices() {
        // An offline device (integration whose server dropped) must be dropped
        // from both the canvas and direct-effect sets so the engine never queues
        // a frame for its dead socket.
        let app = make_app();
        let canvas = MockRgbDevice::new_with_zones(
            "dead0",
            "ring",
            3,
            false,
            vec![make_zone("dead0", "ring")],
        );
        canvas.live.store(false, Ordering::SeqCst);
        let direct_dev = MockRgbDevice::new("dead1", "ring", 3, false);
        *direct_dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "breathing".into(),
            params: HashMap::new(),
        });
        direct_dev.live.store(false, Ordering::SeqCst);
        {
            let mut devices = app.devices.write().await;
            devices.push(canvas as Arc<dyn Device>);
            devices.push(direct_dev as Arc<dyn Device>);
        }

        let engine = RgbEngine::new(app).await;
        let (canvas_state, rgb_devices, direct) = engine.sync_tick_state().await;

        assert!(
            canvas_state.placed_zones.is_empty(),
            "offline canvas zone dropped"
        );
        assert!(
            !rgb_devices.contains_key("dead0"),
            "offline device not mapped"
        );
        assert!(direct.is_empty(), "offline direct-effect device dropped");
    }

    #[tokio::test]
    async fn dispatch_writes_skips_an_offline_device() {
        let engine = RgbEngine::new(make_app()).await;
        let dev = MockRgbDevice::new("dev0", "ring", 3, false);
        dev.live.store(false, Ordering::SeqCst);
        let dev_arc: Arc<dyn Device> = dev.clone();
        let mut pending: PendingWrites = HashMap::new();
        pending.insert(
            "dev0".to_string(),
            (
                dev_arc,
                vec![("ring".to_string(), vec![RgbColor::default(); 3])],
            ),
        );

        engine.dispatch_writes(pending);
        engine.drain_writes().await;

        assert_eq!(
            dev.write_count.load(Ordering::SeqCst),
            0,
            "an offline device must not be written to"
        );
    }

    #[tokio::test]
    async fn sync_tick_state_collects_direct_effect_devices() {
        let app = make_app();
        let dev = MockRgbDevice::new("dev0", "ring", 3, false);
        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "breathing".into(),
            params: HashMap::new(),
        });
        app.devices.write().await.push(dev as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        let (_, _, direct) = engine.sync_tick_state().await;
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].1, "breathing");
    }

    #[tokio::test]
    async fn tick_direct_effect_writes_frame() {
        let app = make_app();
        let dev = MockRgbDevice::new("dev0", "ring", 4, false);
        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "breathing".into(),
            params: HashMap::new(),
        });
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.tick(0.1, 0.05, 0).await;
        engine.drain_writes().await;
        assert!(dev.write_count.load(Ordering::SeqCst) >= 1);
    }

    /// Load a single-file plugin declaring one direct effect that echoes the
    /// live `sensor` callback arg into its red channel, so a test can assert
    /// the engine actually threads a live sensor reading through to a
    /// plugin-declared direct effect (mirrors `load_test_effect_plugin`
    /// above but exercises the `sensor` param/arg wiring instead).
    fn load_test_sensor_plugin(app: &Arc<AppState>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("engine_sensor_fx");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.yaml"),
            "id: engine_sensor_fx\ntype: effect\n",
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("main.lua"),
            r#"
                return {
                  effects = {
                    { kind = "direct", id = "probe", name = "Probe" },
                  },
                  led_colors_probe = function(leds, t, dt, params, sensor)
                    local v = 0.0
                    if sensor ~= nil then v = sensor / 100.0 end
                    local out = {}
                    for i in ipairs(leds) do
                      out[i] = { r = v, g = 0, b = 0 }
                    end
                    return out
                  end,
                }
            "#,
        )
        .unwrap();
        app.registry.load_all(tmp.path());
        app.registry
            .replace_policy(&crate::config::PluginPolicy::default());
        tmp
    }

    #[tokio::test]
    async fn tick_direct_plugin_effect_receives_live_sensor_reading() {
        let app = make_app();
        let _tmp = load_test_sensor_plugin(&app);

        let sensor_dev = crate::test_support::MockDevice::new("sensor_dev").with_sensor(vec![
            halod_shared::types::Sensor {
                id: "temp1".into(),
                name: "CPU".into(),
                value: 80.0,
                unit: halod_shared::types::SensorUnit::Celsius,
                sensor_type: halod_shared::types::SensorType::Temperature,
                visibility: halod_shared::types::VisibilityState::Visible,
            },
        ]);
        app.devices
            .write()
            .await
            .push(Arc::new(sensor_dev) as Arc<dyn Device>);

        let dev = MockRgbDevice::new("dev0", "ring", 2, false);
        let mut params = HashMap::new();
        params.insert("sensor".into(), EffectParamValue::Str("temp1".into()));
        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "engine_sensor_fx:probe".to_string(),
            params,
        });
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.tick(0.1, 1.0, 0).await;
        engine.drain_writes().await;

        let colors = dev.last_colors.lock().unwrap().clone().unwrap();
        // 80.0 / 100.0 = 0.8, gamma-encoded on the way out.
        let expected_r = linear_to_led(0.8, LED_GAMMA);
        assert!(
            colors.iter().all(|c| c.r == expected_r),
            "expected the live sensor reading gamma-encoded into red, got {colors:?}"
        );
    }

    #[tokio::test]
    async fn tick_unknown_direct_effect_falls_back_to_off_and_caches() {
        let app = make_app();
        let dev = MockRgbDevice::new("dev0", "ring", 4, false);
        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "sensor_gradient".into(),
            params: HashMap::new(),
        });
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.tick(0.1, 0.05, 0).await;
        engine.drain_writes().await;
        engine.tick(0.2, 0.05, 1).await;
        engine.drain_writes().await;

        assert!(dev.write_count.load(Ordering::SeqCst) >= 2);
        let frame = dev.last_frame.lock().unwrap().clone();
        assert_eq!(frame.len(), 4);
        assert!(frame.iter().all(|c| (c.r, c.g, c.b) == (0, 0, 0)));
        // Cached under the same key, so the unknown id is built (and warned) once.
        let live = engine.live_direct.lock().await;
        assert!(live.contains_key("dev0"));
    }

    #[tokio::test]
    async fn tick_prunes_direct_effect_when_reverted() {
        let app = make_app();
        let dev = MockRgbDevice::new("dev0", "ring", 4, false);
        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "breathing".into(),
            params: HashMap::new(),
        });
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.tick(0.1, 0.05, 0).await;
        engine.drain_writes().await;
        let after_first = dev.write_count.load(Ordering::SeqCst);
        assert!(after_first >= 1);

        *dev.rgb_state.lock().unwrap() = Some(RgbState::Static {
            color: RgbColor { r: 1, g: 2, b: 3 },
        });
        engine.tick(0.2, 0.05, 1).await;
        engine.drain_writes().await;
        assert_eq!(
            dev.write_count.load(Ordering::SeqCst),
            after_first,
            "reverted device must not be driven by the direct pass"
        );
        assert!(engine.live_direct.lock().await.is_empty());
    }

    #[tokio::test]
    async fn reconcile_engine_mode_reverts_zoneless_engine_device() {
        let app = make_app();
        let dev = MockRgbDevice::new_engine_mode("dev0", "ring", false);
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.reconcile_engine_mode(&CanvasState::default()).await;

        assert!(matches!(dev.current_state(), Some(RgbState::Static { .. })));
        assert_eq!(dev.apply_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reconcile_does_not_blank_device_that_left_canvas_for_a_direct_effect() {
        let app = make_app();
        let dev = MockRgbDevice::new_engine_mode("dev0", "ring", false);
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);
        let engine = RgbEngine::new(app).await;

        let mut placed = CanvasState::default();
        placed.placed_zones.push(make_zone("dev0", "ring"));
        engine.reconcile_engine_mode(&placed).await;

        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "breathing".into(),
            params: HashMap::new(),
        });

        engine.reconcile_engine_mode(&CanvasState::default()).await;
        assert!(matches!(
            dev.current_state(),
            Some(RgbState::DirectEffect { .. })
        ));
    }

    #[tokio::test]
    async fn reconcile_engine_mode_does_not_loop_when_revert_write_fails() {
        let app = make_app();
        let dev = MockRgbDevice::new_engine_mode("dev0", "ring", true);
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.reconcile_engine_mode(&CanvasState::default()).await;
        assert!(matches!(dev.current_state(), Some(RgbState::Static { .. })));
        engine.reconcile_engine_mode(&CanvasState::default()).await;
        assert_eq!(dev.apply_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reconcile_engine_mode_does_not_loop_when_driver_never_records_state() {
        let app = make_app();
        let dev = MockRgbDevice::new_engine_mode("dev0", "ring", true);
        dev.skip_record_on_fail.store(true, Ordering::SeqCst);
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.reconcile_engine_mode(&CanvasState::default()).await;
        assert!(matches!(dev.current_state(), Some(RgbState::Engine)));
        engine.reconcile_engine_mode(&CanvasState::default()).await;
        assert_eq!(dev.apply_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn prepare_zone_returns_none_when_zone_not_in_descriptor() {
        let dev: Arc<dyn Device> = MockRgbDevice::new("dev0", "ring", 3, false);
        assert!(prepare_zone(&dev, "not_a_real_zone", solid_colors(3)).is_none());
    }

    #[test]
    fn prepare_zone_builds_entries_for_each_led() {
        let dev: Arc<dyn Device> = MockRgbDevice::new("dev0", "ring", 4, false);
        let (colors, entries) = prepare_zone(&dev, "ring", solid_colors(4)).unwrap();
        assert_eq!(colors.len(), 4);
        assert_eq!(entries.len(), 4);
        assert!(entries
            .iter()
            .all(|e| e.device_id == "dev0" && e.zone_id == "ring"));
        assert_eq!(
            entries.iter().map(|e| e.led_id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[tokio::test]
    async fn dispatch_writes_frame_when_device_is_free() {
        let engine = RgbEngine::new(make_app()).await;
        let dev = MockRgbDevice::new("dev0", "ring", 3, false);
        let mut pending: PendingWrites = HashMap::new();
        pending.insert(
            "dev0".into(),
            (
                dev.clone() as Arc<dyn Device>,
                vec![("ring".into(), solid_colors(3))],
            ),
        );
        engine.dispatch_writes(pending);
        engine.drain_writes().await;
        assert_eq!(dev.write_count.load(Ordering::SeqCst), 1);
        assert_eq!(dev.last_frame.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn dispatch_drops_frame_while_previous_write_is_in_flight() {
        let engine = RgbEngine::new(make_app()).await;
        let dev = MockRgbDevice::new("dev0", "ring", 3, false);
        // Occupy the device's slot with a task that never finishes on its own.
        let (unblock, blocked) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = blocked.await;
        });
        engine
            .write_slots
            .lock()
            .unwrap()
            .insert("dev0".into(), handle);

        let mut pending: PendingWrites = HashMap::new();
        pending.insert(
            "dev0".into(),
            (
                dev.clone() as Arc<dyn Device>,
                vec![("ring".into(), solid_colors(3))],
            ),
        );
        engine.dispatch_writes(pending);
        tokio::task::yield_now().await;
        assert_eq!(
            dev.write_count.load(Ordering::SeqCst),
            0,
            "frame must be dropped, not queued, while the prior write is in flight"
        );
        let _ = unblock.send(());
    }

    #[tokio::test]
    async fn publish_frame_delivers_frame_to_subscriber() {
        let engine = RgbEngine::new(make_app()).await;
        let mut rx = engine.subscribe();
        engine.publish_frame(black_srgb(), 42, vec![]);

        let frame = rx.try_recv().expect("frame available immediately");
        assert_eq!(frame.frame_id, 42);
        assert_eq!(frame.canvas_w, CANVAS_W);
        assert_eq!(frame.canvas_h, CANVAS_H);
    }

    #[test]
    fn idle_interval_ms_stays_at_base_with_work_or_before_grace() {
        let now = Instant::now();
        assert_eq!(idle_interval_ms(None, now, 50), 50);
        assert_eq!(
            idle_interval_ms(Some(now - Duration::from_secs(5)), now, 50),
            50
        );
    }

    #[test]
    fn idle_interval_ms_drops_to_idle_poll_after_grace() {
        let now = Instant::now();
        assert_eq!(
            idle_interval_ms(Some(now - IDLE_GRACE), now, 50),
            IDLE_POLL_MS
        );
        assert_eq!(
            idle_interval_ms(Some(now - Duration::from_secs(60)), now, 50),
            IDLE_POLL_MS
        );
    }

    #[tokio::test]
    async fn tick_reports_no_work_with_no_zones_no_direct_no_subscriber() {
        let engine = RgbEngine::new(make_app()).await;
        assert!(!engine.tick(0.0, 0.0, 0).await);
    }

    #[tokio::test]
    async fn tick_reports_work_with_a_placed_zone() {
        let app = make_app();
        let zone = make_zone("dev0", "ring");
        let dev: Arc<dyn Device> =
            MockRgbDevice::new_with_zones("dev0", "ring", 3, false, vec![zone.clone()]);
        app.devices.write().await.push(dev);

        let engine = RgbEngine::new(app).await;
        assert!(engine.tick(0.0, 0.0, 0).await);
    }

    #[tokio::test]
    async fn tick_reports_work_with_a_direct_effect_device() {
        let app = make_app();
        let dev = MockRgbDevice::new("dev0", "ring", 4, false);
        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "breathing".into(),
            params: HashMap::new(),
        });
        app.devices.write().await.push(dev as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        assert!(engine.tick(0.0, 0.0, 0).await);
    }

    #[tokio::test]
    async fn tick_reports_work_with_a_live_subscriber() {
        let engine = RgbEngine::new(make_app()).await;
        let _rx = engine.subscribe();
        assert!(engine.tick(0.0, 0.0, 0).await);
    }

    // ── Plugin-declared effects (end-to-end through a live tick) ───────────

    /// Load a single-file plugin declaring one pixmap and one direct effect
    /// into `app`'s plugin registry.
    fn load_test_effect_plugin(app: &Arc<AppState>) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("engine_test_fx");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.yaml"),
            "id: engine_test_fx\ntype: effect\n",
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("main.lua"),
            r#"
                return {
                  effects = {
                    { kind = "pixmap", id = "solid", name = "Solid" },
                    { kind = "direct", id = "ramp", name = "Ramp" },
                  },
                  render_solid = function(buf, t, dt, params)
                    for i = 0, #buf - 1, 4 do
                      buf:set_u8(i, 9)
                      buf:set_u8(i + 1, 8)
                      buf:set_u8(i + 2, 7)
                      buf:set_u8(i + 3, 255)
                    end
                  end,
                  led_colors_ramp = function(leds, t, dt, params)
                    local out = {}
                    for i, led in ipairs(leds) do
                      out[i] = { r = led.p, g = 0, b = 0 }
                    end
                    return out
                  end,
                }
            "#,
        )
        .unwrap();
        app.registry.load_all(tmp.path());
        app.registry
            .replace_policy(&crate::config::PluginPolicy::default());
        tmp
    }

    async fn set_default_effect(app: &Arc<AppState>, def: EffectDef) {
        let mut cfg = app.config.write().await;
        let profile = cfg
            .profiles
            .get_mut(halod_shared::types::DEFAULT_PROFILE_NAME)
            .unwrap();
        profile.lighting.canvas = Some(CanvasState {
            default_effect: Some("inst".to_string()),
            effects: [("inst".to_string(), def)].into_iter().collect(),
            ..Default::default()
        });
    }

    #[tokio::test]
    async fn tick_renders_a_plugin_pixmap_effect_end_to_end() {
        let app = make_app();
        let _tmp = load_test_effect_plugin(&app);
        let zone = make_zone("dev0", "ring");
        let dev = MockRgbDevice::new_with_zones("dev0", "ring", 2, false, vec![zone]);
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);
        set_default_effect(
            &app,
            EffectDef {
                effect_id: "engine_test_fx:solid".to_string(),
                name: None,
                params: Default::default(),
            },
        )
        .await;

        let engine = RgbEngine::new(app).await;
        engine.tick(0.0, 0.016, 0).await;
        engine.drain_writes().await;

        let colors = dev.last_colors.lock().unwrap().clone().unwrap();
        // The pixmap buffer holds linear-light bytes; the sampler gamma-encodes
        // on read (`linear_to_led`), so the raw fill (9,8,7) comes out as this.
        let expected = RgbColor {
            r: linear_to_led(9.0 / 255.0, LED_GAMMA),
            g: linear_to_led(8.0 / 255.0, LED_GAMMA),
            b: linear_to_led(7.0 / 255.0, LED_GAMMA),
        };
        assert!(
            colors.iter().all(|c| *c == expected),
            "expected the plugin's solid fill sampled into the zone as {expected:?}, got {colors:?}"
        );
    }

    #[tokio::test]
    async fn tick_renders_a_plugin_direct_effect_end_to_end() {
        let app = make_app();
        let _tmp = load_test_effect_plugin(&app);
        let dev = MockRgbDevice::new("dev0", "ring", 3, false);
        *dev.rgb_state.lock().unwrap() = Some(RgbState::DirectEffect {
            id: "engine_test_fx:ramp".to_string(),
            params: HashMap::new(),
        });
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn Device>);

        let engine = RgbEngine::new(app).await;
        engine.tick(0.0, 0.016, 0).await;
        engine.drain_writes().await;

        let colors = dev.last_colors.lock().unwrap().clone().unwrap();
        assert_eq!(colors.len(), 3);
        // led_colors_ramp returns r=p (chain fraction), gamma-encoded on the way out.
        assert_eq!(colors[0].r, 0);
        assert!(
            colors[2].r > colors[0].r,
            "ramp must increase along the chain"
        );
    }
}
