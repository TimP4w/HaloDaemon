# DDC/CI Protocol

DDC/CI (Display Data Channel Command Interface) MCCS control tunnelled over USB vendor control transfers — the complete control surface HaloDaemon uses to drive the Philips Evnia 49 monitor (brightness, picture, audio, OSD, system, gaming, and device-info reads).

**Credits:** VESA DDC/CI and MCCS standards — no reverse engineering required.

---

## Overview

DDC/CI bytes are tunnelled inside USB vendor control transfers via the [USB control transport](../transports/usb-control.md). The model is **host-initiated request/response**: every frame is a DDC/CI envelope (`6e 51 <len> <opcode> …`) with a trailing XOR checksum; writes are fire-and-forget control-OUTs, reads are a control-OUT request followed ~150 ms later by a control-IN reply.

Control-transfer parameters:

| Direction | `bmRequestType` | `bRequest` | `wValue` | `wIndex` | Notes |
|-----------|-----------------|------------|----------|----------|-------|
| Write (host→device) | `0x40` | `0xB2` | `0` | `0` | DDC/CI request out (`write_packet`) |
| Read  (device→host) | `0xC0` | `0xA3` | `0` | `0x006F` | DDC/CI reply in (`read_get_reply`/`get_info`) |

Timing: reads wait `150 ms` after the request before the reply is read (firmware assembles it in ~130 ms); a minimum `50 ms` gap is enforced between consecutive writes (MCCS §4.5), tracked by `write_packet`.

---

## 1. Packet layout

All frames share the DDC/CI envelope: source byte `0x6e`, host byte `0x51`, a length byte (`0x80 | payload_len`), then the MCCS opcode and payload, terminated by an XOR checksum (defined in full in [§3](#xor-checksum-algorithm)).

### Standard Set-VCP write — 8 bytes (`build_write`)

```
Offset  Value   Meaning
  0     0x6E    destination (monitor)
  1     0x51    source (host)
  2     0x84    0x80 | length (4)
  3     0x03    Set VCP Feature opcode
  4     vcp     VCP code (single byte)
  5     0x00    value high byte
  6     value   value low byte
  7     xor     XOR of bytes 0..6
```

### Extended (Philips vendor) Set write — 10 bytes (`build_extended_set`)

Philips OSD-only settings sit behind vendor VCP `0xE2` with prefix `0xA0`; the setting is chosen by a one-byte sub-command.

```
Offset  Value   Meaning
  0     0x6E    destination
  1     0x51    source
  2     0x86    0x80 | length (6)
  3     0x03    Set VCP Feature opcode
  4     0xE2    vendor VCP code
  5     0xA0    vendor prefix
  6     sub     sub-command (selects the setting)
  7     0x00    value high byte
  8     value   value low byte
  9     xor     XOR of bytes 0..9
```

### Standard Get-VCP request — 6 bytes (`build_get_standard`)

```
0x6E  0x51  0x82  0x01  vcp  xor
                  └ Get VCP Feature opcode
```

### Extended Get request — 8 bytes (`build_get_extended`)

```
0x6E  0x51  0x84  0x01  0xE2  0xA0  sub  xor
```

### Get-info string request — 10 bytes (`build_get_info`)

Reads device-info / EEPROM strings; the 4-byte address selects the page.

```
0x6E  0x51  0x86  0x01  0xFE  addr[0]  addr[1]  addr[2]  addr[3]  xor
                        └ info-string opcode
```

### Reply frames

- **Get-VCP reply** (`parse_get_reply`) — `6e 88 02 00 vcp type maxH maxL curH curL xor`. Current value is the big-endian `u16` from bytes 8–9.
- **Info-string reply** (`parse_info_reply`) — `6e <len> [02 fe <addr_echo>] <ascii…> xor`. The standard envelope carries an `02 fe <addr_echo>` prefix; the asset-EEPROM reply (`ef 13 00 20`) is raw ASCII with no prefix.

Reply parsing, error codes, and reply-checksum verification are detailed in [§4](#4-responses).

---

## 2. Functions

Frame column shows exact bytes with `<param>` placeholders. **STD** = 8-byte `build_write(vcp, value)`; **EXT** = 10-byte `build_extended_set(sub, value)` (vendor prefix `0xE2 0xA0`). The `<xor>` byte is the trailing checksum — its computation is the same for every frame and is shown in [§2 Standard Set flow](#standard-set-vcp-flow) and defined in [§3](#xor-checksum-algorithm). Every individual VCP write below is a plain `build_write`/`build_extended_set` call with identical mechanics, so they stay table rows referencing the Set-flow subsections. Enum/range values resolve in [§3](#3-parameters).

### Picture

| Function | Frame / transfer | Params | Notes |
|----------|------------------|--------|-------|
| Brightness | `6e 51 84 03 10 00 <val> <xor>` | 0–100 | STD |
| Contrast | `6e 51 84 03 12 00 <val> <xor>` | 0–100 | STD |
| Sharpness | `6e 51 84 03 87 00 <val> <xor>` | 0–100 | STD |
| Color Temperature | `6e 51 84 03 14 00 <code> <xor>` | enum (§3) | STD |
| Select Gamma | `6e 51 84 03 72 00 <code> <xor>` | enum (§3) | STD |
| SmartImage | `6e 51 84 03 dc 00 <code> <xor>` | enum (§3) | STD; SDR/HDR share this VCP |
| sRGB | `6e 51 86 03 e2 a0 20 00 <val> <xor>` | `0x00` off / `0x02` on | EXT |
| Light Enhancement | `6e 51 86 03 e2 a0 3d 00 <val> <xor>` | 0–3 | EXT |
| Color Enhancement | `6e 51 86 03 e2 a0 3e 00 <val> <xor>` | 0–3 | EXT |
| Red Gain | `6e 51 84 03 16 00 <val> <xor>` | 0–100 | STD MCCS Video Gain (color balance) |
| Green Gain | `6e 51 84 03 18 00 <val> <xor>` | 0–100 | STD MCCS Video Gain |
| Blue Gain | `6e 51 84 03 1a 00 <val> <xor>` | 0–100 | STD MCCS Video Gain |

### Gaming

| Function | Frame / transfer | Params | Notes |
|----------|------------------|--------|-------|
| Crosshair | `6e 51 86 03 e2 a0 04 00 <val> <xor>` | 0=Off, 1=On, 2=Smart | EXT |
| SmartResponse | `6e 51 84 03 eb 00 <val> <xor>` | 0=Off,1=Fast,2=Faster,3=Fastest | STD |
| Low Input Lag | `6e 51 86 03 e2 a0 07 00 <val> <xor>` | bool 0/1 | EXT |
| Adaptive Sync | `6e 51 86 03 e2 a0 40 00 <val> <xor>` | bool 0/1 | EXT |

### Audio

| Function | Frame / transfer | Params | Notes |
|----------|------------------|--------|-------|
| Audio Volume | `6e 51 84 03 62 00 <val> <xor>` | 0–100 | STD |
| Audio Mute | `6e 51 84 03 8d 00 <val> <xor>` | 1=mute, 2=unmute | STD; non-0/1 encoding |

### OSD

| Function | Frame / transfer | Params | Notes |
|----------|------------------|--------|-------|
| OSD Language | `6e 51 84 03 cc 00 <code> <xor>` | enum (§3) | STD |
| Resolution Notice | `6e 51 84 03 e9 00 <val> <xor>` | 0=off, 2=on | STD; non-0/1 encoding |
| OSD H-Position | `6e 51 86 03 e2 a0 0e 00 <val> <xor>` | 0–100 | EXT |
| OSD V-Position | `6e 51 86 03 e2 a0 0f 00 <val> <xor>` | 0–100 | EXT |
| OSD Transparency | `6e 51 86 03 e2 a0 10 00 <val> <xor>` | 0–4 | EXT |
| OSD Timeout | `6e 51 86 03 e2 a0 11 00 <val> <xor>` | 0–4 → 5/10/20/30/60 s | EXT |

### System / setup

| Function | Frame / transfer | Params | Notes |
|----------|------------------|--------|-------|
| Input Source | `6e 51 84 03 60 00 <code> <xor>` | enum (§3) | STD |
| Power Mode | `6e 51 84 03 d6 00 <code> <xor>` | `0x01` On / `0x04` Standby | STD MCCS Power Mode |
| Power LED Brightness | `6e 51 84 03 f2 00 <val> <xor>` | 0–4 (0=Off … 4=Max) | STD |
| USB-C Setting | `6e 51 86 03 e2 a0 12 00 <val> <xor>` | 0=Hi-Res USB2.0, 1=Hi-Speed USB3.2 | EXT |
| USB Standby | `6e 51 86 03 e2 a0 13 00 <val> <xor>` | bool 0/1 | EXT |
| KVM | `6e 51 86 03 e2 a0 15 00 <val> <xor>` | 0=Auto, 1=USB Up, 2=USB-C | EXT |
| Smart Power | `6e 51 86 03 e2 a0 16 00 <val> <xor>` | bool 0/1 | EXT |
| CEC | `6e 51 86 03 e2 a0 17 00 <val> <xor>` | bool 0/1 | EXT |
| Pixel Orbiting | `6e 51 86 03 e2 a0 34 00 <code> <xor>` | enum (§3) | EXT |
| Screen Saver | `6e 51 86 03 e2 a0 35 00 <code> <xor>` | enum (§3) | EXT |
| Pixel Refresh | `6e 51 86 03 e2 a0 36 00 01 <xor>` | fixed `0x01` | EXT; momentary one-shot, not stored |
| Auto Warning | `6e 51 86 03 e2 a0 43 00 <val> <xor>` | bool 0/1 | EXT |

### Reads (Get requests)

| Function | Request frame | Reply / notes |
|----------|---------------|---------------|
| `get_standard(vcp)` | `6e 51 82 01 <vcp> <xor>` | Reply via [Get-VCP flow](#get-vcp-read-flow); reads brightness `0x10`, contrast `0x12`, volume `0x62`, power-LED `0xF2`, audio-mute `0x8D`, resolution-notice `0xE9`, smart-response `0xEB`, input `0x60`, OSD-language `0xCC`, SmartImage `0xDC`, sharpness `0x87`, gamma `0x72`, color-temp `0x14`, RGB gain `0x16`/`0x18`/`0x1A`, power-mode `0xD6` |
| `get_extended(sub)` | `6e 51 84 01 e2 a0 <sub> <xor>` | Reads crosshair `0x04`, low-input-lag `0x07`, OSD-H `0x0E`, OSD-V `0x0F`, transparency `0x10`, timeout `0x11`, USB-C `0x12`, USB-standby `0x13`, KVM `0x15`, smart-power `0x16`, CEC `0x17`, sRGB `0x20`, pixel-orbiting `0x34`, screen-saver `0x35`, light-enh `0x3D`, color-enh `0x3E`, adaptive-sync `0x40`, auto-warning `0x43` |
| `get_info(addr)` | `6e 51 86 01 fe <a0> <a1> <a2> <a3> <xor>` | See [Device-info string read flow](#device-info-string-read-flow) |

---

### Standard Set-VCP flow

Used by every "STD" row above. `build_write(vcp, value)`:

1. Lay out the 8-byte frame `[0x6e, 0x51, 0x84, 0x03, vcp, 0x00, value, 0x00]`. Byte 2 `0x84` = `0x80 | 4` (length 4); byte 3 `0x03` = Set-VCP opcode; byte 4 is the VCP code; byte 5 is the value-high byte (always `0x00` — all values are 8-bit); byte 6 is the value.
2. Compute the checksum over **bytes 0–6** (XOR fold from seed `0x00`, see [§3](#xor-checksum-algorithm)) and store it in byte 7.
3. Issue the control-OUT (`bmRequestType=0x40`, `bRequest=0xB2`, `wValue=0`, `wIndex=0`) carrying the 8 bytes, after honouring the 50 ms write gap (`write_packet`).

There is no acknowledgement — the write is fire-and-forget.

Worked example — brightness 75 (`0x4B`): frame `6e 51 84 03 10 00 4b`, checksum `6e^51^84^03^10^00^4b = 0x9b`, transmitted `6e 51 84 03 10 00 4b 9b`.

### Extended Set-VCP flow

Used by every "EXT" row above. `build_extended_set(sub, value)`:

1. Lay out the 10-byte frame `[0x6e, 0x51, 0x86, 0x03, 0xe2, 0xa0, sub, 0x00, value, 0x00]`. Byte 2 `0x86` = `0x80 | 6` (length 6); byte 3 `0x03` = Set-VCP opcode; bytes 4–5 `0xe2 0xa0` are the **Philips vendor VCP code + prefix** that route the command to the vendor sub-feature table; byte 6 is the sub-command selecting the setting; byte 7 is the value-high byte (`0x00`); byte 8 is the value.
2. Compute the checksum over **bytes 0–8** (seed `0x00`) into byte 9.
3. Issue the control-OUT exactly as the standard flow (same `bmRequestType`/`bRequest`/`wValue`/`wIndex`).

Worked example — light-enhancement 3: frame `6e 51 86 03 e2 a0 3d 00 03`, checksum `…^03 = 0xc6`, transmitted `6e 51 86 03 e2 a0 3d 00 03 c6`.

### Get-VCP read flow

Used by `get_standard`/`get_extended`:

1. **Request.** Build the get frame — standard `6e 51 82 01 <vcp> <xor>` (`build_get_standard`, byte 3 `0x01` = Get-VCP opcode) or extended `6e 51 84 01 e2 a0 <sub> <xor>` (`build_get_extended`) — and write it via the standard control-OUT.
2. **Wait.** Sleep `READ_DELAY` = 150 ms so the firmware can assemble the reply.
3. **Read.** Issue the control-IN (`bmRequestType=0xC0`, `bRequest=0xA3`, `wValue=0`, `wIndex=0x006F`) into a 32-byte buffer.
4. **Parse** (`parse_get_reply`): verify byte 0 == `0x6e`, byte 2 == `0x02` (reply opcode), byte 3 == `0x00` (error code; non-zero = monitor error), and the reply checksum `buf[10] == 0x50 ^ XOR(buf[0..10])` (seed `0x50`, see [§3](#xor-checksum-algorithm)). Return the big-endian `u16` from bytes 8–9. Extended replies use the identical layout and report `vcp = 0xe2` (the sub byte is omitted — the caller already knows it).

### Device-info string read flow

Used by `read_device_info` (`get_info`):

1. **Request.** Build `6e 51 86 01 fe <a0> <a1> <a2> <a3> <xor>` (`build_get_info`, byte 3 `0x01` = Get opcode, byte 4 `0xFE` = info-string opcode) for the 4-byte page address and write it.
2. **Wait** 150 ms, then **read** the control-IN into a 32-byte buffer (same params as the Get-VCP read).
3. **Parse** (`parse_info_reply`): verify byte 0 == `0x6e`; take the length `n = buf[1] & 0x7f`; verify the checksum `buf[2+n] == 0x50 ^ XOR(buf[0..2+n])`. If the body starts `02 fe` (standard envelope) strip those two bytes plus the echoed address; otherwise treat the body as raw ASCII (asset EEPROM). Decode lossy-UTF-8, trim at the first NUL.

The five device-info pages:

| Field | Address (`a0 a1 a2 a3`) | Reply envelope |
|-------|-------------------------|----------------|
| Model number | `e9 0d 00 00` | standard `02 fe …` |
| Firmware version | `e1 e6 06 00` | standard `02 fe …` |
| Panel variant | `e1 e6 1d 00` | standard `02 fe …` |
| Panel id | `e1 e8 00 00` | standard `02 fe …` |
| Serial number | `ef 13 00 20` | raw ASCII (asset EEPROM) |

**Frame-type selection.** Standard MCCS features use the standard 8-byte Set; Philips OSD-only features behind vendor VCP `0xE2 0xA0` use the extended 10-byte Set. Reads mirror this. No apply/commit step exists — each Set takes effect on its own, subject only to the 50 ms write gap.

---

## 3. Parameters

This section defines every value, range, enum, and formula referenced above; nothing here requires opening the code.

### XOR checksum algorithm

The trailing checksum byte of every frame is a running XOR fold. Given the frame bytes `b[0..k]` preceding the checksum slot, the checksum is:

```
checksum = seed XOR b[0] XOR b[1] XOR … XOR b[k-1]
```

- **Outbound frames (all writes and get-requests): seed = `0x00`.** The checksum covers every byte from offset 0 up to (but not including) the checksum slot — bytes 0–6 for the 8-byte Set, 0–8 for the 10-byte extended Set, 0–4 for the 6-byte get, 0–6 for the 8-byte extended get, 0–8 for the 10-byte get-info.
- **Reply frames (get-VCP and info-string): seed = `0x50`.** The verifier computes `0x50 XOR (XOR of all reply bytes before the checksum slot)` and compares it to the received checksum byte.

Worked example (outbound, seed `0x00`) — contrast = 0, frame `6e 51 84 03 12 00 00`:
`0x00 ^ 0x6e = 0x6e`; `^0x51 = 0x3f`; `^0x84 = 0xbb`; `^0x03 = 0xb8`; `^0x12 = 0xaa`; `^0x00 = 0xaa`; `^0x00 = 0xaa`. Checksum = **`0xaa`**, so the wire bytes are `6e 51 84 03 12 00 00 aa`.

(The first XORed byte is `0x6e` itself, so the outbound `0x00` seed yields an effective `0x6e`-seeded fold over the remaining bytes; replies fold from an explicit `0x50` seed instead.)

### Value width

Every settable value is a single unsigned byte. In both Set frames the byte immediately before the value (`value high byte`) is always `0x00`; the device uses only the low byte. Ranges below are inclusive.

### Brightness / Contrast / Sharpness / Volume — `0x10` / `0x12` / `0x87` / `0x62`

Whole value **0–100**, sent directly as the value byte.

### Light / Color Enhancement — EXT `0x3D` / `0x3E`

Whole value **0–3** (0 = off … 3 = strongest).

### OSD H-Position / V-Position — EXT `0x0E` / `0x0F`

Whole value **0–100**.

### Power LED Brightness — `0xF2`

Index **0–4**: `0` = Off, `4` = Max (intermediate steps 1–3 dimmer-to-brighter; UI labels the endpoints "Off" and "Max").

### Color Temperature — VCP `0x14`

| Code | Label |
|------|-------|
| `0x02` | Native |
| `0x04` | 5000K |
| `0x05` | 6500K |
| `0x06` | 7500K |
| `0x07` | 8200K |
| `0x08` | 9300K |
| `0x0A` | 11500K |
| `0x0D` | Preset |

### Select Gamma — VCP `0x72`

| Code | Label |
|------|-------|
| `0x50` | 1.0 |
| `0x64` | 2.0 |
| `0x78` | 2.2 |
| `0x8C` | 2.4 |
| `0xA0` | 2.6 |

### Input Source — VCP `0x60`

| Code | Id | Label |
|------|----|-------|
| `0x0F` | dp1 | DisplayPort 1 |
| `0x10` | dp2 | DisplayPort 2 |
| `0x11` | hdmi1 | HDMI 1 |
| `0x12` | hdmi2 | HDMI 2 |

The write sends just the port code in the value byte. On **read**, VCP `0x60` returns the port code in the low byte and `0x35` in the high byte; the driver masks with `& 0xFF` to recover the port.

### SmartImage — VCP `0xDC`

SDR and HDR presets share this single VCP. Codes `0x21`–`0x24` and `0x30` are the HDR presets; `0x20` = HDR Off.

| Code | Label |
|------|-------|
| `0x00` | Standard |
| `0x01` | FPS |
| `0x03` | Movie |
| `0x04` | Game 1 |
| `0x05` | Game 2 |
| `0x06` | Racing |
| `0x07` | RTS |
| `0x08` | Economy |
| `0x0B` | LowBlue Mode |
| `0x0E` | EasyRead |
| `0x11` | Console Mode |
| `0x21` | HDR Game |
| `0x22` | HDR Movie |
| `0x23` | HDR Vivid |
| `0x30` | HDR True Black |
| `0x24` | HDR Personal |
| `0x20` | HDR Off |

The OSD only shows the subset valid for the current input mode, so this exposed list is a superset of what any single mode displays. A separate cap-string entry `0xE2` is omitted (unknown OSD label).

### OSD Language — VCP `0xCC` (21 entries)

| Code | Label |
|------|-------|
| `0x01` | Chinese (Traditional) |
| `0x02` | English |
| `0x03` | French |
| `0x04` | German |
| `0x05` | Italian |
| `0x06` | Japanese |
| `0x07` | Korean |
| `0x08` | Portuguese |
| `0x09` | Russian |
| `0x0A` | Spanish |
| `0x0B` | Chinese (Simplified) |
| `0x0C` | Dutch |
| `0x0D` | Czech |
| `0x0E` | Polish |
| `0x12` | Hungarian |
| `0x14` | Turkish |
| `0x16` | Brazilian Portuguese |
| `0x17` | Finnish |
| `0x1A` | Greek |
| `0x1E` | Ukrainian |
| `0x24` | Swedish |

### Pixel Orbiting — EXT `0x34`

| Code | Label |
|------|-------|
| `0x00` | Off |
| `0x02` | Slow |
| `0x03` | Normal |
| `0x04` | Fast |

### Screen Saver — EXT `0x35`

| Code | Label |
|------|-------|
| `0x00` | Off |
| `0x02` | Slow |
| `0x03` | Fast |

### Fixed-index enums

Sent as the plain 0-based index (the index **is** the value byte):

| Feature | Frame | Values |
|---------|-------|--------|
| Crosshair | EXT `0x04` | 0 = Off, 1 = On, 2 = Smart |
| SmartResponse | STD `0xEB` | 0 = Off, 1 = Fast, 2 = Faster, 3 = Fastest |
| OSD Transparency | EXT `0x10` | 0–4 (Off, 1, 2, 3, 4) |
| OSD Timeout | EXT `0x11` | 0 → 5 s, 1 → 10 s, 2 → 20 s, 3 → 30 s, 4 → 60 s |
| USB-C Setting | EXT `0x12` | 0 = High Resolution (USB 2.0), 1 = High Data Speed (USB 3.2) |
| KVM | EXT `0x15` | 0 = Auto, 1 = USB Up, 2 = USB-C |

### Boolean encodings

Most booleans send `0` (off) / `1` (on) directly: `low_input_lag` `0x07`, `usb_standby` `0x13`, `smart_power` `0x16`, `cec` `0x17`, `adaptive_sync` `0x40`, `auto_warning` `0x43`. Three booleans use **non-0/1** codes:

| Feature | Frame | Off | On |
|---------|-------|-----|----|
| Audio Mute | STD `0x8D` | `0x02` (unmute) | `0x01` (mute) |
| Resolution Notice | STD `0xE9` | `0x00` | `0x02` |
| sRGB | EXT `0x20` | `0x00` | `0x02` |

### Pixel Refresh — EXT `0x36`

Momentary action, not a stored setting: always sends value `0x01` once. There is no "off" — triggering it runs a one-shot panel pixel-refresh cycle.

---

## 4. Responses

Writes have no acknowledgement frame — they are fire-and-forget control-OUTs.

**Get-VCP reply** (`parse_get_reply`): `6e 88 02 00 vcp type maxH maxL curH curL xor`. The driver verifies byte 0 == `0x6e`, byte 2 == `0x02` (reply opcode), byte 3 == `0x00` (error code; non-zero ⇒ monitor error, surfaced as an error and the read skipped), and the checksum `buf[10] == 0x50 ^ XOR(buf[0..10])`. The returned value is the big-endian `u16` from bytes 8–9. Extended-VCP replies use the identical layout with `vcp` reported as `0xe2`.

**Info-string reply** (`parse_info_reply`): length byte masked with `0x7f`; checksum `buf[2+n] == 0x50 ^ XOR(buf[0..2+n])`. The `02 fe <addr_echo>` prefix (standard envelope) is stripped when present; the asset-EEPROM reply is raw ASCII. Payload is decoded lossy-UTF-8 and trimmed at the first NUL.

**Read-after-write behavior** (`read_get_reply`; `get_info`): the host writes the Get request, sleeps `READ_DELAY` (150 ms) so the firmware can assemble the reply, then issues the control-IN (`0xC0`, `0xA3`, wIndex `0x006F`) into a 32-byte buffer and parses it. Excess buffered bytes past the frame are ignored (the parser stops at the declared length / embedded NUL).

---

## 5. Polling & notifications

None — the protocol is host-initiated. The monitor never pushes state; every value is obtained with an explicit Get request (see [§2 Reads](#reads-get-requests) / [§4](#4-responses)). At `initialize()` the driver reads all device-info strings once, then `refresh_from_monitor()` re-reads every VCP on its own cadence; individual read failures are logged and skipped so one mode-gated VCP doesn't block connection.

---

## Notes

- No apply/commit step — each Set takes effect individually; the only ordering constraint is the 50 ms minimum gap between writes, without which the firmware drops or mis-applies commands.
- Many VCPs are mode-gated by the firmware (e.g. brightness is locked unless SmartImage is "Personal"); a Get on a gated VCP returns an error and is skipped.
- The outbound checksum seed is `0x00`; reply checksums use seed `0x50`. The `0x50` seed never appears in outbound frames.
- SmartImage and HDR presets share one VCP, so the exposed preset list is a superset of what any single input mode shows in the OSD.
