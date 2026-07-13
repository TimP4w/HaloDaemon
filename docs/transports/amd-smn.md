# AMD SMN Transport

Windows access to the AMD on-die **System Management Network (SMN)** via PawnIO's `AMDFamily17.bin` kernel module. Used by the AMD Ryzen CPU sensor driver to read Zen-family (17h/19h/1Ah) thermal registers.

**Platform:** Windows only

**Credits:** the register map and decode are derived from [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) (`Amd17Cpu.cs`, MPL-2.0). The `AMDFamily17.bin` blob is a [PawnIO module](https://github.com/namazso/PawnIO_modules) (LGPL-2.1-or-later, © namazso).

---

## Overview

AMD Ryzen CPUs expose temperature and telemetry through the SMN — an internal register fabric reached over PCI config space (device 18h, function 0). Reading it from user space needs a kernel-mode driver, so HaloDaemon goes through [PawnIO](https://pawnio.eu/), a signed kernel driver, exactly as the [LpcIO](lpcio.md) and chipset-SMBus transports do.

The SMN window is a single 32-bit indexed register read: write an offset, read a 32-bit value. The `AMDFamily17.bin` module performs the index/data dance and the required PCI-bus locking internally.

---

## Operations

| Method | PawnIO function | Purpose |
|--------|-----------------|---------|
| `read_smn(offset)` | `ioctl_read_smn` | Read one 32-bit SMN register at `offset` |

Only SMN reads are used. The module also exposes `ioctl_read_msr` (power/clock telemetry in LibreHardwareMonitor), which this transport deliberately does not surface — CPU temperatures need SMN only.

---

## Registers read by the sensor driver

| Register | Offset | Meaning |
|----------|--------|---------|
| `THM_TCON_CUR_TMP` | `0x00059800` | Package control temperature (Tctl/Tdie), CUR_TEMP in bits [31:21] |
| CCD temperature base (Zen 4/5) | `0x00059B08` | First CCD Tdie; CCD *i* at `base + i*4` |
| CCD temperature base (Zen 2/3) | `0x00059954` | First CCD Tdie; CCD *i* at `base + i*4` |

Decode math lives in [`vendors/amd/protocols/ryzen.rs`](../../src/daemon/src/drivers/vendors/amd/protocols/ryzen.rs); see [the AMD Ryzen sensor notes](#decode-summary) below.

### Decode summary

- **Tctl/Tdie:** `cur_temp = (raw >> 21) * 0.125 °C`. If `RANGE_SEL` (bit 19) or `TJ_SEL` (bits [17:16] both set) is asserted, subtract 49 °C. Zen 2+ desktop parts report Tctl and Tdie identically, so a single `Core (Tctl/Tdie)` sensor is exposed.
- **Per-CCD Tdie:** `temp = (raw & 0xFFF) * 0.125 − 305 °C`. A zero raw value means the CCD slot is unpopulated; a result ≥ 125 °C is rejected. The base offset differs between generations (Raphael/Granite Ridge vs. Matisse/Vermeer).
- **CCDs Max / Average:** computed across populated CCDs, and only emitted on parts with more than one CCD.

---

## CPU detection

The driver only opens the module after CPUID confirms an `AuthenticAMD` vendor and a family of `0x17`, `0x19`, or `0x1A`. The CPU **model** selects the CCD base offset and whether per-CCD registers exist (Threadripper 3000 `0x31`, Zen 2 `0x71`, Zen 3 `0x21`, Zen 4 `0x61`, Zen 5 `0x44`).

---

## Module loading

The non-elevated daemon sends only typed `OpenAmdSmn` and `ReadSmn` requests to `halod-broker`. The elevated broker maps those requests to `AMDFamily17.bin` and `ioctl_read_smn`; module and function names never cross the RPC boundary. `PawnIOLib.dll` is resolved only from trusted install locations, and the blob is loaded only from beside the broker executable (or `pwnio/` next to it). If PawnIO or the blob is unavailable, the AMD CPU sensor is silently unavailable.

---

## Limitations

- Windows only — excluded from Linux builds at the compiler level. (On Linux, CPU temperatures come from the [hwmon transport](hwmon.md) via `k10temp`.)
- Requires PawnIO installed; only the on-demand `halod-broker` helper runs as LocalSystem/Administrator.
- SMN reads touch PCI config space, the same window the chipset-SMBus transports use; the PawnIO module serialises PCI access internally, but heavy concurrent chipset-SMBus activity can momentarily delay a poll.
