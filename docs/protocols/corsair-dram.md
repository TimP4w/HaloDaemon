# Corsair DRAM Protocol

SMBus RGB control for Corsair Vengeance / Dominator DDR4 and DDR5 memory modules, with a CRC8-protected 32-byte info block, native hardware effects, and two per-LED color paths selected by the firmware's reported protocol version.

**Credits:** reference implementation from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) (GPL-2.0-or-later): `CorsairVengeanceController` (Adam Honse / CalcProgrammer1, Erik Gilling / konkers).

---

## Overview

A Corsair DRAM module exposes an RGB controller on the chipset SMBus, addressed through a fixed register map. The host issues SMBus byte/block reads and writes; there is no asynchronous channel — every exchange is a host-initiated request/response, and long operations are confirmed by polling a status busy bit. Byte movement is over the [SMBus transport](../transports/smbus.md) (`SmBusSyncOps`).

The controller is probed at 16 chipset-bus addresses:

```
0x58 0x59 0x5A 0x5B 0x5C 0x5D 0x5E 0x5F   0x18 0x19 0x1A 0x1B 0x1C 0x1D 0x1E 0x1F
```

Framing model: most commands are **register-select + 1 data byte** (`write_byte_data`); color payloads use either **SMBus block writes** (direct mode) or a **byte-at-a-time stream** into a buffer that is then committed (effect mode). A 32-byte **info block** (read one byte at a time from register `0x40`) carries firmware and protocol version, validated by an 8-bit CRC the device computes and returns at `0x42`. The same CRC mechanism guards streamed payloads before they are committed.

Two color paths exist and are chosen by the firmware's reported `protocol_version` byte:

- `protocol_version >= 4` → **direct mode** (block writes of a framed RGB packet).
- `protocol_version < 4` → **effect/streaming mode** (RGBA per-LED buffer streamed and committed).

---

## 1. Packet layout

### Register map

| Const | Value | Role |
|-------|-------|------|
| `REG_RESET_BUFFER` | `0x0B` | Reset the binary streaming buffer |
| `REG_SET_BINARY_DATA` | `0x20` | Write one streamed binary byte |
| `REG_BINARY_START` | `0x21` | Start/seek binary buffer (write `0x00`) |
| `REG_STATUS` | `0x30` | Status byte (busy bit) |
| `REG_COLOR_BUFFER_BLOCK_1` | `0x31` | Direct-mode color block 1 (first ≤32 bytes) |
| `REG_COLOR_BUFFER_BLOCK_2` | `0x32` | Direct-mode color block 2 (remainder) |
| `REG_GET_BINARY_DATA` | `0x40` | Read one binary byte (info block source) |
| `REG_GET_CHECKSUM` | `0x42` | Read device-computed CRC8 of last buffer |
| `REG_SENTINEL_A` | `0x43` | Detection sentinel A |
| `REG_SENTINEL_B` | `0x44` | Detection sentinel B |
| `REG_GET_DEVICE_INFO` | `0x61` | Request device-info binary buffer |
| `REG_WRITE_CONFIGURATION` | `0x82` | Commit a streamed config by ID |

### Info block (32 bytes, read from `0x40`)

Read as 32 successive single-byte reads of register `0x40` into `data[0..32]`, then a CRC8 byte read from `0x42`. Interpreted offsets:

| Offset | Field | Encoding |
|--------|-------|----------|
| `data[0..2]` | VID | u16 little-endian (acceptance gate, see §3) |
| `data[2..4]` | PID | u16 little-endian (selects LED count / reverse, see §3) |
| `data[8]` | firmware minor | byte |
| `data[9]` | firmware major | byte |
| `data[10..12]` | firmware patch | u16 little-endian |
| `data[28]` | protocol version | byte; `>= 4` → direct, `< 4` → effect |

Firmware string = `"{data[9]}.{data[8]}.{u16_le(data[10],data[11])}"`. Offsets 4–7, 12–27, 29–31 are unused.

### Direct-mode color packet (protocol ≥ 4) — `build_direct_packet`

Total length `led_count * 3 + 2`:

```
[0]                led_count (u8)
[1 + 3*i .. +3]    R, G, B for LED i      (i = 0 .. led_count-1)
[last]             CRC8 over packet[0 .. len-1]
```

Color order on the wire is **R, G, B**. Trailing byte is `crc8(packet[..len-1])`. The packet is split at 32 bytes across `0x31` / `0x32` (see §2). A 10-LED packet is exactly 32 bytes (one block); a 12-LED packet is 38 bytes (6 spill into block 2).

### Effect-mode color buffer (legacy DDR4, protocol < 4) — `build_effect_color_data`

Per-LED **RGBA** with fixed `0xFF` alpha: for each LED `[R, G, B, 0xFF]`. Length `led_count * 4`. No length prefix and no trailing CRC inside the buffer — integrity is verified by reading the device CRC at `0x42` before commit.

### Native effect descriptor (20 bytes, all protocol versions) — `build_native_effect`

```
[0]       mode        (see §3 mode table)
[1]       speed       (see §3 speed table)
[2]       random flag (0x00 if random, else 0x01 — inverted)
[3]       direction   (see §3 direction table)
[4..7]    color1 R, G, B
[7]       brightness
[8..11]   color2 R, G, B
[11]      brightness (repeated)
[12..20]  zero-filled
```

### DDR4 vs DDR5 differences

| Aspect | Direct (proto ≥ 4) | Effect/streaming (proto < 4) |
|--------|--------------------|------------------------------|
| Color path | block write `0x31` / `0x32` | byte stream + commit `0x82`←`0x02` |
| Per-LED bytes | R, G, B | R, G, B, 0xFF |
| Framing | `led_count` prefix + trailing CRC8 | no prefix/CRC in buffer |
| Commit step | none (block write applies) | required |

Canvas/animation frames always use the direct path regardless of protocol version.

---

## 2. Functions

Master table of every operation the driver can issue. `<addr>` is the 7-bit SMBus device address; `R G B` are per-LED color bytes; all numbers hex. `0xRR←VV` denotes an SMBus `write_byte_data(<addr>, 0xRR, 0xVV)` (register-select then one data byte). Non-trivial functions have an ordered subsection below; trivial single-register writes are table-only.

| Function | Bytes sent (exact) | Params | Note |
|----------|--------------------|--------|------|
| Detection ACK | `write_quick(<addr>)` | — | Probe step 1 — see [Detection probe](#detection-probe) |
| Read sentinel A | read `0x43` | — | Probe step 2 |
| Read sentinel B | read `0x44` | — | Probe step 3 |
| Read info block | `0x61←0x00`, `0x21←0x00`, read `0x40` ×32, read `0x42` | — | See [Info-block read](#info-block-read--parse) |
| Color write (direct) | block `0x31←[len, R G B …]` (+ `0x32←[rest]`) | per-LED `R G B` | proto ≥ 4 — see [Direct block color write](#direct-block-color-write) |
| Color write (effect) | stream `[R G B 0xFF …]` → `0x82←0x02` | per-LED `R G B` | proto < 4 — see [Streamed RGBA color write](#streamed-rgba-color-write) |
| Native effect | stream `[20-byte descriptor]` → `0x82←0x01` | mode, speed, dir, color1, color2 | see [Native-effect write](#native-effect-write) |
| Reset stream buffer | `0x0B←0x00` | — | Trivial; first step of a stream |
| Stream seek/start | `0x21←0x00` | — | Trivial; second step of a stream |
| Stream one byte | `0x20←<byte>` | `<byte>` | Trivial; repeated per payload byte |
| Commit config | `0x82←<config_id>` | `0x01` effect / `0x02` color | Trivial; only after CRC match |
| Poll status | read `0x30` | — | Trivial; ready when `& 0x08 == 0` (§4) |

### Detection probe

Three steps; all must pass for the controller to be considered present (`probe_corsair_dram`). On any failure the device is skipped (`initialize()` returns `Ok(false)`) before any info read.

1. **ACK** — `write_quick(<addr>)`. Must return an ACK (true); otherwise abort.
2. **Sentinel A** — read register `0x43`. Accept only `0x1A`, `0x1B`, or `0x1C`; any other value or a read error aborts.
3. **Sentinel B** — read register `0x44`. Accept only `0x01`, `0x03`, or `0x04`; otherwise abort.

### Info-block read & parse

Loads the 32-byte info block and validates it (`sync_read_device_info`).

1. **Request** — `0x61←0x00` (`REG_GET_DEVICE_INFO`), then `0x21←0x00` (`REG_BINARY_START`). This arms the device-info buffer.
2. **Read body** — read register `0x40` (`REG_GET_BINARY_DATA`) 32 times, filling `data[0..32]`.
3. **Verify CRC** — compute `crc8(data)` (algorithm in §3), read the device CRC from `0x42`. A mismatch **aborts the info read and returns an error** — parsing does not continue with corrupt data.
4. **Parse** — extract fields per the §1 info-block table: VID `data[0..2]`, PID `data[2..4]`, firmware from `data[8..12]`, protocol version `data[28]`.
5. **Validate VID** — if VID ≠ `0x1B1C`, return an error (device rejected). PID then resolves LED count and reverse via the §3 PID table.

### Direct block color write

Used when `protocol_version >= 4` (`build_direct_packet` + `write_direct_packet`). No streaming, no commit, no busy poll — the block write applies on the SMBus ACK.

1. **Build packet** — length `led_count * 3 + 2`:
   - byte `[0]` = `led_count`.
   - bytes `[1 + 3i .. +3]` = `R, G, B` of LED `i` (reverse-mapped if the model reverses, §3).
   - byte `[last]` = `crc8(packet[0 .. len-1])` (CRC8 over everything before it).
2. **Write block 1** — SMBus block-write the first `min(len, 32)` bytes to register `0x31` (`REG_COLOR_BUFFER_BLOCK_1`).
3. **Write block 2 (conditional)** — only if `len > 32`: block-write bytes `[32..]` to register `0x32` (`REG_COLOR_BUFFER_BLOCK_2`). Example: 10 LEDs → 32 bytes (block 1 only); 12 LEDs → 38 bytes (block 1 = 32, block 2 = 6).

### Streamed RGBA color write

Used when `protocol_version < 4` (legacy DDR4) (`build_effect_color_data` + `stream_and_commit`). Selection vs. the direct path is by `info.protocol_version >= 4`.

1. **Build buffer** — for each LED emit 4 bytes `R, G, B, 0xFF` (alpha fixed at `0xFF`), reverse-mapped if the model reverses (§3). Length `led_count * 4`. No length prefix, no in-buffer CRC.
2. **Reset** — `0x0B←0x00` (`REG_RESET_BUFFER`).
3. **Seek** — `0x21←0x00` (`REG_BINARY_START`).
4. **Stream** — for each buffer byte, `0x20←<byte>` (`REG_SET_BINARY_DATA`).
5. **Verify CRC** — read device CRC from `0x42`; compare to `crc8(buffer)`. On **mismatch, log a warning and skip the commit** — LEDs are left unchanged.
6. **Commit** — on match, `0x82←0x02` (`REG_WRITE_CONFIGURATION` ← `CONFIG_ID_COLOR_DATA`).
7. **Poll** — read `0x30` up to 5 times at 10 ms intervals; done when `(status & 0x08) == 0` (busy bit clear).

### Native-effect write

Sets a hardware effect; available on all protocol versions (`build_native_effect` + `stream_and_commit`). Same stream→CRC→commit→poll ceremony as above, differing only in payload and config ID.

1. **Build descriptor** — the 20-byte layout in §1: `[0]` mode, `[1]` speed, `[2]` random flag (`0x00` random else `0x01`), `[3]` direction, `[4..7]` color1 RGB, `[7]` brightness, `[8..11]` color2 RGB, `[11]` brightness again, `[12..20]` zero. Enum values are in §3.
2. **Reset** — `0x0B←0x00`.
3. **Seek** — `0x21←0x00`.
4. **Stream** — `0x20←<byte>` for each of the 20 bytes.
5. **Verify CRC** — read `0x42`; compare to `crc8(descriptor)`. Mismatch → warn and skip.
6. **Commit** — `0x82←0x01` (`CONFIG_ID_EFFECT`).
7. **Poll** — read `0x30` up to 5× at 10 ms until `(status & 0x08) == 0`.

---

## 3. Parameters

This section defines every value, range, enum, and formula the protocol uses. No reference below requires reading the code.

### RGB wire order

Colors are sent **R, G, B** in that byte order. The direct packet uses 3 bytes per LED (`R G B`); the legacy effect buffer uses 4 (`R G B 0xFF`, alpha always `0xFF`). Any LED with no supplied color defaults to black `[0x00, 0x00, 0x00]`.

### LED count & reverse — PID table

LED count is **not** carried in the info block; it is determined entirely by the PID (`data[2..4]`) via this table. A model with `reverse = yes` has its color list mirrored before transmission: logical LED 0 is written to the physically last LED, i.e. color index `led_count - 1 - i` is used for output position `i`.

| Model | PIDs | LED count | reverse |
|-------|------|-----------|---------|
| Corsair Vengeance RGB DDR5 | `0700` `0701` `0900` `0901` `0910` `0911` | 10 | no |
| Corsair Dominator Platinum RGB DDR5 | `0600` `0601` | 12 | yes |
| Corsair Dominator Titanium RGB DDR5 | `0800` `0801` `0810` `0811` | 12 | yes |
| Corsair Vengeance Shugo Series DDR5 | `0A00` `0A01` `0A10` `0A11` | 10 | no |
| Corsair Vengeance RGB RS DDR5 | `0B00` `0B01` | 6 | no |
| Corsair Vengeance RGB Pro DDR4 | `0100` `0101` | 10 | no |
| Corsair Dominator Platinum RGB DDR4 | `0200` `0201` | 12 | yes |
| Corsair Vengeance RGB Pro SL DDR4 | `0300` `0301` | 10 | no |
| Corsair Vengeance RGB RS DDR4 | `0400` `0401` | 6 | no |
| **Any unlisted PID (fallback)** | — | 10 | no |

An unrecognised PID logs a warning and assumes 10 LEDs, non-reversed (the Vengeance DDR5 layout).

### VID acceptance gate

The info-block VID (`data[0..2]`, little-endian) must equal **`0x1B1C`** (Corsair). If it does not, the info read returns an error and the device is rejected.

### DDR4 / DDR5 color-path selection

The path is selected purely by the firmware **protocol-version byte** at info-block offset `data[28]`, compared against the threshold **4**:

| `protocol_version` | Path | Mechanics |
|--------------------|------|-----------|
| `>= 4` | Direct | block writes to `0x31`/`0x32`, framed packet, no commit |
| `< 4` | Effect/streaming | byte stream + commit `0x82←0x02` + busy poll |

This is a firmware-capability threshold, not the marketing DDR generation — a DDR4 stick on new enough firmware uses the direct path, and canvas/animation frames always use the direct path regardless.

### Config IDs (commit register `0x82`)

Written as the data byte of `REG_WRITE_CONFIGURATION` (`0x82`) to apply a streamed buffer:

| ID | Name | Commits |
|----|------|---------|
| `0x01` | `CONFIG_ID_EFFECT` | the 20-byte native-effect descriptor |
| `0x02` | `CONFIG_ID_COLOR_DATA` | the legacy RGBA color buffer |

### Status busy bit

The status register `0x30` returns a byte whose **bit 3 (mask `0x08`)** is the busy flag. The device is ready when `(status & 0x08) == 0`. After a commit it is polled up to **5 times at 10 ms intervals**.

### CRC-8 algorithm

All checksums use **CRC-8/SMBUS**: polynomial `0x07`, initial value `0x00`, **no** input reflection, **no** output reflection, no final XOR. Per byte, MSB-first:

```
crc = 0x00
for each input byte b:
    crc ^= b
    repeat 8 times:
        if crc & 0x80:  crc = (crc << 1) ^ 0x07   (8-bit wrap)
        else:           crc =  crc << 1            (8-bit wrap)
return crc
```

Known-answer values (used as conformance tests): `crc8([]) = 0x00`, `crc8([0x00]) = 0x00`, `crc8([0x01,0x02,0x03]) = 0x48`, `crc8([0xFF]) = 0xF3`.

### Mode enum (descriptor byte `[0]`)

`CorsairDramMode`, one byte:

| Value | Mode | Value | Mode |
|-------|------|-------|------|
| `0x00` | ColorShift | `0x06` | Rain |
| `0x01` | ColorPulse | `0x07` | Marquee |
| `0x03` | RainbowWave | `0x08` | Rainbow |
| `0x04` | ColorWave | `0x09` | Sequential |
| `0x05` | Visor | `0x10` | Static |

The UI surfaces only three of these, mapped from string IDs: `breathing` → ColorPulse (`0x01`), `rainbow_wave` → RainbowWave (`0x03`), `color_shift` → ColorShift (`0x00`). The `off` effect is **not** a hardware mode — it writes an all-black per-LED frame instead.

### Speed enum (descriptor byte `[1]`)

`CorsairDramSpeed`, one byte. Parsed from a string; any unrecognised string falls back to medium:

| Value | Speed | String |
|-------|-------|--------|
| `0x00` | Slow | `slow` |
| `0x01` | Medium | `medium` (also the fallback) |
| `0x02` | Fast | `fast` |

### Direction enum (descriptor byte `[3]`)

`CorsairDramDirection`, one byte. Any unrecognised string falls back to right:

| Value | Direction | String |
|-------|-----------|--------|
| `0x00` | Up | `up` |
| `0x01` | Down | `down` |
| `0x02` | Left | `left` |
| `0x03` | Right | `right` (also the fallback) |

### Brightness, random, and colors

In the native-effect descriptor, **brightness** is a single byte `0x00`–`0xFF` carried at both `[7]` and `[11]`; the device layer always sends `0xFF` (full). The **random flag** at `[2]` is inverted: `0x00` means random, `0x01` means not random; the device layer always sends not-random (`0x01`). `color1`/`color2` are each `R, G, B`; an unspecified color defaults to white `[0xFF, 0xFF, 0xFF]`.

---

## 4. Responses

All reads are register-addressed single bytes (`read_byte_data`); the controller returns no unsolicited data.

- **Info block:** 32 bytes via repeated reads of `0x40`, followed by a CRC8 byte at `0x42` matching `crc8(data[0..32])` (the apply continues even on mismatch — §2 Info-block read).
- **Detection sentinels:** read `0x43` → one of `0x1A`/`0x1B`/`0x1C`; read `0x44` → one of `0x01`/`0x03`/`0x04`. Any other value (or read error) fails detection.
- **Streamed-buffer CRC:** after streaming a payload, read `0x42`; the commit proceeds only if it equals `crc8(payload)`.
- **Busy-bit poll (ACK-after-commit):** after a commit (`0x82`), read `0x30` up to 5× at 10 ms; the device is ready once `(value & 0x08) == 0`. Direct block writes (`0x31`/`0x32`) have no poll — they apply on the SMBus ACK.

---

## 5. Polling & notifications

None — all access is host-initiated request/response. There is no interrupt or asynchronous channel; the busy bit is polled after a commit (see §2 Streamed RGBA color write and §4).

---

## Notes

- DDR4/DDR5 behavior is keyed on firmware `protocol_version`, not the physical memory generation — a DDR4 stick on newer firmware uses the direct path.
- CRC mismatches are non-fatal for the info-block read (logged) but **abort the apply** for streamed writes — a corrupted stream silently leaves LEDs unchanged.
- Native effects only ever send brightness `0xFF` and `random = false`; modes beyond breathing/rainbow_wave/color_shift exist in the enum but are not issued by the device layer.
- Requires SMBus access: the `i2c` group on Linux, PawnIO + Administrator on Windows (see [SMBus transport](../transports/smbus.md)).
