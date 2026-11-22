# Engines

Engines are background loops that drive device state over time. Each engine owns a tick interval (or is event-driven) and broadcasts state changes to subscribers.

---

## Shared run loop

The canvas and LCD engines share one driver, `engine_run_loop_idle` in `application/run_loop.rs`. It owns the master on/off switch, the tick interval, reacting to live config edits, and an **idle gate**: while enabled but with nothing to do, the engine parks on a wake signal instead of ticking (zero renders, zero device I/O) and resumes the instant work appears. An engine implements the `IdleEngine` trait: `tick`, `has_work` (is there anything to do right now?), `wait_for_work` (a future that resolves when work might appear, typically a `tokio::sync::Notify`) and `on_disabled`. Config (enabled flag + tick interval) reaches a running engine through an `EngineConfigReceiver`, a typed subscription to the engine's retained bus record, so a GUI edit takes effect without a restart. The fan-curve engine runs its own interval loop on the same receiver.

Three resting states, with every edge looping back to "read latest config" at the top:

```text
          start
            │     ┌──────────────┐   enabled & has_work     ┌──────────────┐
            └────▶│ read config  │────────────────────────▶ │   RUNNING    │
                  └──────┬───────┘                          │ tick every   │
                         │                                  │ tick_ms      │
           !enabled      │      enabled & !has_work         └──┬────────┬──┘
        ┌────────────────┘──────────────┐                     │        │
        ▼                               ▼          config change   after each tick:
  ┌───────────┐                   ┌───────────┐    (re-read)    if !has_work → IDLE
  │ DISABLED  │                   │   IDLE    │        │              │
  │ await     │                   │ await     │        └──────────────┘
  │ cfg change│                   │ wake OR   │
  └─────┬─────┘                   │ cfg change│   A dropped config sender (daemon
        │ changed                 └─────┬─────┘   shutdown) breaks the loop and the
        └────► back to top              │ woken   task ends cleanly.
                                        └────► back to top
```

The **LCD** engine's `has_work` is "any device has an active template?" and its `wait_for_work` is a `Notify` pinged by `set_template_active()`. The **canvas** engine keeps ticking for 20 s after its last frame that had something to drive (a placed zone, a direct-effect device or a preview subscriber), then parks; it wakes when a subscriber or a canvas edit pings its `Notify`, and probes once a second while parked so a device that appears without a ping is picked up. The `Notify` stores one permit, so a wake in the brief window before an engine parks still takes effect on the next `notified()`: no missed activations.

`tick_ms` is a *target* cadence, not a guarantee of physical write throughput: the daemon's per-device write-rate limiter (see [architecture.md](architecture.md#transport-moving-bytes)) is the actual enforced ceiling underneath it *if* a device has declared one. No device does today, so pushing canvas FPS to 240 reaches the transport at 240fps unthrottled: the limiter exists so a future device (or a misbehaving non-GUI caller) can be capped without touching the engine loop.

---

## Canvas

The canvas engine provides a unified RGB effect system for all placed device zones.

### How it works

A 400×300 tiny-skia `Pixmap` is rendered on each tick (default 50 ms / 20 FPS). Each registered zone has a position and size on the canvas, and the engine samples the rendered pixmap to compute the per-LED RGB values sent to the device.

Zone sampling uses a box filter (radius 3 px) with gamma 2.2 correction. Per-zone LED transforms (flip, reverse, offset) are applied after sampling, before the values are written to the device.

The engine broadcasts a PNG preview and per-LED RGB data on a high-frequency channel: the UI uses this to render the live canvas preview.

### Effects

The engine deliberately supports two effect trust domains:

- **Built-in host effects** are limited to features that require daemon-owned
  services or must exist without an installed package. `screen_sampler` owns
  platform capture handles that are not exposed to the Lua sandbox. The effect
  designer uses the shared typed Rust model in both pixmap and direct modes.
- **Plugin effects** are the extension point for every portable visual effect.
  They are runtime-loaded, permission-scoped, and namespaced by package.

This is a maintained boundary, not a migration fallback: do not add an ordinary
effect to `canvas/effects.rs` or `direct.rs`. Add it to the official
[`halo_effects`](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/halo_effects)
package instead. A new built-in is justified only when it needs a daemon-owned
host capability that the effect sandbox intentionally does not expose.

Only `screen_sampler` and the effect designer's hidden `designer` pixmap are
built-in Rust (`daemon/src/domain/lighting/engine/canvas/effects.rs`). Every other
pixmap effect ships in the official repository's
[`halo_effects`](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/halo_effects)
package; see the [plugin effect API](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/lua-api.md#effect-api).

| Effect | Kind | Description | Parameters |
|--------|------|-------------|------------|
| `screen_sampler` | built-in | Mirror monitor content | Monitor |
| `plasma` | plugin | Animated plasma noise | Speed |
| `rainbow` | plugin | Horizontal hue scroll across the canvas | Speed, scale |
| `random_flash` | plugin | Deterministic per-cell flash, decaying exponentially | Cells, interval, decay, random color, color |
| `audio_spectrum` | plugin | 64-bar spectrum reading `halod.audio()`, bars or solid fill | Low/high color, fill |

The `screen_sampler` effect captures the screen via XDG Desktop Portal (Linux, Wayland) or DXGI (Windows) and blits the result onto the canvas each tick.

### Direct effect: Sensor Gradient

`sensor_gradient` and its sibling `sensor_steps` are direct effects (no pixmap) shipped in the official `halo_effects` package (not built-in Rust). They color a zone from a live sensor reading, delivered each tick as the 5th argument to the effect's `led_colors_<id>` callback (`nil` when the sensor is unset or unavailable): the plugin-effect equivalent of the built-in `DirectLedEffect::sensor_id`/`set_sensor_value` pair used by the `designer` effect below.

`sensor_gradient` picks any sensor via a `Sensor`-kind param (including the synthesized fan readings below), smooths the reading (0–5 s time constant), and blends along an editable list of `value → color` stops (`ParamKind::Gradient` on the wire). Stop values are raw sensor units, and readings outside the stops take the nearest stop's color:

| Mode | Behavior |
|------|----------|
| `gradient` | The whole zone shows one blended color at the current level |
| `meter` | LEDs fill up to the current level by chain position, with the chain spanning the first to the last stop and each LED colored by its own position on the gradient; unlit LEDs are dark |

`sensor_steps` is its sibling for discrete thresholds: the same list of `value → color` stops (`ParamKind::Steps` on the wire), edited in the GUI with the same stop bar, drawn as hard bands instead of one blend. The zone snaps to the color of the highest step the smoothed reading has reached; readings below every threshold take the first step's color.

When the sensor is unset or its reading is unavailable, both effects fade to black rather than freezing on their last color. Like the other direct effects, they apply to one device or many via the same `RgbApply` command and lighting UI; see [Audio capture & media](#audio-capture--media) below for how the engine feeds a live sensor value each tick.

### Fan sensors

Any device with a `FanCapability` gets two synthesized `Sensor` readings: `fan_<id>_duty` (%) and `fan_<id>_rpm` (RPM, omitted when the fan doesn't report one), so fan speed/duty can drive `sensor_gradient` (or any other sensor-consuming effect) exactly like a temperature sensor. These aren't backed by `SensorCapability`; `crate::drivers::fan_sensors()` synthesizes them for both the engine's sensor snapshot and the wire `Sensors` capability list.

---

## Fan Curve

The fan curve engine implements closed-loop temperature-based PWM control.

### How it works

On each tick (default 2 s), the engine reads the configured temperature source, runs it through the pipeline below, and writes the resulting duty to the fan controller.

1. **Stuck sensor check.** When enabled, a reading that has not changed for the configured window marks the curve `SensorMalfunction` and applies the failsafe duty until the value moves again.
2. **Smoothing.** When enabled, the temperature is the mean of the readings within the configured window, so short spikes are damped before the curve sees them.
3. **Hysteresis.** Rising temperatures pass through immediately. A falling temperature is held until it drops more than the configured band below the last value, which stops the duty oscillating around a curve knee when a sensor dithers.
4. **Curve.** The (temperature, duty %) points are linearly interpolated and clamped at both ends.
5. **Ramp.** The duty moves toward the curve value no faster than the configured spin-up or spin-down rate, measured against the time since the previous tick. A limited ramp always moves at least 1 point per tick. The ramp starts from the engine's last accepted write, or from the device-reported duty when there is none.
6. **Deadband.** A write is skipped while the device-reported duty is within 1 percentage point of the curve value, sparing fan bearings constant micro-adjustments.

Constant curves skip steps 1 to 4 and still ramp.

**Failsafe:** if no sensor is assigned, the assigned sensor has been missing for more than 10 s, or it is stuck, the engine writes the configured failsafe duty (75% by default) to avoid thermal runaway.

### Settings

All settings are global, live under `cooling` in `config.yaml`, and appear in Settings → Engines. A value of 0 disables the stage.

| Key | Default | Effect |
|-----|---------|--------|
| `fan_curve_tick_ms` | 2000 | Tick interval, 500 to 60000 ms |
| `fan_failsafe_duty` | 75 | Duty applied when the sensor is missing or stuck |
| `fan_ramp_up_pct_per_s` | 0 | Max duty rise in percentage points per second; 0 is instant |
| `fan_ramp_down_pct_per_s` | 5 | Max duty fall in percentage points per second; 0 is instant |
| `fan_hysteresis_c` | 3 | Falling-temperature band in °C |
| `fan_smoothing_secs` | 0 | Averaging window in seconds |
| `fan_sensor_stale_secs` | 0 | Stuck-sensor window in seconds |

### Preset curves

| Name | Behavior |
|------|----------|
| Balanced | Moderate ramp, quiet at idle |
| Silent | Stays low until high temperatures |
| Performance | Aggressive ramp, prioritizes cooling |
| Full Speed | 100% at all temperatures |
| 50% | Fixed 50% duty |

---

## LCD

The LCD engine renders template images and pushes them to device LCD panels (currently NZXT Kraken models).

### How it works

On each tick (default 50 ms / 20 FPS), the engine calls `LcdTemplate::render(ctx)` which returns an `RgbaImage` and hands it to the device's `stream_frame`. The engine is encoding-agnostic: **the device** decides how the frame reaches the panel (see each device's protocol doc for specifics).

The engine is **only active while a device has a template set**; see the idle gate under [Shared run loop](#shared-run-loop). With nothing on any panel it parks (no rendering, no device I/O) and wakes instantly when a template is activated.

Templates may also carry an optional **background image/GIF** (shared `image` + `dim` params) composited behind their content.

The render context (`TemplateCtx`) provides: canvas dimensions, elapsed time in seconds, frame counter, and live sensor values.

The engine broadcasts a PNG preview and template metadata on a subscription channel for the UI live preview.

### Video mode

For LCD panels, a local video file can be played instead of a template. The **video engine** spawns an `ffmpeg` subprocess that decodes + loops the file into RGBA frames, handing each to the same `stream_frame` path. On Windows a bundled `ffmpeg.exe` shipped beside the daemon is used; otherwise `ffmpeg` is resolved from `PATH`. `ffmpeg` is an optional runtime dependency: when it is absent the daemon reports `ffmpeg_available = false` and the GUI disables video selection.

### Custom template (`custom`)

`custom` is a data-driven template: its `widgets_json` param holds a JSON-encoded `CustomTemplateDef` (`halod-shared::lcd_custom`): a list of `WidgetDef`s (clock, date, sensor, text, image, debug) plus a `ScreenStyle` (accent color, background kind, font, °C/°F). `descriptor().params` is empty on purpose so the generic built-in-template param UI never renders raw JSON; the GUI's LCD editor is the only place that reads/writes `widgets_json`, via the existing `LcdEngineSetTemplate` command: no new IPC surface.

`WireLcdEngineState::device_template_params` (device_id → param map) lets the editor seed itself from whichever custom def is already running on a device, e.g. after a GUI restart.

Background is either the shared `Background` image/GIF compositor (`BgKind::Image`) or one of four procedural fills (`Flow`/`Solid`/`Grid`/`Glow`) tinted by the screen accent. Widgets render in list order via `widget_rect` (normalized-center geometry, clamped to the panel) and `sensor_display` (only converts °C readings to °F, per `ScreenStyle::fahrenheit`).

---

## Key Remap

The key remap engine diverts button events from devices and injects mapped actions into the OS input system.

### How it works

The engine is event-driven. When a device reports a button event (press or release), the engine looks up the configured action for that button and executes it via `/dev/uinput`, a virtual input device that appears to the OS as a regular keyboard or mouse.

A **layer shift** mechanism lets a single button act as a modifier: while held, all mapped keys switch to their alternate action defined in the layer. The layer is global across devices; whenever it engages or releases, every key-remap device is told (`set_layer_shift`), so a device suppresses a button's native output only on the layer where that button is mapped. A button mapped only on the shift layer keeps its native function on the normal layer.

### Device defaults

A device may declare default button mappings (Logitech: the `default_buttons` table in its device profile). These are seeded on first run (when the device has no saved remap config) and restored by the per-button and "Reset all" controls. For example, the G502 X Plus defaults G8 to DPI-up, G7 to DPI-down, and the thumb trigger to a momentary low DPI (sniper). User edits override and persist; clearing a defaulted button back to `native` persists too, so it isn't re-seeded on the next boot.

### Supported action types

| Action | Description |
|--------|-------------|
| `native` | Pass through the button's original function |
| `disable` | Swallow the event (no output) |
| `mouse_button` | Inject a mouse button event |
| `scroll` | Inject a scroll wheel event |
| `key_chord` | Inject a key combination (e.g. Ctrl+C) |
| `media_key` | Inject a media key (play/pause, volume, …) |
| `dpi_cycle` | Cycle through configured DPI steps |
| `profile_cycle` | Cycle through profiles |
| `momentary_dpi` | Temporarily change DPI while held |
| `layer_shift` | Shift all mapped keys to their alternate layer |
| `macro` | Execute a sequence of actions with optional delays |
| `open_app` | Launch an application |
| `command` | Run a shell command |

---

## Audio capture & media

Two sibling shared-handle modules feed audio-reactive RGB effects and the LCD `now_playing` widget: `daemon/src/infrastructure/audio/` and `daemon/src/infrastructure/media/`.

### Lifecycle: consumer-driven singletons

Both modules back a `shared()` function acquired by consumers in `from_params` (an effect's, or `CustomTemplate::from_params` for LCD widgets). Nothing runs at daemon startup, and nothing runs while no audio effect or `now_playing`/audio widget is active anywhere. The two differ in how the backend's lifetime is driven:

- `audio_capture` keeps a process-wide `Arc<AudioHandle>` and tracks *reader activity*, not `Arc` drops: the platform backend exits only after 30 s without a `latest()` call, retries failed sessions in-thread (rate-limited) while readers remain (so it survives an audio-server restart) and `shared()` spawns the backend at most once per 5 s. The rate limit exists because the earlier drop-driven design let effect-rebuild churn reconnect-storm PipeWire until the server exhausted its fd table.
- `media` follows the `ScreenSamplerEffect`/`screen_capture` pattern: a module-level `OnceLock<Mutex<Weak<Handle>>>`; the backend holds a `Weak` and exits when the last consumer's `Arc` drops; a later `shared()` call starts fresh.

### `audio_capture`

Captures the default audio sink's loopback (Linux: PipeWire stream targeting the monitor; Windows: WASAPI shared-mode loopback) and feeds a platform-neutral DSP pipeline (`dsp.rs`, `rustfft`): Hann-windowed FFT, folded into 64 log-spaced bands with per-band attack/release smoothing, RMS level, bass energy-flux beat detection with a refractory period, and a downsampled waveform. Consumers poll the latest `SpectrumFrame` via `AudioHandle::latest()`: latest-only, no ring buffer; scrolling history (spectrum bars, waveform trace) lives in the consuming effect, keyed off `SpectrumFrame::seq`.

Three RGB effects consume it, all shipped in the official `halo_effects` package (not native Rust) via the `halod.audio()` callback helper, which returns the latest `SpectrumFrame` fields (`level`, `flux`, `beat`, `seq`, `bands`):

| Effect | Kind | Behavior |
| --- | --- | --- |
| `audio_beat` | direct | flash + exponential decay on detected beats |
| `audio_level` | direct | brightness follows smoothed RMS, optional level→hue shift |
| `audio_spectrum` | pixmap | 64-bar spectrum, bars or solid fill |

Two LCD widgets (`AudioSpectrum`, `AudioLevel`) read the same handle from `CustomTemplate`, acquired in `from_params` alongside the RGB registries.

### `media`

Watches OS media-session state: MPRIS over D-Bus (`zbus`, modeled on the GNOME Shell active-window observer) on Linux, GSMTC on Windows, and publishes `Option<MediaInfo>` (title/artist/album/status/position/art) via `MediaHandle::latest()`. The Linux watcher is event-driven (`PropertiesChanged`/`Seeked`/`NameOwnerChanged` subscriptions), tracks one `PlayerState` per MPRIS name, and selects the active player (prefer `Playing`, else most-recently-changed, else none); position is interpolated locally between updates via `MediaInfo::position_now()`. Album art resolves off the watcher, `file://` paths and `http(s)://` URLs (4 s timeout, 2 MB cap; streaming players like Spotify only expose remote art), into a small LRU-style cache of decoded, downscaled `Arc<RgbaImage>`s.

The **Windows GSMTC backend polls at 1 Hz** rather than subscribing to session events: a documented follow-up (`// TODO: switch to MediaPropertiesChanged events` in `media/windows.rs`), acceptable because a now-playing widget doesn't need sub-second freshness.

The LCD `NowPlaying` widget consumes it the same way the audio widgets consume `audio_capture`: `CustomTemplate::from_params` acquires `Arc<MediaHandle>` when the widget is present, renders title/artist (marquee-scrolling when the title exceeds `max_chars`) and optional album art, and falls back to a dimmed "—" when no player is active.
