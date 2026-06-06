# HID++ Protocol

Logitech's proprietary protocol layered on top of standard USB HID. Used by modern Logitech peripherals (G502 X Plus, G PRO X TKL, G560, Lightspeed receiver).

**Credits:** reverse-engineered with reference to the [Solaar](https://github.com/pwr-Solaar/Solaar) project (GPL-2.0-or-later), files `base.py`, `hidpp10.py`, `hidpp20.py` by Daniel Pavel and contributors.

**Source:** `src/daemon/src/drivers/vendors/logitech/protocols/`

---

## Overview

Two generations coexist:

- **HID++ 1.0** — register-based; used by the G560 speaker and the Lightspeed receiver for device management.
- **HID++ 2.0** — feature-enumeration-based; used by modern mice and keyboards. Features are discovered at runtime.

Both share the same frame format and report IDs.

---

## Frame Format

```
Byte 0: Report ID     (0x10 = short/7 bytes, 0x11 = long/20 bytes)
Byte 1: Device number (0xFF = receiver, 1–6 = paired wireless device)
Byte 2: Sub-ID / command (1.0) or feature index (2.0)
Byte 3: Register address (1.0) or function + software ID (2.0)
Bytes 4–N: Parameters / data (zero-padded to frame length)
```

---

## HID++ 1.0

Register-based communication. Sub-IDs encode direction and the high bit of the register address:

| Sub-ID | Direction | Register bit 8 |
|--------|-----------|----------------|
| `0x80` | Write | 0 |
| `0x81` | Read | 0 |
| `0x82` | Write | 1 |
| `0x83` | Read | 1 |

Registers used by HaloDaemon:

| Register | Name |
|----------|------|
| `0x0002` | Device count (paired devices on receiver) |
| `0x02B5` | Receiver pairing info |

---

## HID++ 2.0

Feature-enumeration model. The device maintains a table of supported features identified by 16-bit codes. Feature indices are discovered at runtime via the ROOT feature (`0x0000`).

### Feature codes used

| Code | Name | Purpose |
|------|------|---------|
| `0x0000` | ROOT | Feature enumeration |
| `0x0001` | FEATURE_SET | List all features |
| `0x0003` | FIRMWARE_VERSION | Firmware info |
| `0x0005` | DEVICE_NAME | Device name string |
| `0x1000` | BATTERY_STATUS | Battery level + charging state |
| `0x1004` | UNIFIED_BATTERY | Unified battery interface |
| `0x2201` | ADJUSTABLE_DPI | DPI step configuration |
| `0x8010` | GKEY | G-key divert (G PRO X TKL) |
| `0x8060` | REPORT_RATE | Polling rate |
| `0x8071` | RGB_EFFECTS | Zone-based RGB (G502 X Plus) |
| `0x8080` | PER_KEY_LIGHTING | Per-key RGB (G PRO X TKL) |
| `0x8100` | ONBOARD_PROFILES | DPI profiles + button config |
| `0x8110` | MOUSE_BUTTON_SPY | Mouse button divert (G502 X Plus) |
| `0x1B04` | REPROG_CONTROLS_V4 | Button remapping |

---

## Wireless relay (Lightspeed receiver)

The Lightspeed receiver (PID `0xC547`) pairs up to six wireless devices (device index 1–6). HID++ 2.0 requests addressed to a device index are relayed transparently to the paired device. Connected/disconnected events arrive as unsolicited notifications and update the live device list.

---

## Onboard profiles

`ONBOARD_PROFILES` (`0x8100`) stores DPI steps and button configuration on-device in three memory regions:

- **Directory** (sector `0x0000`) — 4-byte entries; `0xFFFF` terminates the list.
- **RAM profiles** (sectors `0x0001`–`0x0005`) — writable live profiles.
- **ROM profiles** (sectors `0x0101`–`0x0501`) — read-only factory defaults.

Profile data is protected by CRC16 (`0x1021` polynomial, init `0xFFFF`). Writes failing the CRC check are silently discarded by the device. Key functions: `setCurrentProfile` (0x30), `memoryRead` (0x50), `memoryAddrWrite` (0x60), `memoryWrite` (0x70), `memoryWriteEnd` (0x80).

---

## Button remapping

Three mechanisms are used depending on the device:

| Feature | Device | Notes |
|---------|--------|-------|
| `REPROG_CONTROLS_V4` (`0x1B04`) | Most mice | Per-button divert; requires host mode |
| `GKEY` (`0x8010`) | G PRO X TKL | All-or-nothing global toggle |
| `MOUSE_BUTTON_SPY` (`0x8110`) | G502 X Plus | Mirrors presses; native action also fires (double-fire, see limitations) |

---

## Limitations

- G502 X Plus button remapping has a known double-fire issue: `setButtonDivert` (function `0x40` of `MOUSE_BUTTON_SPY`) is not yet implemented because the `button_id` encoding is not fully reverse-engineered. The native HID action fires alongside the mapped action.
