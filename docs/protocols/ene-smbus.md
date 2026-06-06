# ENE SMBus Protocol

ENE Technology RGB controller protocol over SMBus I2C. Used for DRAM sticks and GPU RGB controllers that carry an ENE embedded controller.

**Credits:** reference implementation from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) by CalcProgrammer1 et al. (GPL-2.0-or-later): `ENESMBusInterface_i2c_smbus.cpp` and `ENESMBusController.cpp`.

**Source:** `src/daemon/src/drivers/vendors/asus/protocols/ene_smbus.rs`

---

## Overview

ENE controllers are addressed over the motherboard I2C buses via `/dev/i2c-N` on Linux or PawnIO on Windows. All register I/O uses a two-stage addressing scheme: write the 16-bit register address to SMBus command `0x00`, then read (command `0x81`) or write (command `0x01`).

| Device type | I2C addresses |
|-------------|---------------|
| DRAM | candidate list: `0x70`–`0x76`, `0x4F`, `0x66`, `0x67`, `0x39`–`0x3D`, … (after remapping) |
| GPU | `0x67` |

DRAM sticks start at broadcast address `0x77`. HaloDaemon remaps each discovered stick to a unique address from the `ENE_RAM_ADDRESSES` candidate list (see `src/daemon/src/drivers/vendors/asus/devices/ene_smbus.rs`) using `ENE_REG_SLOT_INDEX` (`0x80F8`) and `ENE_REG_I2C_ADDRESS` (`0x80F9`).

---

## Register map

| Register | Address | Purpose |
|----------|---------|---------|
| `ENE_REG_DEVICE_NAME` | `0x1000` | Firmware version string (16 bytes) |
| `ENE_REG_MICRON_CHECK` | `0x1030` | Micron module rejection probe |
| `ENE_REG_CONFIG_TABLE` | `0x1C00` | LED count and layout (64 bytes) |
| `ENE_REG_COLORS_DIRECT` | `0x8000` | Direct RGB buffer (v1) |
| `ENE_REG_COLORS_EFFECT` | `0x8010` | Effect RGB buffer (v1) |
| `ENE_REG_DIRECT` | `0x8020` | Direct mode enable (`0x01` = on) |
| `ENE_REG_MODE` | `0x8021` | Effect mode selection |
| `ENE_REG_SPEED` | `0x8022` | Effect animation speed |
| `ENE_REG_DIRECTION` | `0x8023` | Effect direction |
| `ENE_REG_APPLY` | `0x80A0` | Apply/commit trigger (`0x01`) |
| `ENE_REG_SLOT_INDEX` | `0x80F8` | DRAM slot index for address remapping |
| `ENE_REG_I2C_ADDRESS` | `0x80F9` | Target address for remapping |
| `ENE_REG_COLORS_DIRECT_V2` | `0x8100` | Direct RGB buffer (v2) |
| `ENE_REG_COLORS_EFFECT_V2` | `0x8160` | Effect RGB buffer (v2) |

Wire order for color data is **R, B, G** (green and blue are swapped relative to standard RGB).

---

## Modes

**Direct mode** (canvas engine): write RGB triples to the direct register (`0x8000` or `0x8100`), set `ENE_REG_DIRECT = 0x01`, then commit with `ENE_REG_APPLY = 0x01`.

**Effect mode** (hardware animation): write color data to the effect register, set `ENE_REG_MODE`, `ENE_REG_SPEED`, `ENE_REG_DIRECTION`, then commit.

---

## Effects

| Effect | Code |
|--------|------|
| Off | `0x00` (direct mode, all black) |
| Static | `0x01` |
| Breathing | `0x02` |
| Flashing | `0x03` |
| Spectrum Cycle | `0x04` |
| Rainbow | `0x05` |
| Spectrum Cycle Breathing | `0x06` |
| Chase Fade | `0x07` |
| Spectrum Cycle Chase Fade | `0x08` |
| Chase | `0x09` |
| Spectrum Cycle Chase | `0x0A` |
| Spectrum Cycle Wave | `0x0B` |
| Chase Rainbow Pulse | `0x0C` |
| Random Flicker | `0x0D` |
| Double Fade | `0x0E` |

---

## Controller detection

Before writing, HaloDaemon verifies an ENE controller is present by:
1. Confirming the address ACKs a quick-write.
2. Reading bytes `0xA0`–`0xAF` and verifying the incrementing pattern `0x00, 0x01, ..., 0x0F`.
3. Reading `ENE_REG_MICRON_CHECK` (`0x1030`) and rejecting addresses that spell `"Micron"` (Micron DRAM shares the I2C address space with a different protocol).

---

## Limitations

- Requires the `i2c` group on Linux (see [SMBus transport](../transports/smbus.md)) and PawnIO + Administrator on Windows.
