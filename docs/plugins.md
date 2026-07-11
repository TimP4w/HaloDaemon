<!--
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Device plugins (Lua)

Device plugins let you add support for a new device **without recompiling the
daemon**. A plugin is a single Lua script dropped into the plugins directory;
the daemon loads it at startup, matches it against connected hardware, and drives
it through the same capability system as a native driver.

Plugins expose only the capability *kinds* Halo already knows about (RGB, fan,
sensor, …). The daemon owns the capability taxonomy and the engines that consume
it — a plugin just fills in the device-specific byte formats.

- **Where:** `~/.config/halod/plugins/*.lua` (Linux) or
  `%APPDATA%\halod\plugins\*.lua` (Windows).
- **When:** loaded at daemon start. Press **Scan now** (or use the Plugins
  screen) to re-read the directory without restarting.
- **Managing:** the **Plugins** screen lists every loaded plugin and lets you
  enable/disable each one. A disabled plugin releases its hardware back to the
  native driver (if any).

> **Trust.** A plugin runs inside the (elevated) daemon and can talk to the
> device it matches. The Lua environment is sandboxed — no filesystem, process,
> or native-library access — but you should still only install plugins you
> trust, and they are matched narrowly by USB vendor/product id.

## Anatomy of a plugin

A plugin script `return`s a single table: a declarative part (which hardware,
what it is, which capabilities) plus callback functions that turn capability
calls into transport bytes.

```lua
return {
  -- Which hardware this plugin drives (one table, or an array of them).
  match = { transport = "hid", vid = 0x1234, pid = 0x5678 },

  -- Required identity. `author`, `version` and `description` are optional and
  -- surfaced in the Plugins screen.
  identity = {
    vendor = "Acme", model = "K1", name = "Acme K1",
    author = "you@example.com", version = "1.0.0",
    description = "Driver for the Acme K1 keyboard.",
  },

  -- Transport parameters (optional; sensible HID defaults).
  transports = { hid = { report_size = 64, timeout_ms = 1000 } },

  -- Capabilities: presence of a section enables that capability.
  rgb = {
    zones = {
      { id = "ring", name = "Ring", topology = { type = "ring" },
        leds = { {id=0, x=0.5, y=0.0}, {id=1, x=1.0, y=0.5}, --[[ … ]] } },
    },
  },

  -- Callbacks (see below).
  initialize  = function(dev) --[[ … ]] return true end,
  write_frame = function(dev, zone_id, colors) --[[ … ]] end,
  apply       = function(dev, state) --[[ … ]] end,
}
```

### `match`

`match` is either **one** table or an **array** of tables (a plugin can drive
several hardware shapes — e.g. an SMBus DRAM controller *and* a GPU one). The
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

### `identity`

| field         | type   | meaning                                             |
|---------------|--------|-----------------------------------------------------|
| `vendor`      | string | required                                            |
| `model`       | string | required                                            |
| `name`        | string | display name (defaults to `model`)                  |
| `id`          | string | stable id prefix (defaults to the script file stem) |
| `author`      | string | plugin author, shown in the Plugins screen          |
| `version`     | string | plugin version, e.g. `"1.2.0"`                      |
| `description` | string | free-text summary, shown in the Plugins screen      |

### Capability sections

Include a section to advertise that capability:

- `rgb = { zones = { … }, native_effects = { … } }` — see [RGB](#rgb).
- `fan = { channel = <u8> }` — a controllable fan/pump channel.
- `sensor = {}` — the device reports sensor readings (via `get_sensors`).
- `lcd = { needs_rgb_restore = <bool> }` — the device has an image panel; see [LCD](#lcd).
- `dpi = { min, max, steps = { … } }` — a pointing device's DPI; see [DPI & choices](#dpi--choices).
- `choice = { choices = { … } }` — discrete selectors (e.g. polling rate); see [DPI & choices](#dpi--choices).
- `poll = { interval_ms = <n> }` — run `read_status` on a background loop.
- `chain = { channels = { … }, accessories = { … } }` — host detachable child
  accessories (fan hubs / ARGB chains); see [Chained accessories](#chained-accessories).

Not a capability, but declarable by any plugin: `config = { fields = { … } }`
— user-editable settings (e.g. a server host/port); see
[Config fields](#config-fields).

## Callbacks

Every callback receives `dev` as its first argument. `dev.transport` is the
device's transport; `dev.status` holds the most recent table returned by
`read_status` (see [polling](#polling)).

| callback                         | capability | returns                       |
|----------------------------------|------------|-------------------------------|
| `initialize(dev)`                | —          | `true` if connected           |
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

A zone declares its LED layout as normalized `0..1` positions (`x`,`y`), used
both for canvas sampling and for the GUI's zone widget. `topology` is one of
`{type="ring"}`, `{type="linear"}`, `{type="grid"}`, `{type="rings", count=N}`.

### Fan

`fan = { channel = 0 }` enables a fan/pump channel. `get_duty`/`set_duty` use
duty `0..=255`; `get_rpm` returns an integer or `nil` (e.g. a pump reporting duty
but not rpm).

### Chained accessories

Some devices host detachable children — e.g. an AIO pump whose accessory port
drives an RGB fan. Declare the channel(s) and the accessories you recognize:

```lua
chain = {
  channels = { { id = "0", name = "Accessory", max_leds = 40 } },
  accessories = {
    { id = 0x13, name = "F120 RGB", led_count = 8, topology = "ring", fan = true },
    { id = 0x1B, name = "F240 RGB Core", led_count = 16, topology = "rings", rings = 2, fan = true },
  },
}
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
Declare `poll = { interval_ms = 500 }` and provide `read_status`; the daemon runs
the loop (never the script — it stays single-threaded) and stores the returned
table in `dev.status` for your other callbacks to read:

```lua
poll = { interval_ms = 500 },
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

`dpi = { min, max, steps = { 800, 1600, 3200 } }` enables a pointing device's DPI
control. The **host owns the step-cycle state** (clamp, index, the current value)
— the plugin only writes the chosen value through one callback:

```lua
dpi = { min = 100, max = 26000, steps = { 800, 1600, 3200 } },
set_dpi = function(dev, dpi) dev.transport:write(dpi_report(dpi)) end,
```

`choice = { choices = { … } }` declares discrete selectors (dropdowns / toggles).
The host caches the selection and calls `set_choice` to apply it:

```lua
choice = { choices = {
  { key = "poll_rate", label = "Polling Rate", category = "Mouse", display = "list",
    options = { { id = "1000 Hz", label = "1000 Hz" }, { id = "500 Hz", label = "500 Hz" } },
    default = 0 },
} },
set_choice = function(dev, key, selected) --[[ apply ]] end,
```

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

`transports = { hid = { report_size, feature_report, timeout_ms } }` configures
the stream. `report_size` sets the padding target (a leading `0x00` report id is
prepended and the payload padded to `report_size`); set **`report_size = 0` for
raw passthrough** — no report id, no padding — when the script builds the exact
wire buffer itself (e.g. the Razer 90-byte feature report). `feature_report = true`
routes `:write` through `send_feature_report`.

### Stream transport (TCP)

Same `:write`/`:read`/`:write_then_read` shape as HID, over a plain TCP
connection (see the [TCP transport](transports/tcp.md)) — but `:read(n)` is
**exact**: it returns exactly `n` bytes or errors (timeout / connection
closed), never a short read, since a byte stream has no report framing to
fall back on. Only [integration plugins](#integration-plugins) can declare
this transport today.

`transports = { tcp = { host_key, port_key, timeout_ms } }` — `host_key`/
`port_key` (default `"host"`/`"port"`) name which of the plugin's own
[config fields](#config-fields) hold the address to connect to, so the same
values the user edits in the Plugins screen are what the transport connects
with; `timeout_ms` (default `5000`) bounds the connect attempt and every
subsequent read/write.

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

**`pre_scan(dev)`** (optional, SMBus): a top-level callback run once per matching
bus *before* the host probes addresses. Use it for bus preparation whose control
flow depends on live reads (e.g. an ENE DRAM broadcast remap). It drives the same
`dev.transport:batch(fn)` API, scoped to `addresses` + `extra_addresses`.

### USB control transport (`usb_control`)

For USB vendor control transfers (DDC/CI over a hub controller, ENE RGB
controllers, …). Matched by `vid` + `pid` on a `UsbNonHid` device:

```lua
match = { transport = "usb_control", vid = 0x2109, pid = 0x8884 },
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

```lua
transports = {
  usb_control = {
    interface = 0,
    endpoints = { { id = "ambiglow", vid = 0x0CF2, pid = 0xB201, interface = 0 } },
  },
},
-- …then, in a callback:
dev.transport:control_write("ambiglow", 0x40, 0x80, 0x00, 0xE100, frame)
```

Control transfers have no framing/rate helper of their own; a protocol that needs
timed gaps between transfers (DDC/CI's inter-write gap and read delay) drives them
with **`halod.sleep_ms(ms)`** — a blocking sleep on the device's own worker thread,
so it only serializes that device's queued commands. See the built-in
`philips_evnia` plugin for a full worked example.

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
the row-batched fill in the built-in
[`halo_effects.lua`](../src/daemon/src/drivers/plugins/builtins/halo_effects.lua)
plugin.

## Sandbox

Removed globals: `os`, `io`, `package`, `require`, `dofile`, `loadfile`, `load`,
`debug`, `collectgarbage`. Available: `string`, `table`, `math` (incl. Lua 5.4
bitwise ops and `string.pack`), plus `log(msg)`, `halod.buffer`,
`halod.sleep_ms(ms)` (a blocking sleep on the device's own worker thread, for
protocol inter-transfer gaps; capped at 5 s per call), and `halod.config` (see
[Config fields](#config-fields)).

## Config fields

Any plugin — device, effect, or integration — can *declare* user-editable
settings, but only an integration plugin's fields are ever actually editable
in the GUI: they show up on the dedicated **Integrations** screen (see
[Integration plugins](#integration-plugins)). The Plugins screen lists every
plugin and governs whether its Lua may run at all, but never shows a config
editor — a device or effect plugin declaring `config.fields` has nowhere to
edit them today.

```lua
config = {
  fields = {
    { key = "host", label = "Server host", kind = "text", default = "127.0.0.1" },
    { key = "port", label = "Server port", kind = "number", default = "6742" },
    { key = "token", label = "API token", secure = true },
  },
},
```

| field     | type   | meaning                                                        |
|-----------|--------|-----------------------------------------------------------------|
| `key`     | string | required; the name callbacks read it back by                    |
| `label`   | string | required; shown in the GUI                                      |
| `kind`    | string | `"text"` (default) or `"number"` — a display/validation hint only, the value is still read as a string |
| `default` | string | value shown before the user sets one                            |
| `category`| string | groups fields under a heading in the GUI                        |
| `secure`  | bool   | see [Secure fields](#secure-fields) below                        |

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
local hardware — the built-in [OpenRGB](protocols/openrgb.md) client is the
reference example. Set `type = "integration"` and declare no `match` at all:
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

```lua
type = "integration",
permissions = { "network", "os" }, -- `os` only if you need to throttle sends, see below
config = {
  fields = {
    { key = "host", label = "Server host", kind = "text", default = "127.0.0.1" },
    { key = "port", label = "Server port", kind = "number", default = "6742" },
  },
},
transports = { tcp = { host_key = "host", port_key = "port" } },

enumerate_controllers = function(dev)
  return {
    { index = 0, name = "Keyboard", zones = {
        { id = "0", name = "Main", topology = "linear", led_count = 20 },
    } },
  }
end,

write_controller_frame = function(dev, index, zone, colors)
  -- `zone` is the zone's `id` string as declared above.
end,
```

Two callbacks replace the device capability sections a `Device`-type plugin
would use:

- **`enumerate_controllers(dev) -> controllers`** — called once per
  discovery pass. Returns an array of `{ index, name, zones }`; each becomes
  a separate top-level device (not nested under the integration plugin), so
  they show up in the device list exactly like any native device. Each zone
  needs `id`, `name`, `topology` (`"ring"`/`"linear"`/`"grid"`/`"rings"`), and
  `led_count` — the same shape [chained accessories](#chained-accessories)
  use, since both build an `RgbDescriptor` the same way. **`zone.id` should
  be whatever value `write_controller_frame` needs to address that zone** —
  for OpenRGB that's the zone's ordinal index as a string, since the wire
  protocol addresses zones by position, not name.
- **`write_controller_frame(dev, index, zone, colors)`** — an RGB frame for
  one controller's zone. `index` is the controller's index from
  `enumerate_controllers`; `zone` is that zone's `id`; `colors` is an array of
  `{r, g, b}`. There is one shared `dev.transport` for the whole integration
  (one connection), addressed per-call by `index`/`zone` — unlike a `Device`
  plugin's `write_frame`, which is called on a transport already scoped to
  one physical device.

There's no reconnect/hotplug monitor for a dropped network connection today —
if the server restarts, use the Integrations screen's enable toggle (off then
on) to reconnect just this integration.

The wire protocol itself typically gives no acknowledgement of when a sent
frame is actually applied — the server may queue and process frames on its
own schedule, so pushing them faster than it can drain that queue makes the
visible output lag further and further behind. If that's the case for your
target service, throttle `write_controller_frame` client-side (drop a send
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

```lua
permissions = { "network", "os" },
```

Known permissions: `network` (required to open a [`tcp` transport](#stream-transport-tcp)
— gates whether a config-instantiated [integration plugin](#integration-plugins)
is ever connected at all), `os` (re-enables the read-only wall clock —
`os.time()`/`os.clock()` — inside the sandbox; every other `os.*` function
stays stripped), `secure_storage` (required to read a `secure` [config
field](#secure-fields)'s decrypted value from `halod.config` — without it the
key is simply absent). A plugin with any declared permission loads but stays
**inert** — discovered, listed in the Plugins screen, but never matched
against hardware (or, for an integration plugin, never connected) — until the
user grants it. Manually importing such a plugin (Add plugin) prompts for
consent immediately; one found by a directory scan instead gets a toast
notification. Revoking a grant reverts the plugin to inert on the next
rediscovery.

**Built-in plugins are auto-granted their own declared permissions** — they
ship inside the trusted daemon binary itself, so the consent step (which
exists to gate untrusted third-party scripts) doesn't apply to them. This is
why the built-in [OpenRGB integration](protocols/openrgb.md) works out of the
box once you set its host/port, with no separate "grant network" click.

## RGB effects

A plugin can also declare RGB effects instead of (or alongside) a device.
An effect-only plugin sets `type = "effect"` and needs no `match` spec — it
never opens a transport and is pure compute, so it needs no permissions
either:

```lua
type = "effect",
effects = {
  { kind = "pixmap", id = "plasma", name = "Plasma", params = { ... } },
  { kind = "direct", id = "comet", name = "Comet", params = { ... } },
},
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
(the manifest — `id`, `type`, identity fields, `entry`, `permissions`,
`devices`, `transports`, optional `logo`/`effects` asset references) plus its
entry Lua file (`main.lua` by default) and an optional `assets/` subdirectory
for the logo/effect thumbnails. `plugin.yaml`'s `id` **must equal the
directory name**. There is no single-file plugin format and nothing is
compiled into the daemon binary.

A plugin's **content hash** (`sha256(plugin.yaml bytes || entry script
bytes)`) is what user consent is pinned to (trust-on-first-use): granting a
plugin's declared permissions records this hash, and editing the script —
even swapping the file on disk after a grant — changes the hash and revokes
consent until the user re-approves. This applies uniformly to every plugin,
including the official repo's — nothing is consent-exempt.

Plugins install from three sources:

- **Local** — a package dropped into `~/.config/halod/plugins/<id>/` (Linux)
  or `%APPDATA%\halod\plugins\<id>\` (Windows), or imported via the Plugins
  screen's "Add plugin" (a folder picker).
- **The official repo** — a git repository the daemon seeds a non-removable
  record for and clones at startup (network failure is logged, not fatal —
  the daemon just has no official plugins until a later successful clone).
  Official plugins go through the same consent flow as any other; the repo
  *record* just can't be removed.
- **Community repos** — any other git repository registered via the
  Plugins screen's "+ Add repository", each cloned under
  `~/.config/halod/plugin_repos/<slug>/` and scanned for a package at its
  root and/or under a `plugins/<id>/` subdirectory.

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
untouched, and (since the content changed) re-requires consent.

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

On Windows, the process that runs plugin code is elevated (Administrator),
since some native transports (chipset/GPU SMBus, SuperIO fans) need that —
plugins run with the same privilege the daemon itself does, there's no
separate sandboxing by privilege level today.
