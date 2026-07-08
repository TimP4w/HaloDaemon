# Philips Ambiglow Protocol

ENE-over-USB-control register protocol for the ENE Technology RGB controller
(ENE KB7730) that drives the Philips Evnia 49 monitor's Ambiglow rear LEDs.

**Credits:** transport reverse-engineered by TimP4w/HaloDaemon from a firmware
capture; the **44-LED full-frame control path** (capture-block enable, `0xE100`
frame buffer, baseline-region restore) is corroborated by and adapted from
[`tomasf/evnia`](https://github.com/tomasf/evnia), an independent macOS
reverse-engineering of the same `0x0CF2:0xB201` controller (MIT).

---

## Overview

The Ambiglow LEDs are a separate USB device from the DDC/CI interface (see
[ddc-ci.md](ddc-ci.md)) and present as an ENE Technology RGB controller accessed
via USB vendor control transfers. The model is a flat **register write space**:
every operation is a single USB vendor control-OUT carrying the register address
in `wIndex` and the value(s) in the data stage. There is no response and no
checksum.

Control-transfer parameters:

| Field | Value |
|-------|-------|
| `bmRequestType` | `0x40` (vendor / host-to-device / device recipient) |
| `bRequest` | `0x80` |
| `wValue` | `0` |
| `wIndex` | Target register address (16-bit) |
| `data` | Payload — register-dependent length |

---

## 1. Control model

The controller drives **44 addressable RGB LEDs**. Driving them is three
register regions:

| Region | `wIndex` | Payload | Purpose |
|--------|----------|---------|---------|
| Control blocks | `0xE020`, `0xE030` | 16-byte *capture* block | Hand direct frame control to the host |
| Frame buffer | `0xE100` | `44 × 3 = 132` RGB bytes | The full LED frame, one LED per triple |
| Baseline region | `0xE020` | 64-byte baseline block | Return control to the monitor firmware |

### Capture block (16 bytes → both `0xE020` and `0xE030`)

```
01 00 02 04 00 05 00 00 00 02 FF 00 00 00 00 01
```

Written to **each** control block to arm host control. HaloDaemon sends it once
(lazily, on the first frame after connect) and tracks the armed state so
high-frequency canvas writes don't re-arm every frame.

### Frame buffer (132 bytes → `0xE100`)

The whole strip is one contiguous write: 44 RGB triples, **`[R, G, B]`** byte
order (capture `FF 00 00` = red), 8 bits each, LED 0 first. A short colour list
leaves the tail black; extra colours are dropped.

### Baseline region (64 bytes → `0xE020`)

```
00 01 02 00 00 05 00 00 00 02 FF 00 00 00 00 00
00 00 02 00 00 05 00 00 00 02 FF 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 FF 00 00 00 00 00
00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00
```

Written on `close()` — and on demand when the user selects the **"Monitor
(firmware control)"** mode in the GUI (exposed as a parameter-less native
effect whose apply triggers the same baseline restore) — to hand the LEDs back
to the monitor's own Ambiglow firmware. The 64-byte block spans the four 16-byte
control sub-blocks at
`0xE020`/`0xE030`/`0xE040`/`0xE050` (the controller auto-increments `wIndex`).

The release waits out a **10 ms frame-settle window** since the last `0xE100`
write before restoring the baseline; restoring while a frame is still settling
makes the controller mis-apply the release.

---

## 2. Functions

| Function | Transfer(s) | Notes |
|----------|-------------|-------|
| Arm capture | `wIndex=0xE020 data=<16-byte block>`, then `wIndex=0xE030 data=<16-byte block>` | Idempotent; sent once per session |
| Write frame | `wIndex=0xE100 data=<132 RGB bytes>` | The full 44-LED frame |
| Release | `wIndex=0xE020 data=<64-byte baseline>` | Return control to firmware |

State mapping (`colors_from_state`): `Static` fills every LED with one colour;
`PerLed` reads the `ambiglow` zone's `index → colour` map (`0..44`); `Engine`
drives no frame here (the canvas engine streams frames via `write_frame`). The
sole `NativeEffect` is the `monitor` release — applying it restores the baseline
instead of writing a frame; any other effect id is ignored. The canvas/per-LED
fast path is a single `0xE100` write after the one-time capture arm.

---

## 3. Parameters

### Zones & LEDs

One zone (`ambiglow`) of **44 LEDs**, exposed only because all 44 are actually
driven via the `0xE100` frame buffer. LED order follows the wire order (LED 0 =
first triple). The zone is `Grid` topology with per-LED `(x, y)` positions
(`ambiglow_positions`) approximating the physical layout documented by
`tomasf/evnia` (front view, `x` left→right, `y` top→bottom), so canvas spatial
effects map to the real geometry rather than to wire order:

| LEDs | Location |
|------|----------|
| `0..3` | Right vertical edge (0 bottom → 3 top-right corner) |
| `4..11` | Top row, right of the center break (corner → center) |
| `12..19` | Top row, left of the center break (center → left corner) |
| `20..23` | Left vertical edge (top-left corner → bottom) |
| `24..34` | Upper center column (above the stand mount) |
| `35..43` | Lower center column (below the mount) |

### RGB byte order

Colour triples are **`[R, G, B]`** — red first. No checksum, no padding.

---

## 4. Responses

OUT-only — there is no readback of LED or register state, and no checksum on any
transfer. The current `tomasf/evnia` reference likewise writes a *hardcoded*
baseline rather than reading one back, so the controller is driven write-only in
both implementations. Success means the USB stack accepted the transfer, not that
the controller applied it; errors are surfaced with the register address and byte
count.

---

## 5. Polling & notifications

None — the protocol is write-only. The host pushes state on apply or per-frame
canvas update; the controller emits nothing and is never polled.

---

## Notes

- Reverse-engineered from a firmware capture (bcdDevice `0x0101`) and
  cross-checked against `tomasf/evnia`; other revisions of the same SKU may
  differ — match VID/PID only.
- The capture/baseline block contents are taken verbatim from captures; their
  individual byte meanings (the `0xFF` bytes are likely per-block brightness/max)
  are not fully decoded.
- Capture must be armed before frames land; HaloDaemon arms lazily on the first
  frame and releases the baseline on disconnect.
