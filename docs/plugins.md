<!--
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Device plugins (Lua)

Device plugins let you add support for a new device **without recompiling the
daemon**. A plugin is a directory package containing a declarative
`plugin.yaml` and a Lua callback module. The daemon loads the YAML at startup,
matches it against connected hardware, and drives it through the same capability
system as a native driver. Lua is not compiled or executed while loading the
manifest. Device, integration, effect, and SMBus `pre_scan` workers start only
when the plugin activates (after consent when it declares permissions).

Plugins expose only the capability *kinds* Halo already knows about (RGB, fan,
sensor, …). The daemon owns the capability taxonomy and the engines that consume
it — a plugin just fills in the device-specific byte formats.

- **Where:** `~/.config/halod/plugins/<id>/` (Linux) or
  `%APPDATA%\halod\plugins\<id>\` (Windows).
- **When:** loaded at daemon start. Press **Scan now** (or use the Plugins
  screen) to re-read the directory without restarting.
- **Managing:** the **Plugins** screen lists every loaded plugin and lets you
  enable/disable each one. A disabled plugin releases its hardware back to the
  native driver (if any).

> **Trust.** A plugin runs inside the daemon and can talk to the
> device it matches. The Lua environment is sandboxed — no filesystem, process,
> or native-library access — but you should still only install plugins you
> trust, and they are matched narrowly by USB vendor/product id.

## Anatomy of a plugin

A package separates declarations from code:

- `plugin.yaml` contains identity, matching, permissions, transports,
  capabilities, configuration fields, and effects.
- `main.lua` (or the file selected by `entry`) returns only callback functions
  that turn capability calls into transport bytes.

```yaml
id: acme-k1
compatibility:
  halod: ">=0.2.0"
  plugin_api: 1
name: Acme K1
author: you@example.com
version: 1.0.0
description: Driver for the Acme K1 keyboard.
devices:
  - transport: hid
    vendor: Acme
    model: K1
    vid: 4660
    pid: 22136
transports:
  hid:
    report_size: 64
    timeout_ms: 1000
rgb:
  zones: []
```

```lua
return {
  initialize  = function(dev) --[[ … ]] return true end,
  write_frame = function(dev, zone_id, colors) --[[ … ]] end,
  apply       = function(dev, state) --[[ … ]] end,
}
```

### Device matching (`devices`)

`devices` is an array of device tables (a plugin can drive several hardware
shapes — e.g. an SMBus DRAM controller *and* a GPU one). The
`transport` field selects the backend; each backend requires its own fields.

**HID** (`transport = "hid"`):

| field         | type            | meaning                                        |
|---------------|-----------------|------------------------------------------------|
| `vid`         | integer         | USB vendor id (required)                        |
| `pid`         | integer         | USB product id (optional — omit to match any)  |
| `pids`        | integer array   | match any of several products (device family); takes precedence over `pid` |
| `usage_page`  | integer         | HID usage page (optional; Windows routing)     |
| `usage`       | integer         | HID usage (optional)                           |
| `interface`   | integer         | USB interface number (optional)                |

**USB control** (`transport = "usb_control"`) — see [USB control transport](#usb-control-transport-usb_control):

| field  | type          | meaning                                              |
|--------|---------------|------------------------------------------------------|
| `vid`  | integer       | USB vendor id (required)                              |
| `pid`  | integer       | USB product id (required, or use `pids`)             |
| `pids` | integer array | match any of several products; takes precedence over `pid` |

**SMBus** (`transport = "smbus"`) — see [Register transport](#register-transport-smbus):

| field               | type          | meaning                                                    |
|---------------------|---------------|------------------------------------------------------------|
| `bus`               | string        | `"chipset"` or `"gpu"` (required)                          |
| `addresses`         | integer array | I2C addresses the host may probe (required; the security boundary) |
| `extra_addresses`   | integer array | extra addresses `pre_scan` may write (e.g. a broadcast addr) |
| `max_bytes_per_sec` | integer       | bus write-rate ceiling applied before scanning             |
| `pre_scan`          | bool          | run the plugin's `pre_scan` before probing this bus        |
| `probe`             | string        | `"quick"` (default), `"read_byte"`, or `"none"`            |
| `pci_match`         | table array   | PCI-identity gate — **required for `bus = "gpu"`** (see below) |

**GPU buses require a `pci_match` gate.** A GPU's I²C segment is shared with the
monitor's DDC/EDID lines, so poking an RGB address on a card the plugin doesn't
recognise can hang the display. A `bus = "gpu"` spec must therefore declare a
`pci_match` list confining the scan to known cards; a GPU spec without one is
rejected at load. Chipset/DRAM specs omit it (empty = ungated). Each entry is:

| field        | type    | meaning                                              |
|--------------|---------|------------------------------------------------------|
| `vendor`     | integer | PCI vendor id (e.g. `0x10DE` NVIDIA); omit = wildcard |
| `device`     | integer | PCI device id; omit = wildcard                        |
| `sub_vendor` | integer | subsystem vendor (e.g. `0x1043` ASUS); omit = wildcard |
| `sub_device` | integer | subsystem device id; omit = wildcard                  |
| `confirmed`  | bool    | `true` = a verified board: emit it with **no probe** at all (the curated-whitelist path); `false`/omitted = confirm with the spec's `probe` first |

The host reads each bus's PCI ids during enumeration and, **before opening the
bus**, keeps only buses matching a `pci_match` entry: no match → the bus is left
untouched; a `confirmed` match → emitted without any probe; any other match →
probed with the declared `probe` (use `"read_byte"`, the gentle confirm). This
gate is enforced in the scanner, so native drivers are held to the same rule.

Any spec may also carry per-device identity overrides — `name` and
`device_type` (`"ram"`, `"gpu"`, `"motherboard"`, …) — so one plugin labels each
matched device correctly.

Omitted optional fields mean "don't care". A plugin **shadows** a native driver
for the same hardware, so this is also how you override a built-in driver.

### Package metadata

These fields live at the top level of `plugin.yaml`; there is no nested
`identity` table in a directory package.

| field | type | meaning |
|-------|------|---------|
| `id` | string | required; must exactly equal the package directory name |
| `compatibility` | table | required; `halod` SemVer requirement plus exact `plugin_api` generation |
| `type` | string | `device` (default), `integration`, or `effect` |
| `name` | string | plugin display name (defaults to `id`) |
| `author` | string | plugin author, shown in the Plugins screen |
| `version` | string | plugin version, e.g. `"1.2.0"` |
| `license` | string | SPDX identifier or other license name |
| `description` | string | free-text summary, shown in the Plugins screen |
| `entry` | string | callback module path inside the package (default `main.lua`) |
| `logo` | string | bare filename under `assets/`; defaults to `logo.png` when present |
| `effect_assets` | array | effect-id-to-thumbnail mappings; see [packaging](#packaging--the-official-repo) |

Hardware `vendor`, `model`, optional display `name`, and `device_type` belong
to each entry in `devices`, not to package metadata.

### Capability sections

Include a section to advertise that capability:

- `rgb: { zones: […], native_effects: […] }` — see [RGB](#rgb).
- `fan: { channel: <u8> }` — a controllable fan/pump channel.
- `sensor: {}` — the device reports sensor readings (via `get_sensors`).
- `lcd: { needs_rgb_restore: <bool> }` — the device has an image panel; see [LCD](#lcd).
- `dpi: { min: …, max: …, steps: […] }` — a pointing device's DPI; see [DPI & choices](#dpi--choices).
- `choice: { choices: […] }` — discrete selectors (e.g. polling rate); see [DPI & choices](#dpi--choices).
- `range: { ranges: […] }` — continuous integer sliders (e.g. lift-off distance); see [Controls](#controls).
- `boolean: { booleans: […] }` — on/off toggles (e.g. angle-snap); see [Controls](#controls).
- `action: { actions: […] }` — fire-and-forget buttons (e.g. calibrate); see [Controls](#controls).
- `battery: {}` — the device reports battery levels (via `get_batteries`).
- `connection: {}` — the device reports a wireless connection state (via `connection_status`).
- `equalizer: {}` — the device has an audio equalizer; see [Equalizer](#equalizer).
- `pairing: {}` — the device pairs wireless children; see [Pairing](#pairing).
- `onboard_profiles: {}` — the device has on-board profile slots; see [Onboard profiles](#onboard-profiles).
- `key_remap: { buttons: […], requires_host_mode: <bool>, default_mappings: […] }`
  — remappable buttons; see [Key remap](#key-remap).
- `poll: { interval_ms: <n> }` — run `read_status` on a background loop.
- `chain: { channels: […], accessories: […] }` — host detachable child
  accessories (fan hubs / ARGB chains); see [Chained accessories](#chained-accessories).

Not a capability, but declarable by any plugin: YAML `config.fields`
— user-editable settings (e.g. a server host/port); see
[Config fields](#config-fields).

## Callbacks

Every callback receives `dev` as its first argument. `dev.transport` is the
device's transport; `dev.status` holds the most recent table returned by
`read_status` (see [polling](#polling)); `dev.match` carries the matched-spec
identity (`dev.match.vid`/`pid`/…); and `dev.audio` creates
[virtual audio sinks](#virtual-audio-sinks-devaudio).

| callback                         | capability | returns                       |
|----------------------------------|------------|-------------------------------|
| `initialize(dev)`                | —          | `true`/`nil` to accept, `false` to reject, or dynamic info table |
| `close(dev)`                     | —          | —                             |
| `write_frame(dev, zone, colors)` | rgb        | —                             |
| `apply(dev, state)`              | rgb        | —                             |
| `get_duty(dev)`                  | fan        | duty `0..=255`                |
| `set_duty(dev, duty)`            | fan        | —                             |
| `get_rpm(dev)`                   | fan        | rpm integer or `nil`          |
| `get_sensors(dev)`               | sensor     | array of sensor tables        |
| `read_status(dev)`              | poll       | a status table → `dev.status` |
| `detect_accessories(dev)`        | chain      | array of `{channel, accessory}` |
| `write_ext_frame(dev, ch, colors)`| chain    | —                             |
| `set_fan_duty(dev, ch, duty)`    | chain (fan)| —                             |
| `fan_rpm`/`fan_duty`/`fan_controllable`| chain (fan) | value for that channel   |
| `lcd_stream_frame(dev, rgba, w, h, rotation, raw, brightness)` | lcd | — |
| `set_image(dev, bytes, rotation)`| lcd        | —                             |
| `lcd_set_brightness(dev, brightness, rotation)` | lcd | —                    |
| `lcd_set_rotation(dev, brightness, degrees)` | lcd | —                       |
| `lcd_reset(dev)`                 | lcd        | —                             |
| `set_dpi(dev, dpi)`              | dpi        | —                             |
| `set_choice(dev, key, selected)` | choice    | —                             |
| `set_range(dev, key, value)`     | range      | —                             |
| `get_booleans(dev)`              | boolean    | array of `{key, value}` tables |
| `set_boolean(dev, key, value)`   | boolean    | —                             |
| `trigger_action(dev, key)`       | action     | —                             |
| `get_batteries(dev)`             | battery    | array of battery tables       |
| `connection_status(dev)`         | connection | a connection table or `nil`   |
| `get_equalizer(dev)`             | equalizer  | an equalizer table            |
| `set_eq_preset(dev, preset)`     | equalizer  | —                             |
| `set_eq_bands(dev, values)`      | equalizer  | —                             |
| `start_pairing(dev, timeout_secs)` | pairing  | —                             |
| `stop_pairing(dev)`              | pairing    | —                             |
| `unpair(dev, slot)`              | pairing    | —                             |
| `pairing_status(dev)`            | pairing    | a pairing-status table        |
| `switch_profile(dev, slot)`      | onboard_profiles | —                       |
| `restore_profile(dev, slot)`     | onboard_profiles | —                       |
| `set_profile_enabled(dev, slot, enabled)` | onboard_profiles | —              |
| `onboard_profiles_status(dev)`   | onboard_profiles | a profiles table        |
| `set_button_mapping(dev, mapping)` | key_remap | —                            |
| `reset_button_mapping(dev, cid)` | key_remap  | —                             |
| `reset_all_button_mappings(dev)` | key_remap  | —                             |
| `key_remap_host_mode(dev)`       | key_remap  | `true` if in the host mode    |

### RGB

`write_frame(dev, zone_id, colors)` is the per-frame path the lighting engine
calls (~20 fps in engine mode). `colors` is an array of `{r, g, b}` tables, one
per LED declared in that zone, in declaration order:

```lua
write_frame = function(dev, zone_id, colors)
  local pkt = halod.buffer(1 + 3 * #colors)
  pkt:set_u8(0, 0x06)                       -- report id / opcode
  for i, c in ipairs(colors) do
    local base = 1 + (i - 1) * 3
    pkt:set_u8(base,     c.r)
    pkt:set_u8(base + 1, c.g)
    pkt:set_u8(base + 2, c.b)
  end
  dev.transport:write(pkt)
end
```

`apply(dev, state)` is the user-driven mode change. `state.mode` is one of
`"static"`, `"per_led"`, `"native_effect"`, `"direct_effect"`, `"engine"`; e.g.
`state.mode == "static"` carries `state.color = {r, g, b}`.

A static YAML zone declares `leds` as `{ id, x, y }` entries with normalized
`0..1` positions, used for canvas sampling and the GUI's zone widget.
`topology` is a tagged YAML table such as `{ type: ring }`, `{ type: linear }`,
`{ type: grid }`, or `{ type: rings, count: 2 }`.

### Fan

`fan: { channel: 0 }` in `plugin.yaml` enables a fan/pump channel. `get_duty`/`set_duty` use
duty `0..=255`; `get_rpm` returns an integer or `nil` (e.g. a pump reporting duty
but not rpm).

### Chained accessories

Some devices host detachable children — e.g. an AIO pump whose accessory port
drives an RGB fan. Declare the channel(s) and the accessories you recognize:

```yaml
chain:
  channels:
    - { id: "0", name: Accessory, max_leds: 40 }
  accessories:
    - { id: 19, name: F120 RGB, led_count: 8, topology: ring, fan: true }
    - { id: 27, name: F240 RGB Core, led_count: 16, topology: rings, rings: 2, fan: true }
```

You provide the probe and the routing; the host owns the child device and the
per-channel frame composition (you never write a child device):

- `detect_accessories(dev)` → array of `{ channel = <int>, accessory = <id> }`.
  The host looks each id up in `accessories` and builds a child.
- `write_ext_frame(dev, channel_id, colors)` — write one channel's composed
  frame (the host has already merged all children on that channel).
- For accessories with `fan = true`: `fan_rpm(dev, ch)`, `fan_duty(dev, ch)`,
  `fan_controllable(dev, ch)`, `set_fan_duty(dev, ch, duty)` — the child's fan
  routes through these. (`ch` is the numeric channel from `detect_accessories`.)

The status poll is paused automatically while `detect_accessories` runs, so its
reads don't race the background poll.


### Polling

Devices that report status usually stream a single report you read periodically.
Declare `poll: { interval_ms: 500 }` in `plugin.yaml` and provide `read_status`
in the Lua entry point; the daemon runs
the loop (never the script — it stays single-threaded) and stores the returned
table in `dev.status` for your other callbacks to read:

```lua
read_status = function(dev)
  local r = halod.buffer(dev.transport:read_nonblocking(64))
  return { liquid_temp = r:get_u8(15), pump_rpm = r:get_u16_le(17) }
end,
get_sensors = function(dev)
  local s = dev.status or {}
  return {
    { id = "liquid", name = "Liquid", value = s.liquid_temp or 0,
      unit = "celsius", sensor_type = "temperature" },
  }
end,
```

### LCD

Declare `lcd = { needs_rgb_restore = <bool> }` (set the flag when an image upload
resets the LEDs, so the host re-applies RGB after). The panel descriptor is
reported **dynamically** by `initialize` — resolution can vary by device variant
(`dev.match.pid`) — as an `lcd` field:

```lua
initialize = function(dev)
  local size = LCD_SIZES[dev.match.pid] or { 320, 320 }
  return { ok = true, lcd = {
    shape = "circle", width = size[1], height = size[2],
    rotations = { 0, 90, 180, 270 },
    image_types = { "image/png", "image/jpeg", "image/gif" },
    latches = true,            -- unchanged frames aren't re-streamed
    brightness = 80, rotation = 0,
  } }
end,
```

The host owns rotation/brightness/mode state and passes them into each callback,
so the script stays stateless about them:

- `lcd_stream_frame(dev, rgba, w, h, rotation, raw, brightness)` — one rendered
  engine frame. `rgba` is a `halod.buffer` of `w*h*4` bytes at native resolution.
- `set_image(dev, bytes, rotation)` — upload a still image or GIF.
- `lcd_set_brightness` / `lcd_set_rotation` / `lcd_reset` — panel config.

Pixel data is far too large to shuffle byte-by-byte in Lua, so the `halod` table
provides host-side codecs (each takes/returns a `halod.buffer`):

| helper | purpose |
|--------|---------|
| `halod.rgba_to_q565(rgba, w, h)` | RGBA8 → Q565 (QOI-style RGB565) file |
| `halod.rgba_to_bgr888(rgba)`     | RGBA8 → raw BGR888 (drops alpha)    |
| `halod.rgba_rotate_square(rgba, size, deg)` | rotate a square RGBA buffer 90°× |
| `halod.image_decode(bytes, w, h)`| decode PNG/JPEG and resize to `w×h` RGBA |
| `halod.gif_resize(bytes, w, h)`  | resize an animated GIF to `w×h`     |

Image bytes that exceed a HID report go over the device's **USB bulk-OUT
endpoint** via `dev.transport:write_bulk(buf)` (the 64-byte HID reports carry only
the control handshake). The bulk endpoint opens lazily on first use.

### DPI & choices

`dpi: { min: 100, max: 26000, steps: [800, 1600, 3200] }` in `plugin.yaml`
enables a pointing device's DPI
control. The **host owns the step-cycle state** (clamp, index, the current value)
— the plugin only writes the chosen value through one callback:

```lua
set_dpi = function(dev, dpi) dev.transport:write(dpi_report(dpi)) end,
```

`choice: { choices: [ … ] }` in `plugin.yaml` declares discrete selectors
(dropdowns / toggles).
The host caches the selection and calls `set_choice` to apply it:

```yaml
choice:
  choices:
    - key: poll_rate
      label: Polling Rate
      category: Mouse
      display: list
      options:
        - { id: 1000 Hz, label: 1000 Hz }
        - { id: 500 Hz, label: 500 Hz }
      default: 0
```

```lua
set_choice = function(dev, key, selected) --[[ apply ]] end,
```

### Controls

Three lightweight control kinds, each keyed by a stable `key`. As with `choice`,
the host caches the last-written value; the plugin only applies it.

- **`range`** — a continuous integer slider clamped to `[min, max]` (the host
  clamps before calling). `set_range(dev, key, value)` applies it.
- **`boolean`** — an on/off toggle. `get_booleans(dev)` returns the live
  `{ {key, value}, … }` (label/category are backfilled from the decl if omitted);
  `set_boolean(dev, key, value)` writes one.
- **`action`** — a fire-and-forget button. `trigger_action(dev, key)` runs it.

```yaml
range:
  ranges:
    - { key: lod, label: Lift-off, min: 1, max: 2, default: 1 }
boolean:
  booleans:
    - { key: snap, label: Angle Snap, category: Mouse }
action:
  actions:
    - { key: calibrate, label: Calibrate }
```

```lua
set_range   = function(dev, key, value) --[[ apply ]] end,
get_booleans = function(dev) return { { key = "snap", value = true } } end,
set_boolean = function(dev, key, value) --[[ apply ]] end,
trigger_action = function(dev, key) --[[ run ]] end,
```

### Battery & connection

`battery = {}` reports one or more battery levels via `get_batteries(dev)`;
`connection = {}` reports a wireless link state via `connection_status(dev)`
(return `nil` when unknown). Both sections are empty markers — all state comes
from the callback.

### Equalizer

`equalizer = {}` advertises an audio equalizer. `get_equalizer(dev)` returns the
current bands/preset; `set_eq_preset(dev, preset)` selects a built-in preset and
`set_eq_bands(dev, values)` writes custom band gains.

### Pairing

`pairing = {}` lets the device pair wireless children. `start_pairing(dev,
timeout_secs)` / `stop_pairing(dev)` bracket a pairing window, `unpair(dev, slot)`
removes a slot, and `pairing_status(dev)` reports current slots.

### Onboard profiles

`onboard_profiles = {}` exposes the device's on-board profile slots.
`switch_profile(dev, slot)` / `restore_profile(dev, slot)` change the active slot,
`set_profile_enabled(dev, slot, enabled)` toggles one, and
`onboard_profiles_status(dev)` reports their state.

### Key remap

`key_remap = { buttons = { … }, requires_host_mode = <bool>, default_mappings = { … } }`
declares the device's remappable buttons (fixed hardware, so declared statically).
The host owns the cached mappings; `set_button_mapping(dev, mapping)` writes one,
`reset_button_mapping(dev, cid)` restores a single button's default and
`reset_all_button_mappings(dev)` restores them all. When `requires_host_mode` is
set, `key_remap_host_mode(dev)` reports whether the device is currently in the
mode remapping needs (the GUI shows a notice when it isn't).

## The transport API (`dev.transport`)

The shape of `dev.transport` depends on the matched `transport`. Write rate
limiting is applied automatically on both — you cannot outrun the hardware.

### Stream transport (HID)

Bytes cross as Lua strings **or** [`halod.buffer`](#the-byte-buffer-halodbuffer)
values; reads return Lua strings.

| method                              | effect                                  |
|-------------------------------------|-----------------------------------------|
| `:write(data)`                      | write a packet                          |
| `:read(n)`                          | blocking read of `n` bytes → string     |
| `:read_nonblocking(n)`              | non-blocking read → string              |
| `:write_then_read(data, n)`         | write then read → string                |
| `:feature_exchange(data, n)`        | HID feature report exchange → string    |
| `:write_many({p1, p2, …})`          | write several packets                   |

YAML `transports.hid` (`report_size`, `feature_report`, `timeout_ms`) configures
the stream. `report_size` is either **`0` for raw passthrough** (no report id or
padding) or `1..=1024` as the padding target. With a non-zero size, the transport
adds the platform's report-id framing and pads the payload; use raw mode only
when the script constructs the exact wire frame itself (for example, the ASUS
Aura `0xEC` 65-byte frame). `timeout_ms` is `1..=60000`. `feature_report = true`
routes `:write` through `send_feature_report`.

### Stream transport (TCP)

Same `:write`/`:read`/`:write_then_read` shape as HID, over a plain TCP
connection (see the [TCP transport](transports/tcp.md)) — but `:read(n)` is
**exact**: it returns exactly `n` bytes or errors (timeout / connection
closed), never a short read, since a byte stream has no report framing to
fall back on. Only [integration plugins](#integration-plugins) can declare
this transport today.

YAML `transports.tcp` accepts `host_key`, `port_key`, `timeout_ms`, and
`allow_private`. `host_key`/
`port_key` (default `"host"`/`"port"`) name which of the plugin's own
[config fields](#config-fields) hold the address to connect to, so the same
values the user edits in the Integrations screen are what the transport connects
with; `timeout_ms` (default `5000`) bounds the connect attempt and every
subsequent read/write. By default, resolved loopback, private, link-local, and
other non-routable addresses are rejected as an SSRF defense. Set
`allow_private: true` only for an integration intentionally targeting localhost
or a LAN service.

### Register transport (SMBus)

SMBus is addressed register I/O, not a byte stream. All access goes through
**`dev.transport:batch(fn)`**: `fn` receives an `ops` object and runs entirely
inside **one bus-lock hold**, so a multi-op sequence is atomic and read results
can drive its control flow. `batch` returns whatever `fn` returns.

```lua
local info = dev.transport:batch(function(ops)
  ops:write_word_data(addr, 0x00, reg)      -- set a register pointer
  return ops:read_byte_data(addr, 0x81)     -- read back → branch on it
end)
```

| `ops` method                          | returns                                       |
|---------------------------------------|-----------------------------------------------|
| `:read_byte(addr)`                    | byte, or `nil` on NAK/error                    |
| `:read_byte_data(addr, cmd)`          | byte, or `nil`                                 |
| `:write_quick(addr)`                  | `true` if the address ACKed                    |
| `:write_byte_data(addr, cmd, val)`    | `true` on success                              |
| `:write_word_data(addr, cmd, val)`    | `true` on success                              |
| `:write_block_data(addr, cmd, data)`  | `true` on success (`false` → fall back)        |
| `:supports_block_write()`             | whether the bus supports block writes          |

An op naming an address **outside** the plugin's declared `addresses` (plus
`extra_addresses` during `pre_scan`) raises — the declared set is a hard
boundary, so a script can never free-roam the bus.

**`pre_scan(dev)`** (optional, SMBus): a callback run once per matching bus
*before* the host probes addresses. Use it for bus preparation whose control
flow depends on live reads (e.g. an ENE DRAM broadcast remap). It runs in a
throwaway 64 MiB/instruction-limited VM with a 5-second wall-clock timeout and
drives the same `dev.transport:batch(fn)` API, scoped to `addresses` +
`extra_addresses`. It receives no plugin config. The scanner contributes this
entry only after the plugin reaches `Ready`, and transport injection
independently requires the effective `smbus` grant.

### USB control transport (`usb_control`)

For USB vendor control transfers (DDC/CI over a hub controller, ENE RGB
controllers, …). Matched by `vid` + `pid` on a `UsbNonHid` device:

```yaml
devices:
  - { transport: usb_control, vid: 8457, pid: 34948 }
```

Two methods issue a single blocking control transfer each. The first argument
names the **endpoint** — `""` is the matched (primary) device:

```lua
-- write: (endpoint, bmRequestType, bRequest, wValue, wIndex, data)
dev.transport:control_write("", 0x40, 0xB2, 0x00, 0x00, packet)
-- read: (endpoint, bmRequestType, bRequest, wValue, wIndex, length) → string
local reply = dev.transport:control_read("", 0xC0, 0xA3, 0x00, 0x006F, 32)
```

**Bundling several chips as one device.** A control device may declare
*secondary* endpoints — separate physical USB devices opened by their own
VID/PID — so a plugin can present, say, a monitor's DDC controller and its LED
controller as a single device. Declare them under `transports.usb_control`, then
reach each by its `id`:

```yaml
transports:
  usb_control:
    interface: 0
    endpoints:
      - { id: ambiglow, vid: 3314, pid: 45569, interface: 0 }
```

Then, in a Lua callback:

```lua
dev.transport:control_write("ambiglow", 0x40, 0x80, 0x00, 0xE100, frame)
```

Control transfers have no framing/rate helper of their own; a protocol that needs
timed gaps between transfers (DDC/CI's inter-write gap and read delay) drives them
with **`halod.sleep_ms(ms)`** — a blocking sleep on the device's own worker thread,
so it only serializes that device's queued commands. See the official
[`philips_evnia`](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/philips_evnia)
plugin for a full worked example.

## Virtual audio sinks (`dev.audio`)

A device that mixes multiple audio streams in software — e.g. a headset base
station's **ChatMix** game/chat balance dial — can create virtual audio sinks
bound to its own USB device. `dev.audio:register(name)` creates a
PulseAudio/PipeWire null-sink looped into the device's physical sink and returns
a handle (or `nil` when the device has no physical sink, or the OS can't create
one — e.g. Windows). The handle exposes `:set_volume(pct)` (0–100) and
`:remove()`.

```lua
-- in initialize: create the sinks
media = dev.audio:register("MyHeadset Media")
chat  = dev.audio:register("MyHeadset Chat")

-- when the balance dial moves (parsed in read_status):
if media then media:set_volume(game); chat:set_volume(chat_vol) end
```

Sinks are **host-owned**: the daemon tears every one down when the device
closes (and reclaims any a crashed daemon leaked at next startup), so a plugin
can't leak them — calling `:remove()` yourself is optional. Creation is scoped
to the device's *own* USB id (`dev.match.vid`/`pid`), so a plugin can never open
sinks for hardware it doesn't drive. See the official
[`steelseries_arctis`](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/steelseries_arctis)
plugin for a full ChatMix example.

## The byte buffer (`halod.buffer`)

Building/parsing packets with raw Lua strings is error-prone (1-based indexing,
immutable strings, no bounds checks). `halod.buffer` is a mutable, fixed-length,
**0-based, bounds-checked** byte buffer.

```lua
local b = halod.buffer(8)          -- 8 zero bytes
local b = halod.buffer(reply_str)  -- wrap bytes to parse them

b:set_u8(0, 0x07)
b:set_u16_le(1, 0x1234)            -- also _be, and u32 variants
local x   = b:get_u16_le(1)
local len = #b                     -- or b:len()
local sub = b:slice(1, 2)          -- a new buffer
b:set_bytes(4, string.char(1, 2, 3, 4))  -- write a whole run in one call
dev.transport:write(b)             -- pass a buffer straight to the transport
```

An out-of-range access errors at the call site (not a confusing `nil`
downstream). Lua 5.4's `string.pack`/`string.unpack` and bitwise operators are
also available if you prefer.

`set_bytes(start, str_or_buffer)` matters for hot loops: writing a large
buffer one `set_u8` at a time (e.g. a 400×300 pixmap) pays for one host call
per byte. Build a chunk in pure Lua first (`string.char`/`table.concat`, no
host round-trip) and write it with a single `set_bytes` call instead — see
the row-batched fill in the official
[`halo_effects/main.lua`](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/halo_effects/main.lua)
plugin.

## Sandbox

Manifest discovery creates no Lua VM: the daemon parses and validates only
`plugin.yaml`, then reads the entry file as inert UTF-8 source for hashing and
later activation. Invalid Lua can therefore be discovered and listed; it fails
only after activation, when a worker or SMBus `pre_scan` evaluates the module.

Runtime workers remove `os`, `io`, `package`, `require`, `dofile`, `loadfile`,
`load`, `debug`, and `collectgarbage`. The `os` permission restores a new,
reduced `os` table containing only `os.time()` and `os.clock()`—never process or
filesystem functions. Available standard functionality includes `string`,
`table`, `math`, Lua 5.4 bitwise operators, and `string.pack`/`string.unpack`.
The host adds `log(msg)`, `halod.buffer`, image/effect helpers,
`halod.sleep_ms(ms)` (blocking only the device worker and capped at 5 seconds per
call), and `halod.config` (see [Config fields](#config-fields)).

Each runtime VM has a 64 MiB Lua heap limit and a 50,000,000-instruction budget
reset for each callback. Device callbacks also have a 30-second request
deadline; effect callbacks have a 2-second deadline. These controls bound
accidents and straightforward runaway code, but this is still an in-process
sandbox rather than OS-level isolation: Lua and its native bindings remain
parser/runtime attack surface, and a granted plugin can intentionally operate
its matched device plus any explicitly granted host capability.

## Config fields

Any plugin — device, effect, or integration — can *declare* user-editable
settings, but only an integration plugin's fields are ever actually editable
in the GUI: they show up on the dedicated **Integrations** screen (see
[Integration plugins](#integration-plugins)). The Plugins screen lists every
plugin and governs whether its Lua may run at all, but never shows a config
editor — a device or effect plugin declaring `config.fields` has nowhere to
edit them today.

```yaml
config:
  fields:
    - { key: host, label: Server host, kind: text, default: 127.0.0.1 }
    - { key: port, label: Server port, kind: number, default: "6742" }
    - { key: token, label: API token, secure: true }
```

| field     | type   | meaning                                                        |
|-----------|--------|-----------------------------------------------------------------|
| `key`     | string | required; the name callbacks read it back by                    |
| `label`   | string | required; shown in the GUI                                      |
| `kind`    | string | `"text"` (default) or `"number"` — a display/validation hint only, the value is still read as a string |
| `default` | string | value shown before the user sets one                            |
| `category`| string | groups fields under a heading in the GUI                        |
| `secure`  | bool   | see [Secure fields](#secure-fields) below                        |
| `min` / `max` | number | optional inclusive bounds for `kind: number`; defaults and submitted values must be finite and in range |

Every callback's `dev` argument doesn't carry config — read it from the
sandboxed **`halod.config`** table instead, e.g. `halod.config.host`. It holds
only this plugin's own values (never another plugin's — each plugin's Lua VM
is built with only its own config), pre-filled with each field's `default`
until the user changes it.

### Secure fields

A field with `secure = true` is a **secret** (an API token, a device
password): its value is encrypted at rest — the OS keyring (Windows
Credential Manager / Linux Secret Service) when reachable, falling back to a
machine-local encrypted file otherwise — masked in the GUI, and **never**
sent to the GUI in plaintext (the GUI only ever learns whether a secret is
currently set, not its value). Leaving a secure field's input blank when
saving keeps the existing stored secret; you must type a new value to change
it.

Reading a secure field's value from `halod.config` additionally requires the
plugin to declare (and the user to grant) the **`secure_storage`**
permission — see [Permissions](#permissions). Without that grant the key is
simply absent from `halod.config`, not present-but-empty.

**Threat model, stated plainly:** this protects secrets against casual at-rest
disclosure — config backups, dotfile sync, another user on the same machine.
It does not protect against an attacker who already runs code as you (the
same trust boundary the plugin sandbox itself operates under) or who has your
OS login/keyring unlocked.

## Integration plugins

An integration plugin connects to a **network service** instead of matching
local hardware. Set YAML `type: integration` and declare no `devices`:
the plugin is instantiated from its own [config fields](#config-fields) (a
host/port the user types), not a discovery handle.

Its root device (the thing that connects and enumerates controllers) carries
no capabilities of its own, so it never appears in Home or the device
sidebar — it's shown, enabled/disabled, and configured on the dedicated
**Integrations** screen instead. Only the controllers it enumerates (below)
show up as ordinary top-level devices in the workspace. The Integrations
screen's own enable toggle is independent of the plugin toggle on the
Plugins screen: disabling it there tears down just this integration's root
and the devices it exposes, without touching anything else; saving a config
change (e.g. a new host/port) does the same before reconnecting with the new
values — neither one runs the full device rediscovery.

```yaml
type: integration
permissions: [network, os]
config:
  fields:
    - { key: host, label: Server host, default: 127.0.0.1 }
    - { key: port, label: Server port, kind: number, default: "6742" }
transports:
  tcp: { host_key: host, port_key: port }
```

```lua
return {
enumerate_controllers = function(dev)
  return {
    { index = 0, name = "Keyboard", zones = {
        { id = "0", name = "Main", topology = "linear", led_count = 20 },
    } },
  }
end,

-- One integration root multiplexes every controller, so RGB callbacks receive
-- the controller index explicitly.
write_controller_frame = function(dev, index, zone_id, colors)
  -- zone_id is the zone's declared id; colors is an array of {r, g, b}.
end,
}
```

Integration children are full `LuaDevice` instances. Non-RGB capabilities use
the ordinary device callbacks (`set_duty`, `get_duty`, `get_sensors`, `set_dpi`,
`get_batteries`, and so on), with the controller `index` also available as
`dev.match.index`. RGB is deliberately explicit because one integration root
multiplexes every controller: implement
`write_controller_frame(dev, index, zone_id, colors)` and
`apply_controller(dev, index, state)`, not the ordinary `write_frame`/`apply`
signatures.

- **`enumerate_controllers(dev) -> controllers`** — called once per
  discovery pass. Returns an array of controller tables; each becomes
  a separate top-level device (not nested under the integration plugin), so
  they show up in the device list exactly like any native device.

  Each controller table has:

  | Field | Type | Required | Description |
  |-------|------|----------|-------------|
  | `index` | integer | yes | Controller index (becomes `dev.match.index`). |
  | `name` | string | yes | Display name for the child device. |
  | `zones` | array | see below | RGB-zone topology shorthand (promoted to an `rgb` section when no explicit one is given). |
  | `rgb`, `fan`, `sensor`, `lcd`, `dpi`, `choice`, `range`, `boolean`, `action`, `battery`, `connection`, `equalizer`, `pairing`, `onboard_profiles`, `key_remap`, `chain` | table | no | Per-controller capability sections returned at runtime. Each uses the Lua-table equivalent of the static YAML shape (e.g. `fan = { channel = 0 }`, `sensor = {}`, `lcd = {}`). A controller that declares none of these still gets RGB from the `zones` shorthand. |

  Each zone entry needs `id`, `name`, `topology`
  (`"ring"`/`"linear"`/`"grid"`/`"rings"`), and `led_count` — the same shape
  [chained accessories](#chained-accessories) use.

The integration monitor probes enabled integrations every 5 seconds. It
connects roots that were offline at startup, marks children offline when a
connection drops, retries with a 5/5/10/20/30-second capped backoff, and diffs
controller enumeration so remote additions/removals appear without a full
device rescan. The Integrations screen's enable toggle or a config save still
forces an immediate scoped teardown/reconnect.

The wire protocol itself typically gives no acknowledgement of when a sent
frame is actually applied — the server may queue and process frames on its
own schedule, so pushing them faster than it can drain that queue makes the
visible output lag further and further behind. If that's the case for your
target service, throttle `write_frame` client-side (drop a send
if too little time has passed since the last one actually sent for that
zone, using `os.clock()` — needs the `os` permission) rather than trying to
"cancel" frames already written to the socket, which isn't possible.

## Example

A complete, commented example package — an HID device with an RGB ring, a pump
fan, a liquid-temperature sensor, and a background status poll, exercising
every implemented feature — lives in the official plugin repo rather than
this one; see [Packaging & the official repo](#packaging--the-official-repo)
below.

## Dynamic device info

`initialize(dev)` may return a bare `true`/`false`, or a table with device info
discovered from the hardware:

```lua
initialize = function(dev)
  -- … probe the device …
  return {
    ok = true,
    model = firmware_version,                 -- overrides identity.model
    zones = { { id = "leds", name = "LEDs",   -- dynamic RGB zones (LED count
               topology = "linear", led_count = n } },  -- known only at runtime)
  }
end
```

Returning `false` (or `{ ok = false }`) rejects the device, so a native driver
can still claim it. This is how an SMBus controller reports its firmware string
and per-stick LED count once probed.

## Permissions

A plugin that needs a privileged capability declares it up front:

```yaml
permissions: [network, os]
```

Known permissions:

- `network` — required to open a [`tcp` transport](#stream-transport-tcp); the
  manifest and the actual connect are both gated.
- `os` — restores only `os.time()` and `os.clock()` inside the sandbox.
- `secure_storage` — exposes this plugin's own decrypted `secure` config values
  through `halod.config`; without it those keys are absent.
- `smbus` — required to scan, pre-scan, read, or write SMBus/I²C. Address scope
  is still limited to the manifest's `addresses` and `extra_addresses`.
- `audio_routing` — exposes `dev.audio` so the plugin can create and route
  host-managed virtual audio sinks.

A plugin with any declared permission loads but stays
**inert** — discovered, listed in the Plugins screen, but never matched
against hardware (or, for an integration plugin, never connected) — until the
user grants it. Manually importing such a plugin (Add plugin) prompts for
consent immediately; one found by a directory scan instead gets a toast
notification. Revoking a grant reverts the plugin to inert on the next
rediscovery.

SMBus declarations are contributed to discovery only after the plugin is
enabled, every declared permission is granted, and the acknowledged content
hash still matches. `pre_scan` also checks the effective `smbus` grant at its
transport-injection boundary. PCI gates, address scope, rate limits, VM limits,
and the 5-second timeout remain additional constraints.

Nothing is auto-granted: official, community, local, device, integration, and
effect packages all use the same gate. Permissionless plugins need no consent;
permissioned plugins require every declared permission plus acknowledgement of
their current content hash before any Lua worker is created.

## RGB effects

A plugin can also declare RGB effects instead of (or alongside) a device.
An effect-only plugin sets YAML `type: effect` and needs no `devices` entries — it
never opens a transport and is pure compute, so it needs no permissions
either:

```yaml
type: effect
effects:
  - { kind: pixmap, id: plasma, name: Plasma, params: [] }
  - { kind: direct, id: comet, name: Comet, params: [] }
```

Each entry registers under a namespaced catalog id (`<plugin_id>:<id>`) in
the RGB engine's effect picker, so it can never collide with a built-in
effect or another plugin's. Two kinds:

- **`pixmap`** — fills a shared 400×300 linear-RGBA canvas once per frame;
  every zone using it then samples the canvas at its LED positions. Callback
  `render_<id>(buf, t, dt, params)` mutates `buf` (a `halod.buffer` of
  `halod.canvas_w * halod.canvas_h * 4` bytes) in place; no return value.
- **`direct`** — computes one color per LED directly, once per zone per
  frame. Callback `led_colors_<id>(leds, t, dt, params, sensor) -> colors`
  receives an array of `{p, p_ring, nx, ny}` (chain/spatial coordinates) and
  returns one `{r, g, b}` per LED, linear-light `0..1` (clamped on the host
  side). `sensor` is the live reading for the effect's declared `sensor`-kind
  param (`nil` while unset/unavailable) — the plugin-effect equivalent of a
  native `DirectLedEffect`'s `sensor_id`/`set_sensor_value`. Since a device
  with multiple zones calls `led_colors_<id>` once per zone per tick (all
  sharing the same `t`), an effect that keeps state across calls (a
  smoothed/eased value, a decaying pulse) must guard its update on `t`
  actually advancing, or it will double-update multi-zone devices — see the
  `last_t` guard in `halo_effects.lua`'s `audio_beat`/`audio_level`/
  `sensor_gradient`/`sensor_steps`.

`t`/`dt` are the engine clock/delta, same as native effects. `params` is the
declared param table with the user's current values. `halod.hsv(h, s, v)`
converts to sRGB bytes, and `halod.audio()` returns the latest audio-capture
`SpectrumFrame` as `{level, flux, beat, seq, bands}` (`bands` a 64-entry
0..1 array) — see [Audio capture & media](engines.md#audio-capture--media).
A script that errors, or one whose per-frame instruction budget runs out (a
runaway loop is killed rather than stalling the engine), falls back to a
native default (solid/off) and is disabled for the rest of the session
rather than being retried every frame.

The official plugin repo's `halo_effects` plugin (enable/disable it like any
other plugin in the Plugins screen) is both the reference implementation and
the stock effect library — it ships every pixmap/direct effect except
`screen_sampler` and the effect designer.

## Packaging & the official repo

Every plugin is a **directory package**: a folder containing `plugin.yaml`
(the complete declarative manifest — `id`, `compatibility`, `type`, identity,
permissions, devices, transports, capabilities, config, and effects) plus its
callback-only entry Lua file (`main.lua` by default) and an optional `assets/` subdirectory
for the logo/effect thumbnails. `plugin.yaml`'s `id` **must equal the
directory name**. There is no single-file plugin format and nothing is
compiled into the daemon binary.

### Compatibility

Every manifest must declare both the HaloDaemon release range and the exact
plugin API generation it targets:

```yaml
compatibility:
  halod: ">=0.2.0"
  plugin_api: 1
```

`halod` uses Cargo-style SemVer requirement syntax. Both checks must pass: a
plugin is rejected when the daemon version is outside the declared range or
when `plugin_api` differs from the API generation implemented by the daemon.
The same checks are applied to repository updates, so an incompatible remote
plugin is neither marked as updatable nor checked out by an explicit update.

### Manifest validation and limits

The daemon validates a package when it is loaded, again when configuration is
changed, and again before persisted configuration is handed to a plugin worker.
The GUI may report an error earlier, but it is not the enforcement boundary.

- IDs are non-empty, ASCII-safe identifiers; duplicate device, zone, field,
  effect, parameter, control, and chain IDs are rejected.
- `plugin.yaml` and the selected entry script must each be a regular,
  non-symlink file no larger than 1 MiB. `entry` must remain inside the package.
- Text values may not contain NUL and are bounded (configuration values are at
  most 4096 bytes). Numeric defaults and submitted number fields must be finite
  and satisfy their declared inclusive `min`/`max` bounds.
- HID report sizes are `0` (raw) or `1..=1024`; HID/TCP timeouts are
  `1..=60000` ms; a TCP transport's `host_key` and `port_key` must name declared
  non-secret config fields.
- Poll intervals are `100..=60000` ms. LED counts are capped at 4096 per
  zone/accessory/channel, LCD dimensions at 8192 per side, static zones and
  integration controllers at 256 each.
- Collection limits include 256 devices/effects/control definitions, 128 config
  fields, 64 effect parameters, 64 chain channels, 256 chain accessories, 32
  secondary USB-control endpoints, and 256 key-remap mappings. Invalid values
  are rejected rather than truncated.

Invalid stored plugin configuration is not passed through to Lua: the affected
non-secret field falls back to its manifest default, while an invalid secret is
omitted. Correct integration values in the Integrations screen to make them
available again.

A logo need not be declared: an `assets/logo.png` file is adopted
automatically when `plugin.yaml` omits `logo`. Declare `logo:` explicitly only
to point at a differently-named file.

Effect thumbnails use a separate `effect_assets` list so `effects` remains the
effect declaration list:

```yaml
effect_assets:
  - { id: plasma, thumbnail: plasma.png }
```

Display assets are bounded: any file the daemon serves (logo or effect
thumbnail) must be at most **256 KB**, and a `logo` is additionally held to at
most **512×512 px** and a **2:1** long-to-short side ratio — it's painted into
a small square tile and letterboxed to preserve aspect. A logo that's absent,
undecodable, or out of bounds is dropped at load (the plugin still loads; a
warning is surfaced and the GUI falls back to an initials tile).

A plugin's **content hash** is SHA-256 over the CRLF-normalized
`plugin.yaml` bytes followed by the CRLF-normalized entry-script bytes. For a
permissioned plugin, granting its declared permissions records this hash;
editing either file makes the plugin inert until the user reviews and approves
the new content. Permissionless plugins do not have a consent gate, so their
hash does not block activation. Display assets and `test.lua` are not included
in this hash; changing security-relevant behavior therefore requires changing
`plugin.yaml` or the entry script.

Plugins install from three sources:

- **Local** — a package dropped into `~/.config/halod/plugins/<id>/` (Linux)
  or `%APPDATA%\halod\plugins\<id>\` (Windows), or imported via the Plugins
  screen's "Add plugin" (a folder picker).
- **The official repo** — a git repository the daemon seeds a non-removable
  record for and clones at startup (network failure is logged, not fatal —
  the daemon just has no official plugins until a later successful clone).
  Official plugins use the same permission gate as every other source; the repo
  *record* just can't be removed.
- **Community repos** — any other git repository registered via the
  Plugins screen's "+ Add repository", each cloned under
  `~/.config/halod/plugin_repos/<slug>/` and scanned for a package at its
  root, as sibling package directories directly under its root (the official
  repo's layout: `nzxt_kraken/`, `ene_smbus/`, …), and/or nested under a
  `plugins/<id>/` subdirectory — any combination of the three.

New plugin ids introduced by a user-added community repository start disabled,
including permissionless plugins. The user must explicitly enable each one in
the Plugins screen. This default does not apply to the separately bootstrapped
official repository.

A plugin id is owned by whichever source loads it first — official repo,
then local, then other repos in registration order — so a community repo can
never shadow an existing plugin id; a collision is rejected and surfaced to
the user rather than silently dropped.

**Updates are per-plugin and never automatic.** The daemon compares a
repo-sourced plugin's content hash against its repo's fetched remote tip
(read straight from git's object database, no working-tree checkout) and
flags it in the Plugins screen when they differ — independent of whether the
containing repo as a whole is "behind", since a repo can have unrelated
commits while a given plugin's own files are unchanged. Accepting an update
checks out only that plugin's files, leaving sibling plugins in the same repo
untouched. A permissioned plugin then requires consent for the new content;
a permissionless plugin can activate immediately.

**Testing a package without hardware.** A package may ship a `test.lua`
alongside its `plugin.yaml`, which the daemon can run directly:

```sh
halod plugin-test <package-dir>
```

This drives the package's declared devices through the real Lua worker
against a recording mock transport — no hardware required — and is how the
official plugin repo's own CI validates a driver change. `test.lua` returns
`function(h) … end`; `h:open(spec)` builds a device over a recording
transport (`spec.reads` optionally scripts read replies), and the returned
`dev` exposes `dev:initialize()`, `dev:apply(state)`, `dev:writes()` (the
recorded write log), and `dev:clear()`. `h:assert(cond, msg)` and
`h:assert_eq(a, b, msg)` record pass/fail; the process exits non-zero if any
assertion failed. Today this covers `hid`/`tcp`-transport device plugins —
see [drivers/plugins/plugin_test.rs](../src/daemon/src/drivers/plugins/plugin_test.rs)
if a package needs SMBus/`usb_control` coverage.

## Roadmap

Supported transports: HID (stream), SMBus (register), USB vendor control
(`usb_control`), and TCP (stream, [integration plugins](#integration-plugins)
only). Not yet available to plugins: the USB *bulk* transport beyond what LCD
streaming already exposes via `write_bulk`.

On Windows, plugin code runs inside the non-elevated daemon. Register-bus
operations that need PawnIO are delegated to the narrow `halod-broker` process;
plugin SMBus access remains gated by its declared permission and address scope.
