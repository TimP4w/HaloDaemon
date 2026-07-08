# ENE SMBus Protocol

ENE Technology RGB controller protocol over SMBus/I2C, used by ASUS Aura DRAM modules and ASUS GPU RGB controllers.

**Credits:** reference implementation from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) by Adam Honse (CalcProgrammer1) et al. (GPL-2.0-or-later): `ENESMBusInterface_i2c_smbus.cpp` and `ENESMBusController.cpp`.

---

## Overview

At the protocol level this is an ENE embedded RGB controller addressed over an [SMBus](../transports/smbus.md) I2C bus. There is no fixed-size packet; the wire unit is a single SMBus transaction (quick-write, byte read/write, word write, or block write). Every logical register access is **two-stage**: first write the byte-swapped 16-bit register address to SMBus command `0x00`, then read or write the data at a second SMBus command (`0x81` to read, `0x01` to write one byte, `0x03` to write a block). The controller auto-increments its internal register pointer across a block transfer.

The model is **host-initiated request/response**: the host sets a register address then reads or writes. The controller never originates a transaction. Color data uses **R,B,G** wire order. Two register-layout variants (v1 / v2) are selected by the firmware-version string read at init.

---

## 1. Packet layout

There is no multi-byte packet. The wire primitives are individual SMBus transactions; higher-level operations are sequences of these (see §2).

### Two-stage register address

The 16-bit register number is **byte-swapped** before it is sent as the 16-bit word value to SMBus command `0x00`:

```
wire_word = ((reg << 8) & 0xFF00) | ((reg >> 8) & 0x00FF)
```

i.e. the high and low bytes of `reg` are exchanged. Worked examples:

| Register | `reg` | `wire_word` sent to cmd `0x00` |
|----------|-------|--------------------------------|
| `ENE_REG_MODE` | `0x8021` | `0x2180` |
| `ENE_REG_APPLY` | `0x80A0` | `0xA080` |
| `ENE_REG_DEVICE_NAME` | `0x1000` | `0x0010` |

### Register read transaction

```
1. write_word_data(addr, cmd=0x00, val=byteswap(reg))    set register pointer
2. read_byte_data (addr, cmd=0x81)             → returns one data byte
```

### Register write transaction

```
1. write_word_data(addr, cmd=0x00, val=byteswap(reg))    set register pointer
2. write_byte_data(addr, cmd=0x01, val)                  write one data byte
```

### Block write transaction

Color buffers are written in chunks of at most **32 bytes** (`MAX_BLOCK`). The controller auto-increments its pointer within a chunk, so each chunk re-points the address to `reg + offset`:

```
for each 32-byte chunk starting at offset:
  write_word_data (addr, 0x00, byteswap(reg + offset))
  write_block_data(addr, cmd=0x03, chunk)        SMBus block write
```

If `write_block_data` is unsupported/fails on the platform, it falls back to one byte at a time within that chunk:

```
  for each byte j in chunk:
    write_word_data(addr, 0x00, byteswap(reg + offset + j))
    write_byte_data(addr, cmd=0x01, byte)
```

### Color buffer layout

A color buffer is always exactly `led_count * 3` bytes. Each RGB triple is emitted in **R,B,G** order (green and blue swapped — see §3). Excess input colors are dropped; a short input is right-padded with black (`00 00 00`).

---

## 2. Functions

All transactions use the device's I2C address `addr` and the two-stage scheme above. Below, `read(R)` = a register-read transaction at reg `R`, `write(R,v)` = a register-write transaction, `block(R,buf)` = a block-write of `buf` starting at reg `R`.

| Function | Transactions issued | Params | Required sequence / notes |
|----------|---------------------|--------|----------------------------|
| `test` | probe reads (see subsection) | — | Returns whether an ENE controller is present |
| `build_device` | `read(0x1000+i)` ×16, `read(0x1C00+i)` ×64 | — | Reads version + config; errors if led_count = 0 |
| `set_direct_mode(true)` | 4 writes (see subsection) | — | Enter direct (software) control |
| `set_direct_mode(false)` | `write(0x8020,0x00)`, `write(0x80A0,0x01)` | — | Leave direct control |
| `apply_static_direct` | direct-colour block (see subsection) | `r,g,b` | Solid color; one atomic I2C batch |
| `apply_colors_direct` | direct-colour block (see subsection) | `colors[]` | Per-LED frame; one atomic I2C batch |
| `write_frame_colors` | `block(direct_reg, buf)` only | `colors[]` | No mode/apply — device must already be in direct mode |
| `set_effect_colors` | `block(effect_reg, buf)`, `write(0x80A0,0x01)` | `colors[]` | Load effect color buffer then apply |
| `set_mode` | 4 writes (see subsection) | `mode,speed,direction` | Hardware effect |
| `remap_dram_addresses` | quick-writes + per-stick writes (see subsection) | candidate addr list | DRAM address assignment, pre-scan |

Each protocol method runs all its transactions inside one blocking batch so the I2C lock is held across the whole sequence.

### Two-stage register read (`read_reg`)

1. `write_word_data(addr, 0x00, byteswap(reg))` — point the controller at register `reg`.
2. `read_byte_data(addr, 0x81)` — read and return the data byte.

### Two-stage register write (`write_reg` + block)

Single byte:

1. `write_word_data(addr, 0x00, byteswap(reg))`.
2. `write_byte_data(addr, 0x01, val)`.

Multi-byte (block): split `buf` into ≤32-byte chunks. For each chunk at `offset`:

1. `write_word_data(addr, 0x00, byteswap(reg + offset))`.
2. `write_block_data(addr, 0x03, chunk)`; if that fails, fall back to looping the single-byte write above for each byte at `reg + offset + j`.

### 7-step direct-colour block — `apply_static_direct` / `apply_colors_direct` (`apply_direct_color_block`)

The whole sequence runs as one atomic I2C batch so no concurrent transfer can interleave. Exact order:

1. `write(0x8021, 0x01)` — `ENE_REG_MODE` = Static.
2. `write(0x80A0, 0x01)` — `ENE_REG_APPLY`.
3. `write(0x8020, 0x01)` — `ENE_REG_DIRECT` = on.
4. `write(0x80A0, 0x01)` — `ENE_REG_APPLY`.
5. `block(direct_reg, buf)` — the R,B,G color buffer (`direct_reg` = `0x8000` v1 or `0x8100` v2, see §3).
6. `write(0x8020, 0x01)` — `ENE_REG_DIRECT` = on (re-assert).
7. `write(0x80A0, 0x01)` — `ENE_REG_APPLY` (commit).

DIRECT is asserted **before** the color block (steps 3–4) because some controllers only latch direct-mode writes while DIRECT is already high; it is re-asserted after (steps 6–7) to commit the frame.

### `set_direct_mode(enable)`

For `enable = true`:

1. `write(0x8021, 0x01)` — `ENE_REG_MODE` = Static. Written first so a controller that booted in Off mode after sleep/resume exits it; otherwise `ENE_REG_DIRECT` writes are silently ignored.
2. `write(0x80A0, 0x01)` — `ENE_REG_APPLY`.
3. `write(0x8020, 0x01)` — `ENE_REG_DIRECT` = on.
4. `write(0x80A0, 0x01)` — `ENE_REG_APPLY`.

For `enable = false`: just `write(0x8020, 0x00)` then `write(0x80A0, 0x01)`.

### `set_mode(mode, speed, direction)` — hardware effect

1. `write(0x8021, <mode>)` — `ENE_REG_MODE` (effect id, see §3).
2. `write(0x8022, <speed>)` — `ENE_REG_SPEED`.
3. `write(0x8023, <direction>)` — `ENE_REG_DIRECTION`.
4. `write(0x80A0, 0x01)` — `ENE_REG_APPLY`.

The device layer drives `breathing` / `spectrum_wave` / `off` like this:

1. `set_direct_mode(false)` (for `breathing` and `spectrum_wave`; `off` skips it).
2. For `breathing`: `set_effect_colors(colors)` to preload the effect buffer.
3. `set_mode(<mode>, <speed>, 0)` — direction is always `0`.

### DRAM address remap (`sync_remap_dram_addresses`)

Runs as one blocking batch on the broadcast address `0x77` before the normal bus scan. DRAM sticks all power up answering `0x77`; each is moved to a unique free address from the candidate list (§3):

1. For `slot` in `0..8`:
   1. `write_quick(0x77)` — if `0x77` no longer ACKs, **stop** (no unremapped sticks left).
   2. Advance through the candidate address list until a candidate **NAKs** a `write_quick` (a NAK means that address is free). If the list is exhausted, return.
   3. `write(0x80F8, <slot>)` on address `0x77` — `ENE_REG_SLOT_INDEX`.
   4. `write(0x80F9, <target << 1>)` on address `0x77` — `ENE_REG_I2C_ADDRESS`, written in 8-bit (left-shifted) form. This moves that stick to `target`.

### Controller-detection probe (`probe_ene_controller`, via `test`)

Returns true only if all three checks pass:

1. **Liveness:** `read_byte(addr)` OR `read_byte_data(addr, 0x00)` succeeds.
2. **Incrementing pattern:** for `i` in `0..0x10`, read raw SMBus command byte `0xA0 + i`; the value must equal `i` (so `0xA0→0x00, 0xA1→0x01, … 0xAF→0x0F`). Any mismatch ⇒ not an ENE controller.
3. **Micron rejection:** read 6 bytes via two-stage reads from `ENE_REG_MICRON_CHECK` (`0x1030`…`0x1035`); if they spell ASCII `"Micron"`, reject (Micron DRAM SPD shares the I2C address space with a different protocol).

---

## 3. Parameters

### Register map

| Register | Address | Purpose |
|----------|---------|---------|
| `ENE_REG_DEVICE_NAME` | `0x1000` | Firmware version string (16 bytes, NUL-terminated) |
| `ENE_REG_MICRON_CHECK` | `0x1030` | Micron-rejection probe (6 bytes) |
| `ENE_REG_CONFIG_TABLE` | `0x1C00` | Config table incl. LED count (64 bytes) |
| `ENE_REG_COLORS_DIRECT` | `0x8000` | Direct color buffer (v1 layout) |
| `ENE_REG_COLORS_EFFECT` | `0x8010` | Effect color buffer (v1 layout) |
| `ENE_REG_DIRECT` | `0x8020` | Direct-mode enable (`0x01` on / `0x00` off) |
| `ENE_REG_MODE` | `0x8021` | Effect mode selector |
| `ENE_REG_SPEED` | `0x8022` | Effect animation speed |
| `ENE_REG_DIRECTION` | `0x8023` | Effect direction |
| `ENE_REG_APPLY` | `0x80A0` | Apply/commit trigger (write value `0x01`) |
| `ENE_REG_SLOT_INDEX` | `0x80F8` | DRAM slot index for address remap |
| `ENE_REG_I2C_ADDRESS` | `0x80F9` | DRAM remap target (written `addr << 1`) |
| `ENE_REG_COLORS_DIRECT_V2` | `0x8100` | Direct color buffer (v2 layout) |
| `ENE_REG_COLORS_EFFECT_V2` | `0x8160` | Effect color buffer (v2 layout) |

Constants: apply value `0x01`; DRAM broadcast address `0x77`.

### SMBus command bytes

| Command | Operation | Used for |
|---------|-----------|----------|
| `0x00` | write word | Set register pointer (byte-swapped reg) |
| `0x01` | write byte | Write one data byte |
| `0x03` | write block | Write a color chunk (≤ 32 bytes) |
| `0x81` | read byte | Read one data byte |

### Color encoding — R, B, G wire order

Color data is **not** sent as RGB. Each LED's three bytes are reordered: input `[R, G, B]` is written to the wire as `[R, B, G]` (green and blue swapped; red unchanged). Examples:

- `[0xAA,0xBB,0xCC]` → wire `AA CC BB`.
- Two LEDs `[0xAA,0xBB,0xCC],[0x11,0x22,0x33]` → wire `AA CC BB 11 33 22`.
- One LED padded to a 3-LED buffer → `AA CC BB 00 00 00 00 00 00`.

A buffer is always padded/truncated to exactly `led_count * 3` bytes.

### Register-layout variants (v1 vs v2) (`apply_version_layout`)

The direct/effect color-buffer registers and the LED-count field offset depend on the firmware-version string read from `ENE_REG_DEVICE_NAME`:

| Firmware version(s) | LED-count config offset | `direct_reg` | `effect_reg` |
|---------------------|-------------------------|--------------|--------------|
| `LED-0116`, `DIMM_LED-0102`, `DIMM_LED-0103`, `AUMA0-E8K4-0101` | `0x02` | `0x8000` (v1) | `0x8010` (v1) |
| `AUDA0-E6K5-0101`, `AUMA0-E6K5-0104`, `AUMA0-E6K5-0105`, `AUMA0-E6K5-0106` | `0x02` | `0x8100` (v2) | `0x8160` (v2) |
| `AUMA0-E6K5-0107`, `-1110`, `-1111`, `-1107`, `-0008`, `-1113`, `-1114` (GPU controllers) | `0x03` | `0x8100` (v2) | `0x8160` (v2) |
| any other / unknown string | `0x02` (assumed v1) | `0x8000` (v1) | `0x8010` (v1) |

The LED count is `max(config[offset], config[0x03])` clamped to the range **0…30** (a value of 0 makes `build_device` fail).

### Effect modes (`ENE_REG_MODE` value)

| Mode | Value | Mode | Value |
|------|-------|------|-------|
| Off | `0x00` | SpectrumCycleBreathing | `0x06` |
| Static | `0x01` | ChaseFade | `0x07` |
| Breathing | `0x02` | SpectrumCycleChaseFade | `0x08` |
| Flashing | `0x03` | Chase | `0x09` |
| SpectrumCycle | `0x04` | SpectrumCycleChase | `0x0A` |
| Rainbow | `0x05` | SpectrumCycleWave | `0x0B` |
| | | ChaseRainbowPulse | `0x0C` |
| | | RandomFlicker | `0x0D` |
| | | DoubleFade | `0x0E` |

The device layer exposes only three of these as user effects: `breathing` → Breathing (`0x02`), `spectrum_wave` → SpectrumCycleWave (`0x0B`), `off` → Off (`0x00`). Static color and per-LED frames use **direct mode**, not an effect value.

### Effect speeds (`ENE_REG_SPEED` value)

| Speed | Value |
|-------|-------|
| Fastest | `0x00` |
| Fast | `0x01` |
| Normal | `0x02` |
| Slow | `0x03` |
| Slowest | `0x04` |

`ENE_REG_DIRECTION` is always written `0x00` by the driver.

### Addresses

| Use | Address(es) |
|-----|-------------|
| DRAM broadcast (pre-remap) | `0x77` |
| DRAM remap candidate list (in order) | `0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x4F, 0x66, 0x67, 0x39, 0x3A, 0x3B, 0x3C, 0x3D` |
| GPU controller | `0x67` |

The remap target is written to `ENE_REG_I2C_ADDRESS` as `target << 1` (8-bit form).

---

## 4. Responses

### Register reads

A read transaction returns one data byte from SMBus command `0x81` after the address set. `build_device` issues 16 sequential reads from `0x1000+i` (NUL-terminated → version string) then 64 reads from `0x1C00+i` (config table).

### Config-table fields

`config[0x02]` and `config[0x03]` hold LED counts; which one is authoritative depends on the firmware variant (§3). The driver takes `max()` of the two and clamps to 30. A clamped result of 0 makes `build_device` return an error.

### Controller-detection probe responses (see §2 probe)

- Command bytes `0xA0…0xAF` must read back the incrementing pattern `0x00…0x0F`.
- `ENE_REG_MICRON_CHECK` (`0x1030`) returning ASCII `"Micron"` causes rejection.

### ACK / NAK semantics (remap)

`remap_dram_addresses` relies on `write_quick` ACK/NAK rather than read data:

- Broadcast `0x77` **ACK** ⇒ at least one stick is still unremapped.
- A candidate address that **NAKs** a `write_quick` ⇒ that address is free to assign.

No color, mode, or apply write returns an ACK payload that the driver inspects.

---

## 5. Polling & notifications

None — all access is host-initiated request/response. The controller never originates a transaction; the host sets registers and reads results on demand. There is no periodic status report.

---

## Notes

- LED count is clamped to a maximum of 30 regardless of the config value.
- Unrecognised firmware strings fall back to v1 registers (`0x8000` / `0x8010`) and may select the wrong layout on an unknown controller.
- `ENE_REG_DIRECTION` is never varied by the driver (always `0x00`).
- DRAM remap stops after 8 slots and is also bounded by the 15-entry candidate-address list.
- `write_frame_colors` deliberately omits mode/apply for animation throughput; the device must already be in direct mode (set once at init via `set_direct_mode(true)`).
