# Zotac Blackwell GPU RGB Protocol

Zotac SPECTRA 2.0 ARGB lighting protocol for the **Blackwell**-generation GeForce RTX GPU RGB controller, spoken over the GPU's internal [SMBus/I2C](../transports/smbus.md) bus.

**Credits:** reference implementation from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) by Adam Honse (CalcProgrammer1) et al. (GPL-2.0-or-later)

---

## Overview

The controller is an embedded RGB device on the NVIDIA GPU's I2C bus at address **`0x4B`**. It is a **register file**: the host stages a 16-byte block of effect parameters into registers **`0x20`–`0x2F`** with individual SMBus byte writes, then triggers a **commit** by writing `0x01` to register **`0x17`**. The firmware applies the staged registers on commit.

The model is **host-initiated request/response**; the controller never originates a transaction. Colors are natural **R, G, B** order, **unscaled** (raw `0x00`–`0xFF`). Speed and brightness are **0–100** integers written directly (the byte *is* the percentage — no `0–255` scaling).

Two timing constraints are part of the protocol: a **3 ms delay after every register byte write** (`_DELAY_US = 3000`) and a **10 ms delay after the commit write** (`_COMMIT_DELAY_US = 10000`) to let the firmware process the change.

Lighting is addressed as a set of **single-LED zones** selected by a zone-index register (`0x21`); there is no addressable per-pixel framebuffer. *How many* zones a board has, and their names, is device-specific (§3).

---

## 1. Register layout

The wire primitive is a single **SMBus write-byte-data** (`register, value`) or **read-byte-data** (`register`). Effect state is staged in the 16-register block `0x20`–`0x2F`:

| Register | Const | Field | Meaning |
|----------|-------|-------|---------|
| `0x20` | `_REG_FIXED` | fixed | always written `0x00` (header/marker) |
| `0x21` | `_REG_ZONE` | zone | target zone index (device-specific set, §3) |
| `0x22` | `_REG_MODE` | mode | effect mode id (§3) |
| `0x23` | `_REG_RED1` | color1.R | primary red (raw 0–255) |
| `0x24` | `_REG_GREEN1` | color1.G | primary green |
| `0x25` | `_REG_BLUE1` | color1.B | primary blue |
| `0x26` | `_REG_RED2` | color2.R | secondary red (two-color modes) |
| `0x27` | `_REG_GREEN2` | color2.G | secondary green |
| `0x28` | `_REG_BLUE2` | color2.B | secondary blue |
| `0x29` | `_REG_BRIGHTNESS` | brightness | 0–100 |
| `0x2A` | `_REG_SPEED` | speed | 0–100 |
| `0x2B` | `_REG_DIRECTION` | direction | `0x00` left / `0x01` right |
| `0x2C`–`0x2F` | `_REG_RESERVED_2C…2F` | reserved | reserved / unused |

### Control registers

| Register | Const | Purpose |
|----------|-------|---------|
| `0x10` | — | Read for detection (see §5) |
| `0x11` | `_REG_RELOAD` | Reload — defined but **not written** by the driver |
| `0x17` | `_REG_COMMIT` | Commit: write `0x01` to apply the staged `0x20`–`0x2F` block |

### Color encoding

Colors are natural **R, G, B**, unscaled. `color1` (`0x23`–`0x25`) is the primary/only color; `color2` (`0x26`–`0x28`) is the second color for two-color effects. Each zone is a single logical LED — there is no per-pixel buffer.

---

## 2. Operations

An update to one zone writes the full `0x20`–`0x2F` block then commits. Exact sequence:

```
for i in 0..16:
    write_byte_data(0x4B, 0x20 + i, regs[i])   # regs[0] (0x20) is always 0x00
    usleep(3000)                               # 3 ms per byte
write_byte_data(0x4B, 0x17, 0x01)              # commit
usleep(10000)                                  # 10 ms for firmware to process
```

| Operation | What it does |
|-----------|--------------|
| stage-and-commit | Write `0x20`–`0x2F` for one zone, then write `0x01` to `0x17` |

Every effect parameter — including brightness (`0x29`) — is **per zone**: the values staged apply to the zone named in `0x21`. There is no global-brightness or all-zones path in the protocol; to update several zones, stage + commit each in turn. The driver never reads effect state back.

---

## 3. Parameters

### I2C address & registers (protocol invariants)

| Constant | Value |
|----------|-------|
| I2C device address | `0x4B` |
| Register block base (`_REG_FIXED`) | `0x20` (16 registers, `0x20`–`0x2F`) |
| Commit register (`_REG_COMMIT`) | `0x17` (write `0x01`) |
| Reload register (`_REG_RELOAD`) | `0x11` (unused) |
| Detection register | `0x10` |
| Per-byte delay (`_DELAY_US`) | 3000 µs |
| Commit delay (`_COMMIT_DELAY_US`) | 10000 µs |

### Effect modes (register `0x22`)

Mode **ids** are protocol-level (reverse-engineered from Firestorm V5.0.0.012E); *which* ids a given board actually honours is device/firmware-specific:

| Mode | Value | Mode | Value |
|------|-------|------|-------|
| Static | `0x01` | Bokeh | `0x0A` |
| Breathe | `0x02` | Beacon | `0x0B` |
| Fade | `0x03` | Tandem | `0x18` |
| Wink | `0x04` | Tidal | `0x19` |
| Glide | `0x08` | Astra | `0x20` |
| Prism | `0x09` | Cosmic | `0x21` |
| | | Volta | `0x22` |

Additional ids (Flash, Shine, Random, Fusion) exist in firmware but may be non-functional depending on the board — a device property, not a protocol guarantee.

### Direction (register `0x2B`)

| Direction | Value |
|-----------|-------|
| Left | `0x00` |
| Right | `0x01` |

### Speed & brightness

Both are plain **0–100** integers written directly (`0x2A` speed, `0x29` brightness). No `0–255` scaling. A mode that doesn't use a parameter ignores its register on the device side.

### Zones (register `0x21`) — device-declared, not protocol

The protocol only defines that a zone is selected by its index in register `0x21`. The **number of zones and their names is per board** and must be discovered per device, not assumed. For example, the RTX 5080 AMP Extreme INFINITY exposes three single-LED zones — Logo (`0x00`), Side Bar (`0x01`), Infinity Mirror (`0x02`); a different Blackwell board (e.g. the RTX 5090 SOLID OC) will differ.

---

## 4. Responses

Register writes are fire-and-forget; the driver relies on SMBus transaction success, not a returned payload. The only read is the detection probe (§5). There is no state-readback command.

---

## 5. Detection

- **Bus:** the NVIDIA GPU's internal I2C bus (via NvAPI on Windows / the i2c-nvidia-gpu path on Linux). See [nvidia-gpu-sensors](nvidia-gpu-sensors.md) and [smbus](../transports/smbus.md).
- **Probe (protocol):** `read_byte_data(0x4B, 0x10)`; a **non-negative** result confirms a controller is present.
- **PCI match (device-declared):** gated on NVIDIA vendor `10DE` + Zotac subvendor `19DA` with a per-board `(device_id, subdevice_id)` table. That table grows as boards are added (it currently includes the RTX 5080 AMP Extreme INFINITY and the RTX 5090 SOLID OC) — the IDs are device data, not part of the wire protocol.

---

## 6. Polling & notifications

None. All access is host-initiated request/response; the controller never originates a transaction and there is no status stream.

---

## Notes

- **Timing is part of the protocol:** 3 ms after each of the 16 register byte writes, 10 ms after the commit. The transfers are individual SMBus byte writes, not a block write.
- Colors are **unscaled RGB**; speed/brightness are **0–100**, not 0–255.
- The reload register `0x11` is defined but never written — only `0x17` (commit) latches staged changes.
- **Everything board-specific stays out of the wire protocol.** Zone set/names, which modes actually work, any "mode X suppresses zone Y" firmware behaviour (e.g. Prism vs. an infinity-mirror zone on one board), and the PCI ID table are **device-declared** — a driver reads them from the device/descriptor and must never hard-code one board's layout into shared logic.
