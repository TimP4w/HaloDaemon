# Philips Ambiglow Protocol

USB control transfer protocol for the ENE Technology RGB controller embedded in the Philips Evnia 49 monitor's Ambiglow rear LEDs.

**Credits:** reverse-engineered by TimP4w/HaloDaemon (this is probably why it doesn't work correctly :D)

**Source:** `src/daemon/src/drivers/vendors/philips/protocols/philips_evnia.rs` (`PhilipsAmbiglowProtocol`); `src/daemon/src/drivers/vendors/philips/devices/evnia_49_ambiglow.rs`

---

## Overview

The Ambiglow rear LEDs are driven by a **separate USB device** on the same monitor, distinct from the DDC/CI interface used for brightness and crosshair control. It presents as an ENE Technology RGB controller (VID `0x0CF2`, PID `0xB201`) and is accessed via USB vendor control transfers — not SMBus.

| Field | Value |
|-------|-------|
| VID | `0x0CF2` |
| PID | `0xB201` |
| Interface | 0 |
| Transport | [USB control](../transports/usb-control.md) |

---

## Control transfer format

All writes use a USB vendor control transfer with the register address in `wIndex`:

| Field | Value |
|-------|-------|
| `bmRequestType` | `0x40` (vendor / host-to-device / device recipient) |
| `bRequest` | `0x80` |
| `wValue` | `0` |
| `wIndex` | Target register address (16-bit) |
| `data` | Payload bytes (1 or 3 bytes depending on register) |

---

## Register map

### Master enable

| Address | Value | Purpose |
|---------|-------|---------|
| `0x0023` | `0x04` | Arm the LED engine |
| `0x0023` | `0x00` | Disable all LEDs |

### Zone configuration banks

Four zones, each with a 16-byte bank starting at:

| Zone | Bank base |
|------|-----------|
| Zone 0 | `0xE020` |
| Zone 1 | `0xE030` |
| Zone 2 | `0xE040` |
| Zone 3 | `0xE050` |

Within each bank:

| Offset | Register | Purpose |
|--------|----------|---------|
| `+0x00` | Reserved | Write `0x00` |
| `+0x01` | Mode | `0x00`=off, `0x01`=user color |
| `+0x02`–`+0x03` | Reserved | Write `0x00` |
| `+0x09` | Brightness | `0x00`=Bright, `0x02`=Brighter, `0x04`=Brightest |
| `+0x0F` | Commit | Write `0x01` to apply zone |

### Color registers

Each zone has an RGB triple at a fixed address (3-byte write):

| Zone | Color base address |
|------|--------------------|
| Zone 0 | `0xE980` |
| Zone 1 | `0xE983` |
| Zone 2 | `0xE986` |
| Zone 3 | `0xE989` |

---

## Full apply sequence (29 transfers)

```
write 0x0023 ← [0x04]          (master enable)
for each zone 0–3:
    write bank+0x00 ← [0x00]   (reserved)
    write bank+0x01 ← [mode]
    write bank+0x02 ← [0x00]
    write bank+0x03 ← [0x00]
    write bank+0x09 ← [brightness]
for each zone 0–3:
    write color_base ← [R, G, B]
for each zone 0–3:
    write bank+0x0F ← [0x01]   (commit)
```

For per-zone canvas updates, only the color write and the zone's commit register are sent (2 transfers instead of 29).

---
