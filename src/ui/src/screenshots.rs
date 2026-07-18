// SPDX-License-Identifier: GPL-3.0-or-later
//! Headless screenshot generators for the docs (`docs/images/`).
//!
//! These are `#[ignore]`d — they render real screens through a wgpu-backed
//! [`egui_kittest`] harness (no window, no daemon) and write PNGs, so they only
//! run when explicitly asked for. Each generator seeds an [`AppState`] fixture,
//! drives [`App::draw`] exactly as production does, and captures the frame.
//! Regenerate after a visual change with `--features screenshots`.
//!
//! Two gotchas when running them:
//!  * wgpu needs a Vulkan adapter. `nix develop` ships no loader, so run the
//!    built test binary against the host stack, pointing it at a software
//!    rasterizer if there's no GPU:
//!    `VK_ICD_FILENAMES=/run/opengl-driver/share/vulkan/icd.d/lvp_icd.x86_64.json`.
//!  * Run **one generator per process** — two wgpu contexts in a single process
//!    deadlock under lavapipe. Invoke each test with `--exact`, not a shared
//!    `--test-threads=1` run.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use egui::vec2;
use halod_shared::types::AppState;
use serde_json::json;

use crate::app::App;
use crate::domain::models::device_tabs::{tabs_for, TabKind};
use crate::domain::state::Page;
use crate::domain::tray::Tray;
use crate::runtime::ipc;
use crate::ui::screens::device::DeviceUi;

/// Logical window size, matching the production viewport in `main.rs`.
const SIZE: (f32, f32) = (1320.0, 860.0);
/// Rendered at 2× for crisp docs images.
const SCALE: f32 = 2.0;

/// Render a seeded state to `docs/images/<name>.png`. `setup` positions the app
/// on the target page/tab before the captured frame.
fn capture(name: &str, state: AppState, setup: impl FnOnce(&mut App)) {
    let force_quit = Arc::new(AtomicBool::new(false));
    let (cmd, ui_rx, _senders, sinks) = ipc::fake(state, true);
    let hidden = Arc::new(AtomicBool::new(false));
    let mut app = App::new(
        ui_rx,
        sinks,
        cmd,
        Tray::headless(),
        force_quit,
        hidden,
        Arc::new(crate::domain::state::HideState::default()),
    );
    app.entered = true;
    setup(&mut app);

    // The theme (fonts, dark visuals) is installed by `main.rs` at startup. The
    // harness draws once on build, before we can touch its context, so the first
    // pass installs the theme and skips drawing — set_fonts only binds the custom
    // families ("bold", "mono", …) from the next frame.
    let mut themed = false;
    let mut harness = egui_kittest::Harness::builder()
        .with_size(vec2(SIZE.0, SIZE.1))
        .with_pixels_per_point(SCALE)
        .wgpu()
        .build_ui_state(
            move |ui: &mut egui::Ui, app: &mut App| {
                if !themed {
                    crate::ui::theme::install(ui.ctx());
                    themed = true;
                    return;
                }
                app.draw(ui);
            },
            app,
        );

    // The UI repaints every frame (title-bar clock, grace timer), so `run` would
    // trip its no-more-repaints assertion; a fixed few steps settles layout.
    harness.run_steps(4);
    let image = harness.render().expect("wgpu render");
    let path = out_dir().join(format!("{name}.png"));
    image.save(&path).expect("save png");
    eprintln!(
        "wrote {} ({}x{})",
        path.display(),
        image.width(),
        image.height()
    );
}

/// `docs/images/`, resolved from this crate's manifest dir (`src/ui`).
fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/images")
        .canonicalize()
        .expect("docs/images exists")
}

/// A capability entry in `WireDevice.capabilities` (internally tagged).
fn cap(kind: &str, data: serde_json::Value) -> serde_json::Value {
    json!({ "kind": kind, "data": data })
}

/// `gui` config that suppresses first-run onboarding and every page/tab tour so
/// the captured screen isn't covered by a wizard or coach-mark overlay.
fn gui_onboarded() -> serde_json::Value {
    let mut seen: Vec<String> = crate::domain::tour::defs::ALL_TOUR_KEYS
        .iter()
        .map(|k| k.id().to_string())
        .collect();
    seen.push(halod_shared::types::ONBOARDING_TOUR_KEY.to_string());
    json!({ "seen_tours": seen })
}

fn home_state() -> AppState {
    let devices = json!([
        {
            "id": "nzxt-kraken", "name": "Kraken Elite 360",
            "vendor": "NZXT", "model": "Kraken Elite", "device_type": "a_i_o",
            "connected": true, "transport": "hid",
            "capabilities": [
                cap("sensors", json!([
                    {"id": "liquid", "name": "Liquid", "value": 34.2, "unit": "celsius", "sensor_type": "temperature"},
                ])),
                cap("cooling", json!({"channels": [
                    {"id": "pump", "name": "Pump", "controllable": true, "rpm": 2100, "duty": 55},
                    {"id": "fans", "name": "Fans", "controllable": true, "rpm": 980, "duty": 42},
                ]})),
            ],
        },
        {
            "id": "g-pro-x", "name": "G Pro X Superlight",
            "vendor": "Logitech", "model": "G Pro X Superlight", "device_type": "mouse",
            "connected": true, "connection_type": "wireless", "transport": "hid",
            "capabilities": [
                cap("battery", json!([{"key": "main", "label": "Battery", "level": 82, "status": "discharging"}])),
                cap("dpi", json!({
                    "steps": [800, 1600, 3200], "current_index": 1, "current_dpi": 1600,
                    "available_dpis": [400, 800, 1600, 3200, 6400], "mode": "host",
                })),
            ],
        },
        {
            "id": "rog-z790", "name": "ROG Strix Z790-E",
            "vendor": "ASUS", "model": "ROG Strix Z790-E", "device_type": "motherboard",
            "connected": true, "transport": "smbus",
            "capabilities": [
                cap("sensors", json!([
                    {"id": "cpu", "name": "CPU", "value": 46.0, "unit": "celsius", "sensor_type": "temperature"},
                    {"id": "vrm", "name": "VRM", "value": 51.0, "unit": "celsius", "sensor_type": "temperature"},
                ])),
            ],
        },
        {
            "id": "apex-pro", "name": "Apex Pro TKL",
            "vendor": "SteelSeries", "model": "Apex Pro TKL", "device_type": "keyboard",
            "connected": true, "transport": "hid", "capabilities": [],
        },
        {
            "id": "trident-z5", "name": "Trident Z5 RGB",
            "vendor": "G.Skill", "model": "Trident Z5 RGB", "device_type": "ram",
            "connected": true, "transport": "smbus", "capabilities": [],
        },
        {
            "id": "arctis-nova", "name": "Arctis Nova Pro",
            "vendor": "SteelSeries", "model": "Arctis Nova Pro", "device_type": "headset",
            "connected": true, "connection_type": "wireless", "transport": "hid",
            "capabilities": [
                cap("battery", json!([{"key": "main", "label": "Battery", "level": 64, "status": "charging"}])),
            ],
        },
    ]);

    serde_json::from_value(json!({
        "devices": devices,
        "profiles": { "active": "Gaming", "available": ["Default", "Gaming", "Silent"] },
        "gui": gui_onboarded(),
    }))
    .expect("home fixture")
}

fn plugins_state() -> AppState {
    fn plugin(v: serde_json::Value) -> serde_json::Value {
        v
    }
    let plugins = json!([
        plugin(json!({
            "id": "logitech", "name": "Logitech", "path": "/plugins/logitech/main.lua",
            "plugin_type": "device", "enabled": true, "active": true, "consented": true,
            "author": "HaloDaemon", "version": "2024.6.2", "license": "GPL-3.0-or-later",
            "description": "Lightspeed mice, keyboards, and headsets — battery, DPI, and RGB.",
            "capabilities": ["Battery", "DPI", "RGB"], "targets": ["G Pro X", "G502", "G915"],
            "declared_permissions": ["hid"],
            "source": {"kind": "repo", "slug": "halodaemon-plugins"},
        })),
        plugin(json!({
            "id": "halo_effects", "name": "Halo Effects", "path": "/plugins/halo_effects/main.lua",
            "plugin_type": "effect", "enabled": true, "active": true, "consented": true,
            "author": "HaloDaemon", "version": "2024.6.0", "license": "GPL-3.0-or-later",
            "description": "The built-in effect pack: breathing, rainbow, comet, and screen sampler.",
            "effect_names": ["Breathing", "Rainbow", "Comet", "Screen Sampler"],
            "source": {"kind": "repo", "slug": "halodaemon-plugins"},
        })),
        plugin(json!({
            "id": "nzxt_kraken", "name": "NZXT Kraken", "path": "/plugins/nzxt_kraken/main.lua",
            "plugin_type": "device", "enabled": true, "active": true, "consented": true,
            "author": "HaloDaemon", "version": "2024.6.1", "license": "GPL-3.0-or-later",
            "description": "Liquid coolers with pump control, LCD panel, and RGB ring.",
            "capabilities": ["Cooling", "LCD", "RGB"], "targets": ["Kraken Elite", "Kraken 2023"],
            "declared_permissions": ["hid"],
            "source": {"kind": "repo", "slug": "halodaemon-plugins"},
        })),
        plugin(json!({
            "id": "openrgb", "name": "OpenRGB", "path": "/plugins/openrgb/main.lua",
            "plugin_type": "integration", "enabled": false, "active": false, "consented": false,
            "author": "Community", "version": "0.9.2",
            "description": "Import devices from a running OpenRGB SDK server over the network.",
            "capabilities": ["RGB"], "declared_permissions": ["network"],
            "source": {"kind": "repo", "slug": "halodaemon-plugins"},
        })),
        plugin(json!({
            "id": "nanoleaf", "name": "Nanoleaf", "path": "/plugins/nanoleaf/main.lua",
            "plugin_type": "integration", "enabled": true, "active": true, "consented": true,
            "author": "Community", "version": "1.4.0",
            "description": "Nanoleaf light panels over the local network via the Open API.",
            "capabilities": ["RGB"], "declared_permissions": ["network"],
            "source": {"kind": "repo", "slug": "halodaemon-plugins"},
        })),
        plugin(json!({
            "id": "asus_aura", "name": "ASUS Aura", "path": "/plugins/asus_aura/main.lua",
            "plugin_type": "device", "enabled": true, "active": true, "consented": true,
            "author": "Community", "version": "2024.2.0",
            "description": "Aura-capable motherboards and RGB memory over SMBus.",
            "capabilities": ["RGB"], "targets": ["ROG Strix", "TUF Gaming", "Aura DRAM"],
            "declared_permissions": ["smbus"],
            "source": {"kind": "repo", "slug": "halodaemon-plugins"},
        })),
    ]);

    let repos = json!([
        {
            "url": "https://github.com/HaloDaemon/HaloDaemon-plugins",
            "slug": "halodaemon-plugins",
            "locked_sha": "9f2c1ab7d4e6",
            "official": true,
            "signature": {"status": "verified"},
            "compatibility": {"status": "compatible"},
        },
    ]);

    serde_json::from_value(json!({
        "plugins": { "plugins": plugins, "repos": repos },
        "profiles": { "active": "Gaming", "available": ["Default", "Gaming", "Silent"] },
        "gui": gui_onboarded(),
    }))
    .expect("plugins fixture")
}

// ── Device-page fixtures ─────────────────────────────────────────────────────

/// A `DeviceUi` opened on the capability tab of `kind` for device `id`. The tab
/// index is resolved from the device's capabilities so it survives tab-set
/// changes (the `tab_kind` field that would do this natively is module-private).
fn device_ui_on_tab(state: &AppState, id: &str, kind: TabKind) -> DeviceUi {
    let mut ui = DeviceUi::new(id.to_string());
    if let Some(dev) = state.devices.iter().find(|d| d.id == id) {
        ui.tab = tabs_for(dev)
            .iter()
            .position(|t| t.kind == kind)
            .unwrap_or(0);
    }
    ui
}

/// A vertical ARGB strip zone: `n` LEDs laid out in a column.
fn strip_leds(n: u32) -> serde_json::Value {
    let leds: Vec<_> = (0..n)
        .map(|i| json!({"id": i, "x": 0.0, "y": i as f32 / (n - 1).max(1) as f32}))
        .collect();
    json!(leds)
}

/// A ring zone: `n` LEDs around a circle.
fn ring_leds(base: u32, n: u32) -> serde_json::Value {
    let leds: Vec<_> = (0..n)
        .map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            json!({"id": base + i, "x": 0.5 + 0.5 * a.cos(), "y": 0.5 + 0.5 * a.sin()})
        })
        .collect();
    json!(leds)
}

/// A device exposing a rich Lighting capability (ARGB strip + fan ring) with a
/// live rainbow effect — drives both the device Lighting tab and the RGB canvas.
fn lighting_cap() -> serde_json::Value {
    cap(
        "lighting",
        json!({
            "descriptor": {
                "channels": [
                    {"id": "strip", "name": "ARGB Strip", "topology": {"type": "linear"},
                     "leds": strip_leds(16), "color_order": "grb"},
                    {"id": "fan_ring", "name": "Fan Ring", "topology": {"type": "ring"},
                     "leds": ring_leds(100, 12), "color_order": "rgb"},
                ],
                "native_effects": [
                    {"id": "static", "name": "Static", "params": [
                        {"id": "color", "label": "Color", "kind": {"kind": "color"}, "default": {"r": 255, "g": 0, "b": 0}}
                    ]},
                    {"id": "rainbow", "name": "Rainbow Wave", "params": [
                        {"id": "speed", "label": "Speed", "kind": {"kind": "range", "min": 0.0, "max": 100.0, "step": 1.0}, "default": 50.0},
                        {"id": "brightness", "label": "Brightness", "kind": {"kind": "range", "min": 0.0, "max": 100.0, "step": 1.0}, "default": 80.0},
                        {"id": "direction", "label": "Direction", "kind": {"kind": "enum", "options": ["Left", "Right"]}, "default": "Right"},
                    ]},
                ],
            },
            "state": {"mode": "native_effect", "id": "rainbow",
                      "params": {"speed": 65.0, "brightness": 80.0, "direction": "Right"}},
        }),
    )
}

fn strip_device() -> serde_json::Value {
    json!({
        "id": "argb-strip", "name": "Aura ARGB Strip",
        "vendor": "ASUS", "model": "Addressable Gen2", "device_type": "led_strip",
        "connected": true, "transport": "smbus",
        "capabilities": [lighting_cap()],
    })
}

/// Device-page **Lighting/RGB** tab (per-zone LED editor).
fn lighting_device_state() -> AppState {
    serde_json::from_value(json!({
        "devices": [strip_device()],
        "profiles": {"active": "Gaming", "available": ["Default", "Gaming", "Silent"]},
        "gui": gui_onboarded(),
    }))
    .expect("lighting device fixture")
}

/// Mouse with editable DPI stages for the Performance tab.
fn dpi_state() -> AppState {
    serde_json::from_value(json!({
        "devices": [{
            "id": "g502", "name": "G502 X Plus",
            "vendor": "Logitech", "model": "G502 X Plus", "device_type": "mouse",
            "connected": true, "connection_type": "wireless", "transport": "hid",
            "capabilities": [
                cap("battery", json!([{"key": "main", "label": "Battery", "level": 76, "status": "discharging"}])),
                cap("dpi", json!({
                    "steps": [800, 1600, 3200, 6400], "current_index": 1, "current_dpi": 1600,
                    "available_dpis": [100, 200, 400, 800, 1600, 3200, 6400, 12800, 25600],
                    "mode": "onboard",
                })),
            ],
        }],
        "profiles": {"active": "Gaming", "available": ["Default", "Gaming", "Silent"]},
        "gui": gui_onboarded(),
    }))
    .expect("dpi fixture")
}

/// AIO with a Cooling capability + temperature sensors, plus the matching fan
/// curves in `AppState.cooling` — drives both the Cooling page and the
/// per-device fan-curve editor.
fn cooling_device() -> serde_json::Value {
    json!({
        "id": "aio-001", "name": "Kraken Z73",
        "vendor": "NZXT", "model": "Kraken Z73", "device_type": "a_i_o",
        "connected": true, "transport": "hid",
        "capabilities": [
            cap("cooling", json!({"channels": [
                {"id": "pump", "name": "Pump", "kind": "pump", "controllable": true, "rpm": 2400, "duty": 70},
                {"id": "fan1", "name": "Fan 1", "kind": "fan", "controllable": true, "rpm": 1180, "duty": 45},
                {"id": "fan2", "name": "Fan 2", "kind": "fan", "controllable": true, "rpm": 1210, "duty": 45},
            ]})),
            cap("sensors", json!([
                {"id": "cpu-package", "name": "CPU Package", "value": 62.0, "unit": "celsius", "sensor_type": "temperature"},
                {"id": "coolant", "name": "Coolant", "value": 38.0, "unit": "celsius", "sensor_type": "temperature"},
            ])),
        ],
    })
}

fn cooling_appstate_field() -> serde_json::Value {
    json!({
        "fan_curves": [
            {"device_id": "aio-001", "channel_id": "fan1", "sensor_id": "cpu-package",
             "points": [[20.0, 20.0], [40.0, 35.0], [55.0, 55.0], [70.0, 80.0], [85.0, 100.0]], "status": "ok"},
            {"device_id": "aio-001", "channel_id": "pump", "sensor_id": "coolant",
             "points": [[20.0, 50.0], [45.0, 70.0], [65.0, 100.0]], "status": "ok"},
        ],
        "preset_curves": [
            {"id": "silent", "name": "Silent", "points": [[30.0, 20.0], [60.0, 40.0], [80.0, 70.0]]},
            {"id": "balanced", "name": "Balanced", "points": [[30.0, 30.0], [55.0, 55.0], [80.0, 90.0]]},
            {"id": "performance", "name": "Performance", "points": [[30.0, 40.0], [60.0, 100.0]]},
        ],
        "config": {"fan_curve_enabled": true, "fan_curve_tick_ms": 1000, "fan_failsafe_duty": 60},
    })
}

fn cooling_state() -> AppState {
    serde_json::from_value(json!({
        "devices": [cooling_device()],
        "cooling": cooling_appstate_field(),
        "profiles": {"active": "Gaming", "available": ["Default", "Gaming", "Silent"]},
        "gui": gui_onboarded(),
    }))
    .expect("cooling fixture")
}

/// A round Kraken LCD panel plus a populated editor template, for the LCD
/// screen editor.
fn lcd_state() -> AppState {
    serde_json::from_value(json!({
        "devices": [{
            "id": "kraken-lcd", "name": "NZXT Kraken Elite",
            "vendor": "NZXT", "model": "Kraken Elite 360", "device_type": "a_i_o",
            "connected": true, "transport": "hid",
            "capabilities": [cap("lcd", json!({
                "descriptor": {"shape": "circle", "width": 240, "height": 240,
                    "supported_rotations": ["r0", "r90", "r180", "r270"],
                    "supported_image_types": ["png", "jpg", "gif"], "latches_last_frame": false},
                "brightness": 80, "rotation": "r0", "mode": "engine",
                "active_image": null, "video_path": null, "raw_streaming": false, "health": "stable",
            }))],
        }],
        "lcd": {"engine": {"available_templates": [], "device_templates": {}, "available_widgets": [
            {"id": "halo_lcd:text", "plugin_id": "halo_lcd", "name": "Text", "icon": "text.svg", "resize": "uniform", "uses_color": true, "uses_font": true},
            {"id": "halo_lcd:gauge", "plugin_id": "halo_lcd", "name": "Gauge", "icon": "gauge.svg", "resize": "uniform"},
            {"id": "halo_lcd:clock", "plugin_id": "halo_lcd", "name": "Clock", "icon": "clock.svg", "resize": "uniform"},
            {"id": "halo_lcd:logo", "plugin_id": "halo_lcd", "name": "Logo", "icon": "logo.svg", "resize": "box"},
        ]}},
        "profiles": {"active": "Gaming", "available": ["Default", "Gaming", "Silent"]},
        "gui": gui_onboarded(),
    }))
    .expect("lcd fixture")
}

/// The editor template seeded into `DeviceUi.lcd.editor.def`.
fn lcd_template() -> halod_shared::lcd_custom::CustomTemplateDef {
    serde_json::from_value(json!({
        "widgets": [
            {"id": "w1", "widget": "halo_lcd:text", "x": 0.5, "y": 0.18, "scale": 1.0,
             "color": {"r": 255, "g": 255, "b": 255}, "params": {"text": "CPU"}},
            {"id": "w2", "widget": "halo_lcd:gauge", "x": 0.5, "y": 0.5, "scale": 1.6, "params": {}},
            {"id": "w3", "widget": "halo_lcd:clock", "x": 0.5, "y": 0.8, "scale": 0.9, "params": {}},
        ],
        "style": {"accent": {"r": 0, "g": 200, "b": 220}, "background": {"kind": "flow"}, "font": "Noto Sans"},
    }))
    .expect("lcd template")
}

/// A single profile detail page with an app-focus auto-activate rule.
fn profiles_state() -> AppState {
    serde_json::from_value(json!({
        "profiles": {
            "active": "Gaming",
            "available": ["Default", "Gaming", "Silent"],
            "app_rules": [
                {"process_names": ["cs2.exe", "steam.exe"], "profile": "Gaming", "enabled": true},
                {"process_names": ["obs64.exe"], "profile": "Silent", "enabled": true},
            ],
        },
        "gui": gui_onboarded(),
    }))
    .expect("profiles fixture")
}

// ── Generators ───────────────────────────────────────────────────────────────

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn home() {
    capture("home", home_state(), |a| a.page = Page::Home);
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn plugins() {
    capture("plugins", plugins_state(), |a| a.page = Page::Plugins);
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn cooling() {
    capture("cooling", cooling_state(), |a| a.page = Page::Cooling);
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn canvas() {
    // The RGB Lighting page defaults to the Effects Canvas tab; seed a placed
    // device so the canvas is populated.
    let mut state = lighting_device_state();
    state.lighting.canvas.placed_zones = serde_json::from_value(json!([
        {"device_id": "argb-strip", "channel_id": "strip", "x": 0.16, "y": 0.22, "w": 0.5, "h": 0.10},
        {"device_id": "argb-strip", "channel_id": "fan_ring", "x": 0.55, "y": 0.5, "w": 0.22, "h": 0.22},
    ]))
    .expect("placed zones");
    capture("canvas", state, |a| a.page = Page::Lighting);
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn lighting() {
    // RGB Lighting page, Direct Effects tab (the second sub-view).
    capture("lighting", lighting_device_state(), |a| {
        a.page = Page::Lighting;
        a.lighting_ui.tab = 1;
    });
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn rgb() {
    let state = lighting_device_state();
    let ui = device_ui_on_tab(&state, "argb-strip", TabKind::Lighting);
    capture("rgb", state, move |a| {
        a.page = Page::Device("argb-strip".into());
        a.device_ui = ui;
    });
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn fan() {
    let state = cooling_state();
    let ui = device_ui_on_tab(&state, "aio-001", TabKind::Cooling);
    capture("fan", state, move |a| {
        a.page = Page::Device("aio-001".into());
        a.device_ui = ui;
    });
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn lcd() {
    let state = lcd_state();
    let mut ui = device_ui_on_tab(&state, "kraken-lcd", TabKind::Lcd);
    ui.lcd.media_tab = crate::ui::screens::device::LcdMediaTab::Template;
    ui.lcd.seeded_profile = Some("Gaming".to_string());
    ui.lcd.editor.def = lcd_template();
    ui.lcd.editor.seeded = true;
    ui.lcd.editor.selected.insert("w2".to_string());
    capture("lcd", state, move |a| {
        a.page = Page::Device("kraken-lcd".into());
        a.device_ui = ui;
    });
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn dpi() {
    let state = dpi_state();
    let ui = device_ui_on_tab(&state, "g502", TabKind::Performance);
    capture("dpi", state, move |a| {
        a.page = Page::Device("g502".into());
        a.device_ui = ui;
    });
}

#[test]
#[ignore = "screenshot generator; run with --ignored"]
fn profiles() {
    capture("profiles", profiles_state(), |a| {
        a.page = Page::Profile("Gaming".into())
    });
}
