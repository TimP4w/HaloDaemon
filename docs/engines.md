# Engines

Engines are background loops that drive device state over time. Each engine owns a tick interval (or is event-driven) and broadcasts state changes to subscribers.

---

## Canvas

The canvas engine provides a unified RGB effect system for all placed device zones.

### How it works

A 400×300 tiny-skia `Pixmap` is rendered on each tick (default 50 ms / 20 FPS). Each registered zone has a position and size on the canvas, and the engine samples the rendered pixmap to compute the per-LED RGB values sent to the device.

Zone sampling uses a box filter (radius 3 px) with gamma 2.2 correction. Per-zone LED transforms (flip, reverse, offset) are applied after sampling, before the values are written to the device.

The engine broadcasts a PNG preview and per-LED RGB data on a high-frequency channel — the UI uses this to render the live canvas preview.

### Effects

| Effect | Description | Parameters |
|--------|-------------|------------|
| `static_color` | Solid fill | RGB color |
| `breathing` | Sine-wave brightness pulse | RGB color, speed |
| `rainbow` | Horizontal hue scroll across the canvas | Speed, scale |
| `screen_sampler` | Mirror monitor content | — |

The `screen_sampler` effect captures the screen via XDG Desktop Portal (Linux, Wayland) or DXGI (Windows) and blits the result onto the canvas each tick.

---

## Fan Curve

The fan curve engine implements closed-loop temperature-based PWM control.

### How it works

On each tick (default 2 s), the engine reads the configured temperature source, linearly interpolates the (temperature, duty %) curve defined by the user, and writes the new duty to the fan controller.

A hysteresis guard skips the write if the computed duty changed by 1% or less — this prevents unnecessary wear on fan bearings from constant micro-adjustments.

**Failsafe:** if the temperature sensor is absent or its value has not changed for more than 90 seconds (indicating a stuck/stale sensor), the engine writes 75% duty to avoid thermal runaway.

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

On each tick (default 50 ms / 20 FPS), the engine calls `LcdTemplate::render(ctx)` which returns an `RgbaImage`. The device driver encodes the image in its required format and uploads it:

- **NZXT Kraken:** Q565 encoding, uploaded as a GIF via USB bulk transfer.

The render context (`TemplateCtx`) provides: canvas dimensions, elapsed time in seconds, frame counter, and live sensor values. Templates use these to produce animated or data-driven displays.

The engine broadcasts a PNG preview and template metadata on a subscription channel for the UI live preview.

---

## Key Remap

The key remap engine diverts button events from devices and injects mapped actions into the OS input system.

### How it works

The engine is event-driven. When a device reports a button event (press or release), the engine looks up the configured action for that button and executes it via `/dev/uinput`, a virtual input device that appears to the OS as a regular keyboard or mouse.

A **layer shift** mechanism lets a single button act as a modifier: while held, all mapped keys switch to their alternate action defined in the layer.

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
