# ASUS Aura USB Protocol

ASUS Aura USB HID protocol for motherboard RGB headers, on-board LEDs, and daisy-chained ARGB strips.

**Credits:** reference implementation from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) by Martin Hartl (inlart) and contributors (GPL-2.0-or-later): `AsusAuraUSBController` / `AsusAuraMainboardController`.

---

## Overview

At the protocol level this is an ASUS Aura HID controller addressed with **65-byte raw HID frames**. There is **no report-ID prefix**: byte 0 on the wire is always `0xEC` (the Aura header), byte 1 is the command opcode, and the remaining bytes are zero-padded command payload. The transport is [HID](../transports/hid.md) opened in raw passthrough mode (no report-size translation) with a 1000 ms read timeout.

The model is **host-initiated request/response**. Most commands are fire-and-forget writes. Two commands (firmware, config) write a request frame and then read a reply frame whose byte 1 identifies it. Per-LED color is streamed as a sequence of write-only sub-packets, the last of which carries an "apply" bit. The device never sends unsolicited data.

---

## 1. Packet layout

Offsets are 0-based. Every frame is exactly 65 bytes, zero-initialised, with `buf[0] = 0xEC` and `buf[1] = command`; unlisted bytes are zero.

### Generic command frame

```
byte 0    0xEC                  Aura header
byte 1    command opcode
byte 2…   command-specific payload, zero-padded to 65 bytes
```

### Direct-stream sub-packet — opcode `0x40`

```
byte 0    0xEC
byte 1    0x40                  CMD_DIRECT
byte 2    direct_channel, OR'd with 0x80 if this is the LAST sub-packet
byte 3    LED start offset      first LED index covered by this sub-packet (u8)
byte 4    LED count             number of LEDs in this sub-packet (1…20)
byte 5…   R,G,B triples, 3 bytes per LED:
            byte 5+i*3   = R of LED i
            byte 5+i*3+1 = G of LED i
            byte 5+i*3+2 = B of LED i
```

### SetMode frame — opcode `0x35`

```
byte 0    0xEC
byte 1    0x35                  CMD_SETMODE
byte 2    effect_channel
byte 3    0x00
byte 4    0x00
byte 5    mode byte             0xFF = direct-control mode
```

### Effect frame — opcode `0x3B`

```
byte 0    0xEC
byte 1    0x3B                  CMD_ADDR_EFFECT
byte 2    effect_channel
byte 3    0x00
byte 4    mode byte             see effect-mode table in §3
byte 5    R
byte 6    G
byte 7    B
```

### StopGen2 frame

The only frame whose byte 1 is not a standard opcode; a fixed magic sequence:

```
byte 0    0xEC
byte 1    0x52
byte 2    0x53
byte 3    0x00
byte 4    0x01
```

### Firmware request / Config request

```
firmware:  0xEC 0x82            CMD_FIRMWARE  (reply identified by byte 1 = 0x02)
config:    0xEC 0xB0            CMD_CONFIG    (reply identified by byte 1 = 0x30)
```

Reply layouts are in §4.

---

## 2. Functions

| Function | Bytes sent (65-byte frame, unlisted = `0x00`) | Params | Required sequence / notes |
|----------|-----------------------------------------------|--------|----------------------------|
| `stop_gen2` | `EC 52 53 00 01` | — | Single write; must be first command |
| `get_firmware_version` | `EC 82` | — | Write then read reply `EC 02 …`; see subsection + §4 |
| `get_config_table` | `EC B0` | — | Write then read reply `EC 30 …`; see subsection + §4 |
| `set_channel_direct` | `EC 35 <effch> 00 00 FF` | `effch` (0 = mainboard) | Single write; see subsection |
| `send_direct` | `EC 40 <dch\|apply> <off> <cnt> <r,g,b…>` | `dch`, `colors` | Multi-step chunked stream; see subsection |
| `send_direct_mb` | identical to `send_direct` with `dch = 0x04` | `colors` | Mainboard fixed zone |
| `send_effect_argb` | `EC 3B <effch> 00 <mode> <r> <g> <b>` | `effch`, `mode`, `r,g,b` | Single write |
| `send_effect` | resolves `id`→mode + `params["color"]`, then `send_effect_argb` | `id`, `effch`, `params` | Thin wrapper |

### `send_direct` — chunked per-LED stream

Streams an arbitrary-length color array to one direct channel as a sequence of sub-packets, each carrying at most **20 LEDs** (20 × 3 = 60 color bytes, which fits after the 5-byte header in a 65-byte frame). An empty color array sends nothing.

1. Set `offset = 0`.
2. While `offset < led_count`:
   1. `count = min(20, led_count − offset)`; `is_last = (offset + count == led_count)`.
   2. Build a 65-byte frame:
      - byte 0 = `0xEC`, byte 1 = `0x40`
      - byte 2 = `direct_channel | 0x80` if `is_last`, else `direct_channel`
      - byte 3 = `offset`  (**must fit in a u8** — a channel with > 255 LEDs is rejected with an error)
      - byte 4 = `count`
      - bytes 5… = `count` R,G,B triples taken from `colors[offset .. offset+count]`
   3. Write the frame; `offset += count`.

The `0x80` bit on byte 2 of the **final** sub-packet is the commit signal ("apply now"); all earlier sub-packets clear it. There is no separate apply command. A single write of ≤ 20 LEDs is its own last packet, so it always has `0x80` set.

### `set_channel_direct` — claim a channel for software control

Single write `EC 35 <effch> 00 00 FF`: puts effect channel `effch` into direct-control mode (mode byte `0xFF`). During init this is issued for every channel `0 … argb_count` inclusive, where channel 0 is the mainboard and channels `1 … argb_count` are the ARGB headers.

### Init sequence

Run once at device open, in this exact order:

1. **`stop_gen2()`** → write `EC 52 53 00 01`. Disables the controller's legacy gen-2 continuous-cycle so software can take ownership. Must precede everything.
2. **`get_firmware_version()`** → write `EC 82`, read reply (§4). Informational only — init does **not** fail if this returns nothing.
3. **`get_config_table()`** → write `EC B0`, read reply (§4). **Bails** if no reply arrives.
4. **Parse config** into `(argb_count, led_counts[], mb_leds)` (see config-read subsection). **Bails** if `argb_count == 0 && mb_leds == 0`.
5. **Claim every channel:** for `ch` in `0 ..= argb_count`, write `EC 35 <ch> 00 00 FF` (`set_channel_direct`).

### Config-table read + parse

1. Write `EC B0`.
2. Read a reply frame matching byte 0 = `0xEC`, byte 1 = `0x30` (up to 8 read attempts).
3. Copy `resp[4..64]` into a 60-byte config table (`config[0]` = `resp[4]`).
4. Extract (all offsets into the 60-byte table; see §3 for exact field semantics):
   - `argb_count = config[0x02]`
   - `mb_leds    = config[0x1B]`
   - for each ARGB channel `i` in `0 … argb_count-1`: `leds = config[4 + i*6 + 2]`; if `leds == 0` use the default **30**, else clamp to a max of **120**.

---

## 3. Parameters

### Frame constants

| Constant | Value | Meaning |
|----------|-------|---------|
| Header byte (byte 0) | `0xEC` | Always present on every frame, request and reply |
| Frame size | 65 bytes | Fixed; shorter payloads are zero-padded |
| Read timeout | 1000 ms | Per read attempt |
| Max LEDs per direct sub-packet | 20 | 20 × 3 = 60 color bytes after the 5-byte header |
| Default LEDs per ARGB channel | 30 | Used when the config reports 0 for a channel |
| Max LEDs per ARGB channel | 120 | Per-channel counts are clamped to this |

### Command opcodes (byte 1)

| Opcode | Name | Direction |
|--------|------|-----------|
| `0x82` | Firmware request | write → reply `0x02` |
| `0xB0` | Config request | write → reply `0x30` |
| `0x35` | Set mode (direct/effect channel) | write only |
| `0x40` | Direct per-LED stream | write only |
| `0x3B` | Native effect on a channel | write only |
| `0x52` | StopGen2 (magic `52 53 00 01`) | write only |

### Channel numbering

Each physical ARGB header `i` (0-based) has two channel numbers used by different commands:

| Channel kind | Formula | Used by | Notes |
|--------------|---------|---------|-------|
| **Effect channel** | `i + 1` (1-based) | `0x35` SetMode, `0x3B` Effect | Effect channel **0** is the mainboard |
| **Direct channel** | `i` (0-based) | `0x40` Direct stream | Mainboard fixed zone uses direct channel **`0x04`** |

The mainboard's fixed on-board LEDs (chipset, I/O cover, and any 12V RGB header positions) are written as one contiguous block to **direct channel `0x04`**; effect commands address the mainboard as effect channel `0`.

### Color encoding

Color is plain **R, G, B** byte order — there is no GRB/BGR swap. Each channel is a whole byte **0–255**. In a direct sub-packet LED `i`'s bytes are at `5+i*3` (R), `5+i*3+1` (G), `5+i*3+2` (B). Example: a single red LED `(R=0x11, G=0x22, B=0x33)` serialises to bytes `5,6,7 = 11 22 33`.

### Effect modes (byte 4 of the `0x3B` frame / byte 5 of `0x35`)

| Effect id | Name | Mode byte | Color param? |
|-----------|------|-----------|--------------|
| `off` | Off | `0x00` | no |
| `breathing` | Breathing | `0x02` | yes |
| `spectrum_cycle` | Spectrum Cycle | `0x04` | no |
| `rainbow_wave` | Rainbow Wave | `0x05` | no |
| (direct / canvas) | Direct control | `0xFF` | n/a (used by `0x35`) |

`send_effect` looks up the mode byte from the effect `id`, reads the color from `params["color"]`, and defaults to **white `(255,255,255)`** when the param is absent or not a color. Only `breathing` actually uses the color; the others ignore bytes 5–7.

### Config-table fields (60-byte table at `resp[4..64]`)

| Field | Offset | Definition |
|-------|--------|------------|
| ARGB channel count | `0x02` | Number of 5V ARGB headers the controller exposes |
| Mainboard LED count | `0x1B` | Total fixed on-board LEDs, **including** 12V RGB header positions, written as one block to direct channel `0x04` |
| Per-channel LED count | `4 + i*6 + 2` | LED count for ARGB channel `i`. The per-channel blocks start at offset `4`, are `6` bytes each, and the LED count sits at `+2` within the block. A reported `0` means "unknown" → default 30; any value is clamped to ≤ 120 |

### Mode-byte values

- `0xFF` = direct (software) control — written by `set_channel_direct`.
- `0x00` / `0x02` / `0x04` / `0x05` = the hardware effects in the effect-mode table above.

---

## 4. Responses

Only the two read commands return data; every other command is write-only with no acknowledgement.

### Firmware reply — byte 1 = `0x02`

After writing `EC 82`, read a frame whose byte 0 = `0xEC` and byte 1 = `0x02` (the reader retries up to 8 frames, discarding non-matching ones). The firmware version is the ASCII string in `resp[2..18]`, truncated at the first NUL byte. Returns nothing if no matching reply appears within the attempt limit.

### Config-table reply — byte 1 = `0x30`

After writing `EC B0`, read a frame whose byte 0 = `0xEC` and byte 1 = `0x30` (up to 8 attempts). The 60-byte config table is `resp[4..64]`; its field offsets are listed in §3.

No other command requires reading an ACK before continuing.

---

## 5. Polling & notifications

None — all access is host-initiated request/response. The device never originates packets; per-LED frames are pushed by the host on demand, and there is no periodic status report.

---

## Notes

- A channel reporting `0` LEDs in the config defaults to 30; per-channel counts are clamped to a maximum of 120.
- A single direct channel cannot exceed 255 LEDs — the start-offset byte is a u8, and `send_direct` errors rather than silently wrapping.
- Effect speed and direction are not encoded by this protocol; only `breathing` carries a color.
- The firmware read is informational; init succeeds without it. Some product IDs are assumed-compatible / untested (the supported-PID list lives in the [supported-devices table](../supported-devices.md), not here).
