# Corsair NXP Peripheral Protocol

Corsair's proprietary "NXP" (NXP-microcontroller generation) USB HID wire protocol, shared across a large family of RGB keyboards, mice, a mousepad and a headset stand. No public specification exists; this documents the protocol as reverse-engineered by the open-source community.

**Credits:** reverse-engineered from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) (GPL-2.0-or-later, `CorsairPeripheralController`). Every command and sub-command value HaloDaemon sends is taken from there.

This is a **device-agnostic** wire spec. Everything a specific model must supply — its PID, key/zone count, key-position table, and whether it needs the layout-setup step — is a property the **device declares**, not a fact baked into the protocol. Where behaviour varies by device, this document says so and names the declared property that drives it.

---

## Device applicability

All devices are VID `1B1C`; RGB/control lives on the **first vendor HID interface** (Windows `MI_00`), not the boot-keyboard/mouse interface. The protocol predates and is distinct from Corsair's newer **Bragi** protocol (K100, K70 RGB PRO and later) and the older **Legacy** protocol — those are *not* covered here.

**Keyboards** — 24-bit (`07 28`) or 9-bit (`07 27`) submit path, class byte `0x03`:

| Model | PID | Model | PID |
|-------|-----|-------|-----|
| K55 RGB | `1B3D` | K70 RGB RAPIDFIRE | `1B38` |
| K65 RGB | `1B17` | K70 RGB MK.2 | `1B49` |
| K65 LUX RGB | `1B37` | K70 RGB MK.2 SE | `1B6B` |
| K65 RGB RAPIDFIRE | `1B39` | K70 RGB MK.2 Low Profile | `1B55` |
| K68 RGB | `1B4F` | K95 RGB | `1B11` |
| K70 RGB | `1B13` | K95 RGB PLATINUM | `1B2D` |
| K70 LUX RGB | `1B33` | Strafe / Red / MK.2 | `1B20`/`1B44`/`1B48` |

**Mice** — mouse submit path (`07 22`), class byte `0x01`:

| Model | PID | Model | PID |
|-------|-----|-------|-----|
| Glaive RGB / PRO | `1B34`/`1B74` | M65 PRO / RGB Elite | `1B2E`/`1B5A` |
| Harpoon RGB / PRO | `1B3C`/`1B75` | Scimitar PRO RGB | `1B3E` |
| Ironclaw RGB | `1B5D` | Sabre RGB | `1B2F` |

**Mousepad** — MM800 RGB Polaris `1B3B` (class byte `0x04`). **Headset stand** — ST100 RGB `0A34`.

---

## Overview

The device speaks a **64-byte HID report** protocol. Every host→device report is a fixed 64-byte payload zero-padded to length; the transport prepends a leading `0x00` report-ID byte, so the raw write buffer is **65 bytes** (byte 0 = report ID `0x00`, bytes 1–64 = the payload). Offsets in this document are **1-based into the 65-byte buffer** to match the driver code — i.e. byte `[1]` is the command, `[2]` the property, and so on. (Equivalently: byte `[1]` is the first payload byte.)

The first byte of the payload is the **command class**:

| Command | Value | Direction | Reply? |
|---------|-------|-----------|--------|
| `WRITE`  | `0x07` | host → device | no reply |
| `READ`   | `0x0E` | host → device | device replies |
| `STREAM` | `0x7F` | host → device | no reply (bulk color/firmware payload) |

Device → host also uses `0x01` (HID input event) and `0x03` (Corsair-specific event); those are input reports and not driven here.

The second byte is the **property** being written or read. On a `READ`, the device answers with a 64-byte report carrying the requested data at fixed offsets (see [§4](#4-responses)). The first four bytes echo the request, but do not rely on an echo; the reply is read at absolute offsets.

To drive per-LED RGB from software, the host must first take the device out of its onboard hardware-lighting mode (see [§2 Initialization](#initialization)); otherwise the firmware ignores streamed colors.

---

## 1. Packet layout

Offsets are 1-based into the payload (`[1]` = command). Unlisted bytes are zero. Every report is 64 payload bytes zero-padded, written as 65 bytes with a leading `0x00` report ID.

### Base report header

```
byte [1]   command class   (0x07 WRITE / 0x0E READ / 0x7F STREAM)
byte [2]   property        (firmware, lighting-control, submit-color, …)
byte [3…]  property-specific payload
```

### Special-function control (`07 04`)

Selects onboard vs. host control of the special-function keys / lighting engine.

```
07 04 01     → hardware special-function control
07 04 02     → software special-function control
```

### Lighting control (`07 05`)

Switches the lighting engine between onboard effects and host-streamed colors.

```
byte [1]   0x07
byte [2]   0x05   (LIGHTING_CONTROL)
byte [3]   0x01 = HARDWARE, 0x02 = SOFTWARE
byte [4]   0x00
byte [5]   device-class selector: 0x03 keyboard, 0x01 mouse, 0x04 mousepad
```

The class byte in `[5]` is chosen from the connected device's declared class — e.g. a keyboard sends `07 05 02 00 03` to enable software lighting.

### Reset (`07 02`)

> **note:** HaloDaemon does not send `RESET`.

```
07 02 00   medium reset
07 02 01   fast reset
07 02 f0   slow reset
07 02 aa   reboot into bootloader (firmware-update entry — do not send casually)
```

### Firmware query (`0e 01`)

```
0e 01      → device replies with firmware/identification; first 4 bytes echo the request
```

### Submit color — property varies by device class

The submit property selects how streamed pixel data is interpreted; a device uses the one matching its class and firmware:

| Property | Value | Used by |
|----------|-------|---------|
| `SUBMIT_MOUSE_COLOR` | `0x22` | mice (per-zone RGB) |
| `SUBMIT_KBZONES_COLOR_24` | `0x25` | zone-lit keyboards |
| `SUBMIT_KEYBOARD_COLOR_9` | `0x27` | legacy 9-bit per-key keyboards |
| `SUBMIT_KEYBOARD_COLOR_24` | `0x28` | 24-bit per-key keyboards |

**24-bit per-key commit (`07 28`)** — latches a full frame after its data has been streamed with `0x7F` packets (see [§2](#24-bit-color-frame)):

```
byte [1]   0x07
byte [2]   0x28
byte [3]   color channel just streamed (RED=0x01, GREEN=0x02, BLUE=0x03)
byte [4]   number of stream packets that carried this channel (3 for a full keyboard)
byte [5]   finish flag: 1 for the intermediate channels, 2 for the final (blue) commit
```

The finish byte is **1** on the red and green commits and **2** on the blue commit, which latches the whole frame — it is *not* a 0/1 "last channel" flag.

**9-bit per-key (`07 27`)** — legacy 3-bit-per-channel commit, sent after four `0x7F` stream packets (60 + 60 + 60 + 36 = 216 bytes):

```
07 27 00 00 <byte_count>     byte_count at byte [5] = 0xD8 (216)
```

Each 8-bit channel is **clamped** to 0–7 (`v = min(v, 7)`, so anything ≥ 7 saturates — it is *not* a `>>5` scale), then **inverted** (`v = 7 − v`), then packed two keys per byte (low nibble = even key). Devices that report the 24-bit path do not use this.

**Mouse color (`07 22`)** — byte `[3]` is the zone count, then one `<zone_index> RR GG BB` record per zone from byte `[5]`.

### Stream packet (`7f`)

Carries bulk payload (color channels, firmware) that exceeds one 64-byte report.

```
byte [1]   0x7F
byte [2]   NN  packet nonce / sequence index (1,2,3,…)
byte [3]   SS  data length in this packet (≤ 60)
byte [4]   0x00
byte [5…]  up to 60 data bytes
```

### Hardware (onboard) effect mode

Switches the device to a **self-running onboard effect** (only when *not* streaming colors host-side). 

1. `07 05 02 00 00 <brightness>` — lighting-control write carrying brightness at byte `[5]`.
2. `07 17 05 00 <"lght_00.d">` — a config write whose payload `[5..]` is the ASCII string `lght_00.d`.
3. `7f 01 0d 00 <mode> <speed> <color_mode> <direction> …` — a `0x7F` stream packet carrying the effect: `mode` at `[5]` (see the mode table below), `speed` at `[6]` (or `[9]` for Type-Key), `color_mode` flag at `[7]`, `direction` at `[8]`.

---

## 2. Functions

| Function | Bytes sent (`<param>`) | Notes |
|----------|------------------------|-------|
| Firmware query | `0e 01` | READ — device replies, first 4 bytes echo request |
| Software special-function | `07 04 02` | Take special keys off onboard control |
| Hardware special-function | `07 04 01` | Return special keys to onboard |
| Enable software lighting | `07 05 02 00 <class>` | `class` from device (kbd `03`/mouse `01`/pad `04`) |
| Enable hardware lighting | `07 05 01 00 <class>` | Restore onboard effects |
| Stream data | `7f <n> <len> 00` + ≤60 bytes | One channel/chunk of color or firmware |
| Commit per-key 24-bit frame | `07 28 <ch> <npkts> <fin>` | Per channel; `fin` = 1 for R/G, 2 for blue |
| Commit mouse color | `07 22 …` | Per-zone `NN RR GG BB` records |
| Layout setup | `07 05 08 00 01` + 4× `07 40 1e …` | Per-key keyboards; see [Key-layout setup](#key-layout-setup) |
| Reset | `07 02 <mode>` | `00`/`01`/`f0`; `aa` = bootloader |

### Initialization

Run once, in order, before streaming any color:

1. `0e 01` — firmware query; confirms the device is present/responsive and identifies the model.
2. `07 04 02` — software special-function control.
3. `07 05 02 00 <class>` — software lighting control, `<class>` from the device's declared class.
4. **Per-key keyboards** then send the **key-layout setup** (see below). Devices without a declared key table (mice, mousepads, zone-only keyboards) skip this step.

Restore onboard control on shutdown with a single `07 04 01` (hardware special-function); no lighting-control packet is needed.

### Key-layout setup

Per-key keyboards must announce their physical layout before streaming colors, or streamed bytes land on the wrong keys. The burst is a primer followed by four setup packets:

1. **Primer** — `07 05 08 00 01`: a lighting-control packet with sub-mode `0x08` and byte `[5] = 0x01`.
2. **Four `07 40 1e` packets**, each carrying **30** `<key_id> C0` pairs (payload bytes `[5..65]`):

   ```
   byte [1]   0x07
   byte [2]   0x40
   byte [3]   0x1e
   byte [4]   0x00
   byte [5]   key_id_0        byte [6]   0xC0
   byte [7]   key_id_1        byte [8]   0xC0
   …                          (30 pairs)
   ```

`key_id` runs as a monotonically increasing counter across all 120 slots (0, 1, 2, …), **skipping** the identifiers in a physical-layout **skip list**. The skip list *is* the layout selector: omitting a key-id tells the firmware that key is absent. The lists are sorted ascending, so a single ordered pass also clears consecutive runs. For the K70 MK.2:

- **ANSI** (e.g. US): skip `31 3f 41 42 51 53 55 6f 7e 7f 80 81`.
- **ISO** (e.g. Swiss, Italian): skip `3f 41 42 50 53 55 6f 78 79 7a 7b 7c 7d 7e 7f 80 81`.

### 24-bit color frame

A full per-key frame is sent **one color channel at a time** — all slots' red bytes, then all green, then all blue. The wire buffer is indexed by **device key-id**, not LED order: the host scatters each LED's color into `buffer[key_id]` via the model's key table, then streams the buffer. Each channel is streamed then committed:

1. Split the channel's buffer into `0x7F` stream packets of ≤ 60 bytes each. A full keyboard buffer is **144 bytes → three packets** (60 + 60 + 24). Nonce byte `[2]` restarts at 1 per channel.
2. Commit the channel: `07 28 <channel> <packet_count> <finish>`, where `channel` is `0x01`/`0x02`/`0x03` (R/G/B), `packet_count` = 3, and `finish` = 1 for red/green, **2 for blue** to latch the whole frame.

So a full frame is: stream R → `07 28 01 03 01`, stream G → `07 28 02 03 01`, stream B → `07 28 03 03 02`.

---

## 3. Parameters

### Color encoding

Per-key colors are transmitted **channel-planar**, not interleaved: the wire carries every key's **red** byte, then every key's **green**, then every key's **blue** — each channel an 8-bit value `0x00`–`0xFF`. This is the opposite layout of the packed GRB/RGB-per-LED streams used by most other vendors. The 9-bit legacy path instead packs 3 bits per channel, two keys per byte, inverted. Mice stream interleaved `RR GG BB` per zone.

### Key/zone ordering & layout

The mapping from physical key (or mouse zone) to wire index is **declared per model**.
Physical keyboard layout is further selected by a layout byte: ANSI `0x00`, ISO `0x01`, ABNT `0x02`, JIS `0x03`, Dubeolsik `0x04`. The wire color stream is always ordered by the model's declared table regardless of layout; the layout byte only affects which physical keycaps exist.

### Hardware effect modes

The `mode` byte of the onboard-effect stream packet (see [Hardware (onboard) effect mode](#hardware-onboard-effect-mode)). Only relevant when driving onboard effects rather than software streaming:

| Mode | Value | Mode | Value |
|------|-------|------|-------|
| Color Shift | `0x00` | Color Pulse | `0x01` |
| Spiral | `0x02` | Rainbow Wave | `0x03` |
| Color Wave | `0x04` | Visor | `0x05` |
| Rain | `0x06` | Type (Key) | `0x08` |
| Type (Ripple) | `0x09` | Direct | `0xFF` |

- **Speed:** `0x01` (min) – `0x03` (max).
- **Brightness:** `0x00` (min) – `0x03` (max).

### Stream packet limits

Each `0x7F` packet carries ≤ **60** data bytes (payload bytes `[5]`–`[64]`); the length goes in byte `[3]` and the running sequence nonce in byte `[2]`.

---

## 4. Responses

- **READ echo:** a `0e <prop> …` request is answered by a 64-byte report whose **first four bytes repeat the request**, followed by the payload. Match replies by that 4-byte prefix.
- **Firmware reply (`0e 01`):** carries the firmware/identification block; used to confirm model and readiness.
- **WRITE / STREAM:** `0x07` and `0x7F` reports are **fire-and-forget** — the device sends no acknowledgement. Frame pacing is host-driven.

---

## 5. Polling & notifications

The device sends unsolicited input reports — command `0x01` (standard HID key/button events) and `0x03` (Corsair-specific events, e.g. media/macro keys). RGB control does not depend on them; no lighting state is pushed back to the host. These devices expose no sensor/telemetry stream.

---

## Notes

- **Software mode is required per session.** The firmware reverts to onboard lighting on unplug/reset; re-run the [initialization](#initialization) after any reset before streaming colors. (This mirrors the well-known iCUE behavior where a custom profile only applies while software is actively driving the device.)
- **Do not send `07 02 aa`** unless intentionally entering the bootloader for a firmware update — it drops the device off the normal interface.
- **Device-declared, not protocol-baked:** PID, class byte, key/zone count, key-position table, the submit property (`0x22`/`0x25`/`0x27`/`0x28`), and whether the layout-setup step is sent all come from the device descriptor. Shared code branches on those declared values; it must not assume any one model's key count or submit path.
