# OpenRGB SDK Network Protocol

The network protocol OpenRGB's SDK server exposes (default `127.0.0.1:6742`,
"ORGB" on a phone keypad), letting a client enumerate the RGB controllers
OpenRGB itself supports and drive them directly.

**Source:** transcribed directly from OpenRGB's own serialization code —
`RGBController::GetDeviceDescriptionData`/`GetModeDescriptionData`/
`GetZoneDescriptionData` in `RGBController/RGBController.cpp`, and
`NetworkServer::SendReply_ControllerData`/`ProcessRequest_ClientProtocolVersion`
in `NetworkServer.cpp` (both in the
[OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) repository,
GPL-2.0-or-later). Several third-party summaries of this protocol get details
wrong (a version-1+ `vendor` field is easy to miss, since it isn't obvious
from a packet capture alone) — this doc and the client are both checked
against the actual server source, not a paraphrase of it.

---

## Overview

TCP, little-endian throughout. HaloDaemon connects as a client via the
[`tcp` transport](../transports/tcp.md); the implementation is the built-in
Lua plugin
[`drivers/plugins/builtins/openrgb.lua`](../../src/daemon/src/drivers/plugins/builtins/openrgb.lua)
(a [config-instantiated integration plugin](../plugins.md), not a hardware
device — it connects to a host/port the user configures).

**Scope implemented:** connect, name the client, enumerate controllers and
their zones, and drive them in Direct/custom mode (`SetCustomMode` +
`UpdateZoneLEDs`). Mode enumeration/switching beyond that, profiles, and
plugin-to-plugin messages are **not** implemented — see [Fields deliberately
not parsed](#fields-deliberately-not-parsed).

---

## 1. Packet header

Every message starts with a fixed 16-byte header:

| Offset | Size | Field | Value |
|--------|------|-------|-------|
| 0 | 4 | magic | `"ORGB"` (ASCII, not null-terminated) |
| 4 | 4 | `device_idx` | u32 — target controller index (0 for global requests) |
| 8 | 4 | `packet_id` | u32 — message type, see below |
| 12 | 4 | `message_size` | u32 — length of the payload that follows (**not** including this header) |

## 2. Message types used

| ID | Name | Direction |
|----|------|-----------|
| 0 | `REQUEST_CONTROLLER_COUNT` | client request → server u32 reply |
| 1 | `REQUEST_CONTROLLER_DATA` | client request (u32 protocol version) → server `DeviceDescription` reply |
| 40 | `REQUEST_PROTOCOL_VERSION` | client request (u32) → server u32 reply |
| 50 | `SET_CLIENT_NAME` | client → server, no reply |
| 1051 | `RGBCONTROLLER_UPDATEZONELEDS` | client → server, no reply |
| 1100 | `RGBCONTROLLER_SETCUSTOMMODE` | client → server, no reply, empty payload |

(OpenRGB defines many more — server info/flags, detection, profile/plugin/
settings managers, `RESIZEZONE`, `UPDATELEDS`/`UPDATESINGLELED`,
`UPDATEMODE`/`SAVEMODE`/`UPDATEZONEMODE` — none are used by this client.)

## 3. Protocol version negotiation

`OPENRGB_SDK_PROTOCOL_VERSION` is currently `6`; this client requests **`3`**
(`CLIENT_PROTOCOL_VERSION` in the plugin). The server clamps whatever the
client asks for to its own max (`ProcessRequest_ClientProtocolVersion`:
`if requested > server_max: negotiated = server_max`, otherwise `negotiated =
requested`) — so against any current OpenRGB server, negotiated = 3 exactly.

This matters because **several `DeviceDescription`/`ModeDescription`/
`ZoneDescription` fields are conditional on the negotiated version**, and the
server serializes its reply according to *that* per-request value (passed
straight through as the `protocol_version` argument to `Get*DescriptionData`)
— not some fixed "current" schema. This client only ever requests version 3,
so it only needs to parse what's present *at* version 3:

| Field | Present when |
|-------|--------------|
| `vendor` string | version ≥ 1 |
| Mode `value` | version **< 6** |
| Mode `brightness_min`/`brightness_max`/`brightness` | version ≥ 3 |
| Zone segments, zone flags, zone modes/display name | version ≥ 4 / ≥ 5 / ≥ 6 (never present here) |

If `CLIENT_PROTOCOL_VERSION` in the plugin is ever bumped, every field in
this table needs re-checking against the real source above — don't just
extrapolate from a packet capture or a summary.

## 4. Handshake

1. `SET_CLIENT_NAME` — payload is the client name **plus a trailing `\0`**,
   with `message_size = length(name) + 1`. Unlike every other string in this
   protocol there is no separate length-prefix field; the header's
   `message_size` *is* the length. No reply.
2. `REQUEST_PROTOCOL_VERSION` — payload is one u32, `CLIENT_PROTOCOL_VERSION`
   (`3`). Reply payload is one u32, the negotiated version (see above). This
   client doesn't branch on the reply value — it already knows what it asked
   for and what that implies about the reply layout.

## 5. Enumerating controllers

1. `REQUEST_CONTROLLER_COUNT` (empty payload, `device_idx=0`) → reply payload
   is one u32 `count`.
2. For `index` in `0..count`: `REQUEST_CONTROLLER_DATA` with `device_idx =
   index` and a u32 payload (`CLIENT_PROTOCOL_VERSION`) → reply is a
   `DeviceDescription`.

### `DeviceDescription` reply payload (at negotiated version 3)

```
uint32   data_size          -- duplicates header.message_size (see note below)
uint32   type                -- DeviceType enum; not used
String   name
String   vendor              -- version >= 1 (always present here)
String   description
String   version
String   serial
String   location
uint16   num_modes
uint32   active_mode
ModeDescription[num_modes]
uint16   num_zones
ZoneDescription[num_zones]
uint16   num_leds
LEDDescription[num_leds]     -- present on the wire, not parsed (see below)
uint16   num_colors
Color[num_colors]            -- present on the wire, not parsed (see below)
```

A `String` is `uint16 length` followed by exactly `length` bytes. OpenRGB
always writes a trailing `'\0'` and counts it in `length` (`strlen(...) + 1`,
`strcpy` on the server side) — this client doesn't care either way, since it
reads exactly `length` bytes regardless of what's inside them.

`data_size`'s value is a straight duplicate of the packet header's
`message_size` field (`NetworkServer::SendReply_ControllerData` writes the
computed size twice: once into the header, once as the first 4 bytes of the
payload). Every payload that starts with a `data_size` field
(`REQUEST_CONTROLLER_DATA` reply, `UPDATELEDS`, `UPDATEZONELEDS`,
`UPDATEMODE`, `SAVEMODE`) has the same duplication.

### `ModeDescription` (skipped, not parsed)

At negotiated version 3:

```
String   name
uint32   value                -- present: version < 6
uint32   flags                 -- ModeFlags bitset (HasSpeed, HasBrightness, …)
uint32   speed_min
uint32   speed_max
uint32   brightness_min        -- present: version >= 3
uint32   brightness_max        -- present: version >= 3
uint32   colors_min
uint32   colors_max
uint32   speed
uint32   brightness            -- present: version >= 3
uint32   direction
uint32   color_mode
uint16   num_colors
Color[num_colors]
```

12 `uint32` fields total at version 3 (`value` is still present because 3 <
6; `brightness_min`/`brightness_max`/`brightness` are present because 3 ≥
3). This client only needs to skip exactly this many bytes to reach the
zones section — it never interprets a mode's contents.

### `ZoneDescription`

At negotiated version 3 (no segments/flags/modes/display-name — those need
version ≥ 4/5/6/6 respectively):

```
String   name
uint32   type                -- ZoneType: Single=0, Linear=1, Matrix=2 (not used)
uint32   leds_min
uint32   leds_max
uint32   leds_count
uint16   matrix_length        -- byte length of the matrix-map block that follows
uint32   matrix_height        -- always present when matrix_length > 0
uint32   matrix_width
uint32[height*width] matrix_values
```

**The matrix-map block is unconditional on OpenRGB's side** — it always
writes `height`+`width` (8 bytes minimum) even for a non-matrix zone, where
both are `0` and no `matrix_values` follow. So `matrix_length` is, in
practice, never actually `0`; this client just skips whatever byte length it
reports rather than assuming "0 means absent."

Every zone becomes one `RgbZone` with `id` set to its **0-based ordinal
position** (`"0"`, `"1"`, …) — not its name. `UPDATEZONELEDS` addresses a zone
by index, so the id must round-trip back to that index; the human-readable
`name` field is carried separately for display only.

### `LEDDescription` / trailing `Color[]` — present, not parsed

Both sections follow the zones in the reply and are read as part of the same
`message_size`-bounded payload, but this client has no use for per-LED names
or the controller's current color snapshot, so it never walks past the zones
section — the whole payload was already consumed by one `read(dsize)` call,
so there's nothing left to skip.

## 6. Driving LEDs (Direct mode)

1. `RGBCONTROLLER_SETCUSTOMMODE` — empty payload, `device_idx = <controller>`.
   Puts the controller into direct/custom mode so `UPDATEZONELEDS` frames take
   effect instead of whatever built-in mode was previously active. Sent
   **once per controller per connection**, not on every frame — sending it
   repeatedly risks a visible reset/flicker on some controllers, and it isn't
   idempotent-safe to assume otherwise.
2. `RGBCONTROLLER_UPDATEZONELEDS` — `device_idx = <controller>`, payload:

```
uint32   data_size    -- = 4 (this field) + 4 (zone_idx) + 2 (num_colors) + 4*num_colors
uint32   zone_idx
uint16   num_colors
Color[num_colors]
```

### `Color`

```
uint8   red
uint8   green
uint8   blue
uint8   padding = 0
```

Four raw bytes, in that order — not a packed integer requiring endianness
math beyond "write the bytes in this order."

## Fields deliberately not parsed

- `description`/`version`/`serial`/`location` strings — read (to advance the
  parse position correctly) but discarded.
- LEDs and colors sections of `DeviceDescription` — see above.
- Everything mode-related beyond skipping past it: no mode listing, no mode
  switching via `UPDATEMODE`, no `SAVEMODE`.
- Server info/flags, detection, profiles, plugin-to-plugin messages,
  `RESIZEZONE`, `UPDATESINGLELED`.

## Testing

[`drivers/plugins/openrgb_test.rs`](../../src/daemon/src/drivers/plugins/openrgb_test.rs)
drives the real plugin through the real worker + TCP transport against an
in-process fake OpenRGB server built from this same spec, asserting both
directions of the wire format byte-for-byte (including a real `vendor` field
and a full `ModeDescription` entry, not an empty/degenerate reply — a
previous version of this test used `num_modes = 0` and missed the `vendor`
field, which meant it never actually exercised the mode-skipping code path
and passed despite the client misparsing every real OpenRGB server; see the
`controller_data_payload` doc comment).
