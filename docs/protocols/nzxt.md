# NZXT Protocol

NZXT's proprietary USB HID wire protocol for Kraken AIO liquid coolers and the RGB & Fan Control Hub. No public specification exists; documented here strictly from the HaloDaemon driver code.

**Credits:** reverse-engineered from [liquidctl](https://github.com/liquidctl/liquidctl) (GPL-3.0, `kraken3.py`) and the Linux kernel [`nzxt-smart2`](https://github.com/torvalds/linux/blob/master/drivers/hwmon/nzxt-smart2.c) driver (GPL-2.0-or-later).

---

## Overview

Two device families share one base HID wire format: firmware query, accessory detection, and raw report I/O are identical. Device-specific framing diverges as follows.

- **Kraken AIO**. Two wire sub-families, selected by PID:
  - **X3** — **64-byte** HID reports. 8-LED ring; per-channel RGB via `0x22 0x10`/`0x11` + commit `0x22 0xA0`; single pump-head logo LED via `0x2A 0x04`. Pump is on the CPU_OPT header, not USB-controllable.
  - **Z/Elite** — 24-LED ring; combined ring+ext RGB via `0x26 0x14`; USB pump control; LCD panel. Command packets are 64 bytes, except the `0x26 0x14` ring+ext frame, which is a single 124-byte report (it has no sequence nibble and cannot be chunked) sent oversized — the HID framer pads short writes to the report size but never truncates long ones. The LCD bulk path uses a separate USB bulk endpoint (see [Packet layout](#1-packet-layout)).
- **RGB & Fan Control Hub** — 5 fan channels (RPM/duty/type); per-channel ARGB chains via `0x26 0x04` + commit `0x26 0x06`. Fan/init/status writes are 64 bytes, and the `0x26 0x04` lighting frame is a single `4 + 3·LED`-byte packet sent oversized — the HID framer pads short writes up to the report size but never truncates long ones, so long chains aren't cut off.

Each report's first byte is the command ID, the second a sub-command. Replies are matched by prefix: a request `0xNN 0xSS` is acknowledged by `0x(NN+1) 0xSS` (`0x10→0x11`, `0x20→0x21`, `0x30→0x31`, `0x32→0x33`, `0x36→0x37`, `0x38→0x39`). The transport's `read_matching(size, predicate, max_attempts)` drains non-matching reports while searching, up to `max_attempts` reads. HID writes are zero-padded to the report size.

---

## 1. Packet layout

Offsets are 0-based. Unlisted bytes are zero.

**64 bytes** is the HID report size for all commands — RGB, fan/pump duty, init, status, LCD config, ACKs; short writes are zero-padded to 64. The single-packet lighting frames — Z/Elite `0x26 0x14` (124 bytes) and Control Hub `0x26 0x04` (`4 + 3·LED` bytes) — have no sequence nibble and cannot be chunked, so they exceed 64 bytes and are sent as a single oversized report: the HID framer pads short writes up to the report size but never truncates long ones. **512 bytes** is the USB bulk endpoint's max packet size, used *only* for pushing LCD frame pixels (Q565/GIF/BGR888) over a separate bulk-OUT endpoint — the driver hands the bulk transport one big buffer and the USB stack splits it into 512-byte packets.

Every layout below is a **64-byte HID report** except the Z/Elite `0x26 0x14` ring+ext frame and Control Hub `0x26 0x04` frame (which exceed 64 bytes) and the **Kraken LCD bulk header**, which is the header for the **512-byte bulk endpoint** payload.

### Base report header

```
byte 0   command ID
byte 1   sub-command
byte 2…  command-specific payload
```

### Kraken X3 ring/ext LED packet (`0x22 0x10`)

Two data packets + one commit, per channel (`ch` = `0x02` ring, `0x01` ext):

```
data packet n (n = 0,1):
  byte 0   0x22
  byte 1   0x10 | n          → 0x10 then 0x11
  byte 2   ch
  byte 3   0x00
  byte 4…  up to 60 GRB bytes (source chunk n*60 … n*60+60), zero-padded to 64

commit packet:
  0x22 0xA0 ch 0x00 0x01 0x00 0x00 0x28 0x00 0x00 0x80 0x00 0x32 0x00 0x00 0x01
```

### Kraken X3 logo LED packet (`0x2A 0x04`)

```
byte 0      0x2A
byte 1      0x04
bytes 2-6   0x04 0x04 0x00 0x32 0x00
byte 7      G
byte 8      R
byte 9      B
bytes 56-59 0x01 0x00 0x01 0x03
```

### Kraken Z/Elite ring+ext LED packet (`0x26 0x14`)

```
byte 0   0x26
byte 1   0x14
byte 2   channel byte (ring = 0x01, ext = 0x02)
byte 3   channel byte (repeated)
byte 4…  GRB buffer
```

Ring buffer is fixed at **120 bytes** = 40 slots × 3 (GRB); slots 0–23 carry the 24 live ring LEDs, slots 24–39 are zero. The ext buffer length follows the accessory's LED count.

### Kraken fan/pump duty profile packet (`0x72`)

Pump and fan duty are both set with the same 44-byte command — a 4-byte header followed by `profile[40]`, a 40-byte duty table:

```
pump: 0x72 0x01 0x00 0x00  <profile[0]..profile[39]>
fan:  0x72 0x02 0x01 0x01  <profile[0]..profile[39]>
```

**`profile[40]` definition:** 40 bytes. `profile[i]` is the duty cycle — a whole percentage **0–100** — the cooler runs at when the liquid temperature is **(20 + i) °C**. So `profile[0]` is the duty at 20 °C and `profile[39]` the duty at 59 °C (1 °C per step, 20–59 °C inclusive). The device extrapolates the ends: liquid below 20 °C uses `profile[0]`, at or above 59 °C uses `profile[39]`. The whole table is always transmitted — there is no "set one duty value" command. A **fixed** speed is encoded by writing the same value into all 40 bytes (pump values first clamped up to ≥ 20); a **curve** is the user's temperature→duty curve sampled at each of the 40 temperature points.

### Kraken status/sensor packet (`0x75 0x01` / `0x75 0x02`)

```
byte 0      0x75
byte 1      0x01 or 0x02
byte 15     liquid temp, whole °C
byte 16     liquid temp, tenths        → temp = byte15 + byte16/10
byte 17-18  pump RPM   (u16 little-endian)
byte 19     pump duty %
byte 23-24  fan RPM    (u16 little-endian)
byte 25     fan duty %
```

`0xFF 0xFF` at bytes 15-16 is a "no reading" sentinel. Byte 16 is a tenths digit in the range 0–9; some firmware revisions report values above 9, which are clamped to 9 (the packet is still used — discarding it would also lose the RPM readings). Minimum length 26 bytes.

### Kraken LCD config packet (`0x30 0x02`)

```
0x30 0x02 0x01 <brightness> 0x00 0x00 0x01 <rotation_index>
  rotation_index = (degrees / 90) % 4
```

### Kraken LCD bulk header (20 bytes)

Prepended to every bulk-OUT payload on the separate USB bulk endpoint:

```
bytes 0-11  0x12 0xFA 0x01 0xE8 0xAB 0xCD 0xEF 0x98 0x76 0x54 0x32 0x10
byte 12     asset_mode   (0x08 = Q565, 0x09 = raw BGR888)
bytes 13-15 0x00 0x00 0x00
bytes 16-19 payload length, u32 little-endian
```

The GIF/image bucket pipeline uses the same 12-byte magic with an 8-byte tail `[0x01,0x00,0x00,0x00, len_le32]`.

### Control Hub RGB packets (`0x26 0x04` + commit `0x26 0x06`)

```
color packet:  0x26 0x04 <channel_byte> 0x00  + GRB bytes/LED
  channel_byte = 1 << channel
commit packet (64 bytes):
  0x26 0x06 <channel_byte> 0x00 0x01 0x00 0x00 0x18 0x00 0x00 0x80 0x00 0x32 0x00 0x00 0x01
```

### Control Hub fan-speed report (`0x67 0x02`)

```
byte 0       0x67
byte 1       0x02
byte 16+i    fan type of channel i  (0=none, 1=DC, 2=PWM)
byte 24+i*2  RPM of channel i  (u16 little-endian)
byte 40+i    duty % of channel i
```

5 channels, i = 0…4. The `0x61 0x03` fan-config report carries fan types alone at bytes `16+i`.

### Control Hub set-fan-speed packet (`0x62`)

```
11 bytes: 0x62 0x01 <channel_bitmask> <duty[0]> <duty[1]> … <duty[7]>
  channel_bitmask = 1 << channel; only that channel's duty slot is written.
  The wire carries 8 duty slots; only channels 0-4 exist on the hub, so
  duty[5..8] are always zero.
```

---

## 2. Functions

| Function | Bytes sent (exact, `<param>`) | Params | Required sequence / notes |
|----------|-------------------------------|--------|----------------------------|
| **Base** | | | |
| Firmware version | `10 02` | — | Reply `11 02`; version at bytes 0x11-0x13 |
| Accessory detect | `20 03` | — | Reply `21 03`; count @14, IDs @15+ch*6 |
| **Kraken** | | | |
| Initialize | `70 02 01 B8 01`, `70 01`, `10 01` | — | Send in order, then firmware query `10 02` |
| Pump duty | `72 01 00 00` + `<profile[40]>` | profile = 40 duty bytes | Z/Elite only; fixed duty repeats one value ×40 |
| Fan duty | `72 02 01 01` + `<profile[40]>` | profile = 40 duty bytes | Same packet carries an interpolated curve |
| Ring RGB (X3) | `22 10`/`22 11` data + `22 A0` commit, `ch=0x02` | GRB per LED | Two data packets then commit |
| Ext RGB (X3) | `22 10`/`22 11` data + `22 A0` commit, `ch=0x01` | GRB per LED | As above, channel `0x01` |
| Logo LED (X3) | `2A 04 04 04 00 32 00 <G> <R> <B> … 01 00 01 03` | single GRB | X3 pump-head logo only |
| Ring RGB (Z/Elite) | `26 14 01 01` + `<ring_grb[120]>` | 24 LEDs in 40 slots, GRB | See ring co-send note below |
| Ext RGB (Z/Elite) | `26 14 02 02` + `<ext_grb>` | GRB per LED | Sent only when accessory present; see co-send note |
| LCD brightness/rotation | `30 02 01 <br> 00 00 01 <rot>` | br 0-100, rot index 0-3 | `rot = (degrees/90)%4` |
| LCD read state | `30 01` | — | Reply `31 01`; br @0x18, rot @0x1A |
| LCD default display | `38 01 02 00` | — | Reply `39 01`; built-in screen |
| LCD frame upload | multi-step | image/frame | See [LCD frame upload](#lcd-frame-upload) |
| **Control Hub** | | | |
| Detect fans | `60 03` | — | Reply `61 …`; refreshes fan types |
| Set update interval | `60 02 01 E8 <ctl> 01 E8 <ctl>` | ctl byte (see §3) | Enables `0x67 0x02` status pushes |
| Fan speed read | (status push) | — | Parse `67 02`; RPM/duty/type (§1) |
| Set fan duty | `62 01 <mask> <duty[5]>` | mask=1<<ch, duty 0-255 | Only channel `ch`'s slot set |
| RGB frame | `26 04 <ch_byte> 00` + GRB then `26 06 …` commit | GRB per LED | ch_byte = 1<<channel |

### Kraken initialize

Run once at startup, in order, before any other Kraken command:

1. `70 02 01 B8 01` — primary init / report-mode enable; arms the device's status-push reporting.
2. `70 01` — secondary init handshake.
3. `10 01` — request the device begin sending its standard reports.
4. `10 02` — firmware-version query; the reply (`11 02`) carries the version at bytes 0x11/0x12/0x13 and confirms the device is responsive.

### X3 ring / ext RGB write

X-series channels are written as **two 64-byte data packets followed by one commit**. `ch` selects the target: `0x02` = ring, `0x01` = ext accessory. The GRB byte stream (3 bytes/LED, green-red-blue) is split across the two data packets, 60 payload bytes each:

1. Data packet 0: `22 10 <ch> 00` + GRB bytes for LEDs covered by source offset 0…59, zero-padded to 64 bytes.
2. Data packet 1: `22 11 <ch> 00` + GRB bytes from source offset 60…119, zero-padded to 64 bytes. (Header low nibble is `0x10 | packet_number`, so `0x10` then `0x11`.)
3. Commit: `22 A0 <ch> 00 01 00 00 28 00 00 80 00 32 00 00 01` — latches the two data packets onto the channel. Nothing displays until the commit is sent.

Both data packets are always sent even if the LED count fits in one; the second is simply empty payload (all-zero) when there are ≤ 20 LEDs.

### X3 logo LED write

The X-series pump head has one extra logo LED, set with a single 64-byte packet:

```
byte 0      0x2A           command
byte 1      0x04           sub-command
bytes 2-6   0x04 0x04 0x00 0x32 0x00   fixed
byte 7      G              logo colour, green
byte 8      R              logo colour, red
byte 9      B              logo colour, blue
bytes 56-59 0x01 0x00 0x01 0x03        fixed trailer (apply)
```

One colour, GRB order like all NZXT RGB. No separate commit — this packet both sets and applies.

### Accessory detection

`20 03` asks the device which RGB accessories are connected; the reply is `21 03`:

1. **Byte 14** = channel count `N` (driver caps to 8 to guard against malformed packets).
2. For each channel `ch` in `0…N-1`, read **byte `15 + ch*6`** — the first accessory ID of that channel. (Each channel reserves 6 ID slots; only slot 0 is read.)
3. An ID of `0x00` means "nothing on this channel" and is skipped; a non-zero ID identifies the accessory type for that channel.

### Control Hub — set update interval

`60 02 01 E8 <ctl> 01 E8 <ctl>` configures how often the hub pushes `0x67 0x02` status reports, and must be sent to start the push stream. The cadence byte `ctl` (repeated for both halves of the packet) maps to a period:

- `ctl = 0` → **250 ms**.
- `ctl = n` (`n ≥ 1`) → **488 + (n − 1) × 256 ms**.

The driver derives `ctl` from the requested interval: intervals ≤ 250 ms use `0`, otherwise `ctl = (interval_ms − 488) / 256 + 1` (clamped to ≤ 255).

### Control Hub — set fan duty

`62 01 <mask> <duty[0]> … <duty[4]>` (11 bytes total) sets one channel's duty:

1. **Byte 2** `mask` = `1 << channel` — selects which channel(s) this packet applies to.
2. **Bytes 3–7** are the five per-channel duty slots; only the slot for the selected channel (`byte 3 + channel`) is filled, the rest stay `0x00`.
3. `duty` is a **raw 0–255 byte**, not a percentage.

The channel index must be 0–4; out-of-range channels are rejected before the write.

### Z/Elite ring/ext co-send

The firmware requires ring and ext to be written together. The driver caches both GRB buffers; every ring **or** ext write re-emits the cached ring packet (`26 14 01 01 …`), followed by the ext packet (`26 14 02 02 …`) only when an external accessory is connected.

### LCD frame upload

Three paths share the HID control channel (`0x36 …` start/end) plus the USB bulk endpoint (20-byte header + payload). **Every transfer must read its ACK** — see §4.

**A. Q565 live frame** (asset mode `0x08`)

1. `drain_hid_nonblocking` — clear stale ACKs queued from prior frames (>~19 unread desync the firmware).
2. `36 01 00 01 08` (start) → read `37 01` ACK.
3. Bulk-OUT: 20-byte header (asset_mode `0x08`) + Q565 payload, in 2 MB chunks.
4. `36 02` (end) → read `37 02` ACK.

**B. GIF / static image bucket upload** (`run_bucket_pipeline`)

1. `36 03` (prepare).
2. Query buckets: `30 04 i` for i = 0…15, collecting bucket-info replies (`31 04`).
3. Find the lowest unoccupied bucket; `prepare_bucket` deletes buckets walking forward (`32 02 i`) until landing on a free one.
4. Compute memory offset: reuse the slot if the payload fits, else append past the highest occupied region, else wrap to 0, else (no contiguous room) delete all buckets and restart at bucket 0 / offset 0.
5. `setup_bucket`: `32 01 <start> <end> <mem_lo> <mem_hi> <size_lo> <size_hi> 01` (reply `33 01`; any reply = accepted on fw 2.x).
6. `36 01 <bucket_index>` (start) → read `37 01` ACK.
7. Bulk-OUT: 20-byte magic header (8-byte GIF tail) + data, in 2 MB chunks, over the persistent bulk transport (opened once, reused).
8. `36 02` (end) → read `37 02` ACK.
9. `38 01 04 <bucket_index>` — switch display to the new bucket.

**C. Raw BGR888 streaming** (asset mode `0x09`) — requires a one-time handshake.

`enter_streaming_mode(brightness)`, run once; each step writes then reads one ACK:

1. `drain_hid_nonblocking`
2. `10 02`
3. `70 02 01 B8 0B`
4. `74 01`
5. `36 04`
6. `30 01`
7. `36 03`
8. `30 02 00 00 00 00 1E`
9. `38 01 02` — switch to liquid display
10. `32 02 <bi>` for bi = 0…15 — delete all buckets
11. `30 02 01 <pct> 00 00 00 1E` — set brightness (`pct = brightness.min(100)`)
12. `drain_hid_nonblocking`; reset active bucket.

Then per raw frame (`run_stream_frame_raw`):

1. `drain_hid_nonblocking`.
2. Replay LUTs (each `write_then_read`): `stream_lut1()` = `72 01 01 00` + `0x3F`×41 (45 bytes); `stream_lut2()` = `72 02 01 01` + `0x1F`×41.
3. `36 01 00 01 09` (start, asset mode `0x09`) → read `37 01` ACK.
4. Bulk-OUT: 20-byte header (asset_mode `0x09`) + BGR888, in 245 760-byte URB chunks (`STREAM_URB`).
5. `36 02` (end) → read `37 02` ACK.

---

## 3. Parameters

### RGB colour encoding

Every NZXT RGB byte stream — X3 ring/ext/logo, Z/Elite ring/ext, and Control Hub channels — is **GRB**: each LED is three consecutive bytes in **green, red, blue** order. There is no RGB or per-model variation.

### LED counts & channels

| Target | LED count | Wire buffer |
|--------|-----------|-------------|
| X3 ring | 8 | 8 × 3 GRB bytes |
| Z/Elite ring | 24 | serialized into a fixed **40-slot × 3 = 120-byte** buffer; the 24 live LEDs occupy slots 0–23, slots 24–39 are zero-filled |
| Z/Elite ext / X3 ext | accessory-defined | one GRB triple per LED, length follows the connected accessory |
| Control Hub | up to **96 per channel**, **5 channels** (0–4) | channel chosen by the bitmask byte `1 << channel` (channel 0 → `0x01`, channel 4 → `0x10`); the bitmask byte physically admits indices up to 7, but only 0–4 are populated |

### Duty cycles

| Target | Range | Encoding |
|--------|-------|----------|
| Kraken pump | 20–100 % | whole percent; values below 20 are clamped up to 20 (the pump must not stall) |
| Kraken fan | 0–100 % | whole percent |
| Control Hub fan | 0–255 | **raw byte, not a percentage** — written directly into the per-channel duty slot |

Kraken pump/fan duty is delivered through the 40-byte temperature table, not a scalar — see **`profile[40]`** below.

### Temperature→duty profile (`profile[40]`)

The 40-byte table carried by the `0x72` pump/fan packet (defined in [§1](#1-packet-layout)). `profile[i]` is the duty percentage (0–100) applied when the **liquid temperature is (20 + i) °C**, for `i` = 0…39 — i.e. 20 °C … 59 °C inclusive in 1 °C steps. Liquid below 20 °C uses `profile[0]`; at or above 59 °C uses `profile[39]`. A fixed speed = all 40 bytes set to the same value (pump clamped ≥ 20); a curve could be encoded by sampling it at the 40 temperature points, but the driver never does — it always sends flat tables and drives curves host-side (the fan_curve engine re-sends fixed duties as the temperature moves).

### LCD asset modes

The codec of an LCD frame is selected by **byte 12 of the 20-byte bulk header**:

| Mode byte | Codec | Meaning |
|-----------|-------|---------|
| `0x08` | Q565 | compressed RGB565 — the panel's default/native codec |
| `0x09` | BGR888 | uncompressed 24-bit true colour — higher bandwidth, used only by the raw-streaming path |

### LCD brightness & rotation

- **Brightness:** whole percentage **0–100**. Only the streaming handshake clamps values above 100; the plain config packet (`0x30 0x02`) sends the byte as-is.
- **Rotation:** an index **0–3** = `(degrees / 90) mod 4`, i.e. 0 → 0°, 1 → 90°, 2 → 180°, 3 → 270°. The config index orients only the panel's **built-in/default display**; **streamed frames (asset modes `0x08`/`0x09`) are not rotated by firmware** and must be rotated **host-side** before encoding. The host still sends the config index so the default display matches.

### LCD frame memory & resolution

- The panel has **24 320 KB** of image memory, divided into **16 buckets** (indices 0–15) used to store still images.
- Bucket offset and size are expressed in **1 KB (1024-byte) units**, stored **little-endian** in the bucket-info reply: offset at bytes 17–18, size at bytes 19–20.
- Live-frame bulk payload is chunked at **2 MB** for Q565/GIF, and **245 760 bytes per URB** for the raw stream.
- Panels are **square**; the edge length is model-dependent: **240, 320, or 640 px**.

### Q565 frame format

Little-endian throughout: 4-byte ASCII magic **`q565`**, then **width** and **height** as `u16` LE, then the RGB565 pixel stream, terminated by the op byte **`0xFF`** (end-of-frame).

### BGR888 conversion

Each source RGBA8 pixel is written as **`[B, G, R]`** (three bytes, blue first); the alpha byte is discarded.

### Control Hub fan type

Reported per channel: **0 = no fan, 1 = DC, 2 = PWM**. Any non-zero value means the channel is controllable.

### Control Hub poll interval (`ctl` byte)

The status-push cadence is set by the `ctl` byte in the `60 02` command: **`0` → 250 ms**; **`n > 0` → 488 + (n − 1) × 256 ms**. The value is derived from the requested update interval.

---

## 4. Responses

- **Prefix matching:** a request `0xNN 0xSS` is answered by a report whose first two bytes are `0x(NN+1) 0xSS`. `read_matching` discards non-matching reports while searching, up to a per-call attempt limit (8–24).
- **Firmware reply (`11 02`)**: bytes 0x11 / 0x12 / 0x13 = major / minor / patch.
- **Accessory-detect reply (`21 03`)**: channel count at byte 14; first accessory ID of channel `ch` at byte `15 + ch*6` (6 slots/channel, only slot 0 read). ID `0x00` ⇒ channel skipped.
- **LCD bucket-info reply (`31 04`)**: memory offset LE at bytes 17-18, size LE at 19-20; a bucket reading non-zero anywhere from byte 15 is "occupied".
- **LCD transfer ACK (`37 <sub>`)**: a `36 01 …` start emits `37 01`; a `36 02` end emits `37 02`. `await_xfer_ack(sub)` reads (drains) the matching `37 <sub>` via `read_matching`, matching the exact sub-byte so a start-ACK read never swallows an end-ACK. **Unread ACKs desync the panel firmware** — ~19+ unread `37 02` ACKs accumulating cause display artifacts then a crash into the bootloader (commit `6a8be4f`). Hence every frame reads its own ACKs and `drain_hid_nonblocking`s stale ones before starting.
- **Control Hub command replies:** `61` for fan-detect/config; status arrives as unsolicited `67 02` pushes rather than request/response.

---

## 5. Polling & notifications

### Kraken status push (`0x75 0x01` / `0x75 0x02`)

Sent periodically by the device. Wire layout (offsets, see §1): liquid temp = byte 15 (whole °C) + byte 16 / 10 (tenths); pump RPM = bytes 17-18 (u16 LE); pump duty = byte 19; fan RPM = bytes 23-24 (u16 LE); fan duty = byte 25. Bytes 15-16 = `0xFF 0xFF` means no reading.

### Control Hub status push (`0x67 0x02`)

Sent at the interval configured by `60 02 …`. Per channel i (0…4): fan type at byte `16+i` (0=none/1=DC/2=PWM), RPM at bytes `24+i*2` (u16 LE), duty % at byte `40+i`. A separate `0x61 0x03` report carries fan types alone (bytes `16+i`).

---

## Notes

- The Kraken Z/Elite ring uses only 24 of 40 wire slots; slots 24–39 are always zero and have no visible effect.
- X3 pump speed is not USB-controllable (CPU_OPT header); only Z/Elite expose pump duty.
- `read_matching` / ACK reads retry only up to a fixed attempt limit (8–24 by call site); very high unsolicited packet rates can cause a match to time out and return nothing.
- Control Hub ARGB chains are capped at 96 LEDs per channel by firmware.
- LCD bucket setup has no reliable success byte on firmware 2.x — any reply is treated as accepted.
- Unread LCD transfer ACKs crash the panel firmware — every `36 01`/`36 02` must consume its `37` ACK.
