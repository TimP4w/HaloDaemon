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
- `poll = { interval_ms = <n> }` — run `read_status` on a background loop.
- `chain = { channels = { … }, accessories = { … } }` — host detachable child
  accessories (fan hubs / ARGB chains); see [Chained accessories](#chained-accessories).

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

[`plugins/examples/nzxt_kraken.lua`](../plugins/examples/nzxt_kraken.lua) is a
full port of the NZXT Kraken Z: pump RGB, pump fan, liquid-temp sensor, status
poll, and an attached RGB fan as a child — everything but LCD.

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
dev.transport:write(b)             -- pass a buffer straight to the transport
```

An out-of-range access errors at the call site (not a confusing `nil`
downstream). Lua 5.4's `string.pack`/`string.unpack` and bitwise operators are
also available if you prefer.

## Sandbox

Removed globals: `os`, `io`, `package`, `require`, `dofile`, `loadfile`, `load`,
`debug`, `collectgarbage`. Available: `string`, `table`, `math` (incl. Lua 5.4
bitwise ops and `string.pack`), plus `log(msg)` and `halod.buffer`.

## Example

A complete, commented example lives at
[`plugins/examples/example_device.lua`](../plugins/examples/example_device.lua):
an HID device with an RGB ring, a pump fan, a liquid-temperature sensor, and a
background status poll — every implemented feature in one file.

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

## Roadmap

Not yet available to plugins (native drivers still required): LCD panels and the
USB bulk/control transports. HID (stream) and SMBus (register) are supported.
