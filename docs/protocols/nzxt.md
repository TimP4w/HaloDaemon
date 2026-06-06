# NZXT Protocol

NZXT's proprietary USB HID protocol. No public specification exists.

**Credits:** reverse-engineered from [liquidctl](https://github.com/liquidctl/liquidctl) (GPL-3.0, `kraken3.py`) and the Linux kernel [`nzxt-smart2`](https://github.com/torvalds/linux/blob/master/drivers/hwmon/nzxt-smart2.c) driver (GPL-2.0-or-later).

**Source:** `src/daemon/src/drivers/vendors/nzxt/protocols/`

---

## Overview

Both the Kraken AIO and the Control Hub share the same base wire format. All communication uses 64-byte USB HID reports. Command packets start with a 1–2 byte command ID; response packets echo the same ID in the first two bytes for matching.

---

## Base commands

| Command bytes | Response prefix | Purpose |
|---------------|-----------------|---------|
| `[0x10, 0x02]` | `[0x11, 0x02]` | Firmware version (bytes 0x11, 0x12, 0x13) |
| `[0x20, 0x03]` | `[0x21, 0x03]` | Accessory detection — channel count at byte 14, per-channel accessory IDs from byte 15 |

---

## Kraken commands

### Ring LED update

The ring has 40 physical wire slots but only 24 carry live data (slots 0–23, clockwise from 12-o'clock). The buffer is GRB-encoded.

```
Prefix:  [0x26, 0x14]
Byte 2:  channel (0x01 = ring, 0x02 = external accessory)
Bytes 3–122: 120-byte slot buffer (40 slots × 3 bytes GRB)
```

### Fan / pump duty

```
[0x72, channel, duty_percent]
```

`duty_percent` range: 20–100 for pump, 0–100 for fan.

### Temperature fan profile

A 40-entry lookup table (temperatures 20–59 °C → duty values). HaloDaemon interpolates a user-defined curve onto this table and sends it in one packet.

### LCD image upload

LCD frames are uploaded via HID control packets and a USB bulk-OUT channel, Q565-encoded, streamed as GIF memory buckets. The Kraken LCD brightness is set with a separate single-byte command.

### Sensor read

The device sends periodic status packets. Liquid temperature is a big-endian `u16` (high byte = integer °C, low byte = tenths). Pump RPM and fan RPM are also included.

---

## Control Hub commands

| Operation | Notes |
|-----------|-------|
| Fan RPM read | Included in periodic status reports |
| Fan duty set | Write duty per channel |
| Fan type detection | 0 = no fan, 1 = DC, 2 = PWM |
| RGB accessory enumeration | Uses the base `detect_accessories()` command |

---

## Limitations

- The Kraken ring uses only 24 of 40 wire slots, writing to unused slots has no visible effect.
- The `read_matching` helper retries up to a fixed attempt limit; very high unsolicited packet rates can cause matching to fail.
