# ASUS Aura USB Protocol

ASUS Aura USB protocol for motherboard RGB headers and controllers.

**Credits:** reference implementation from [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) by Martin Hartl and contributors (GPL-2.0-or-later): `AsusAuraUSBController` / `AsusAuraMainboardController`.

**Source:** `src/daemon/src/drivers/vendors/asus/protocols/aura_usb.rs` (protocol constants, packet layout); `src/daemon/src/drivers/vendors/asus/devices/aura_usb.rs` (device driver)

---

## Overview

The ASUS Aura USB controller presents as a standard USB HID device (VID `0x0B05`). HaloDaemon uses 65-byte raw HID writes with no report-ID prefix — byte 0 is always `0xEC` (the Aura header byte). This matches the raw passthrough mode used by OpenRGB.

---

## Supported PIDs

All with VID `0x0B05`:

| PID | Notes |
|-----|-------|
| `0x1866`, `0x1867`, `0x1872` | ASUS Aura USB |
| `0x18A3`, `0x18A5`, `0x18F3` | ASUS Aura USB |
| `0x1939`, `0x19AF`, `0x1A30`, `0x1A6C` | ASUS Aura USB |
| `0x1AA6`, `0x1B3B` | ASUS X870E |
| `0x1BED` | ASUS Aura USB |

---

## Command structure

Each HID write is 65 bytes. Byte 0 is always `0xEC`. Byte 1 selects the command.

| Command | Byte 1 | Purpose |
|---------|--------|---------|
| Firmware | `0x82` | Request firmware version string |
| Config | `0xB0` | Read channel configuration table |
| Direct | `0x40` | Stream per-LED colors to an ARGB channel |
| SetMode | `0x35` | Set effect mode on a channel |
| AddrEffect | `0x3B` | Set native effect on an ARGB effect channel |
| StopGen2 | `0x52` | Disable legacy gen-2 continuous-cycle mode |

---

## Direct mode packet layout

```
Byte 0:   0xEC
Byte 1:   0x40 (CMD_DIRECT)
Byte 2:   direct_channel | (0x80 if last packet in sequence)
Byte 3:   LED offset (start index for this packet)
Byte 4:   LED count in this packet (max 20)
Bytes 5…: R, G, B triples
```

Up to 20 LEDs per packet (20 × 3 = 60 bytes fits in the 65-byte frame). Multi-packet sequences set bit `0x80` on the channel byte of the final packet to signal "apply now".

---

## Channel addressing

- **Effect channel** — 1-indexed; channel 0 is fixed mainboard LEDs (intentionally skipped to avoid corrupting the Q-Code POST display).
- **Direct channel** — 0-indexed; maps 1:1 to ARGB headers.

---

## Supported native effects

| Effect | Mode byte |
|--------|-----------|
| Off | `0x00` |
| Breathing | `0x02` |
| Spectrum Cycle | `0x04` |
| Rainbow Wave | `0x05` |
| Direct (canvas) | `0xFF` |

---

## Initialization sequence

1. Send `StopGen2` (`0x52 0x53 0x00 0x01`) to disable legacy continuous-cycle mode.
2. Read firmware version string.
3. Read 60-byte config table to discover ARGB channel count and per-channel LED counts.
4. Send `SetMode` with `MODE_DIRECT` (`0xFF`) to all channels.

---

## Limitations

- LED count per channel is read from the config table; if reported as `0`, HaloDaemon defaults to 30 LEDs per channel.
- Effect speed is not configurable.
