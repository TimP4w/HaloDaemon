# SteelSeries Arctis Protocol

SteelSeries vendor HID protocol for the Arctis Nova Pro Wireless headset family.

**Credits:** based on the [linux-arctis-manager](https://github.com/elegos/Linux-Arctis-Manager) project (GPL-3.0).

**Source:** `src/daemon/src/drivers/vendors/steelseries/devices/arctis_nova_pro_wireless.rs`

---

## Overview

| Field | Value |
|-------|-------|
| VID | `0x1038` |
| PID (Nova Pro Wireless) | `0x12E0` |
| PID (Nova Pro X Wireless) | `0x12E5` |
| HID interface | 4 |
| Packet size | 64 bytes |

HID interface 4 is opened. On Linux, `hidapi` prepends a `0x00` report-ID byte; the driver strips it before parsing.

---

## Status polling

Two command/response pairs are polled every 250 ms:

### Status poll `[0x06, 0xB0]`

| Byte offset | Field | Encoding |
|-------------|-------|---------|
| `0x06` | Headset battery | 0–8 → 0–100% |
| `0x07` | Dock battery | 0–8 → 0–100% |
| `0x08` | Noise cancellation level | 0–10, ×10 for % |
| `0x09` | Mic muted | non-zero = muted |
| `0x0A` | NC mode | 0=Off, 1=Transparent, 2=On |
| `0x0C` | Auto-off timeout | 0–6 (see table below) |
| `0x0D` | Wireless mode | 0=Speed, 1=Range |
| `0x0F` | Power status | 0x01=Offline, 0x02=Charging, 0x08=Online |

### Settings poll `[0x06, 0x20]`

| Byte offset | Field | Encoding |
|-------------|-------|---------|
| `0x05` | Microphone gain | 0x01=Low, 0x02=High |
| `0x09` | EQ preset | 0–4 |
| `0x35` | Sidetone | 0–3 |

---

## Audio controls

All write commands follow this pattern: send the command packet → send `[0x06, 0x09]` (persist) → suppress polling for 3 seconds.

| Control | Command | Values |
|---------|---------|--------|
| NC mode | `[0x06, 0x47, raw, 0x00, raw]` | 0=Off, 1=Transparent, 2=NC |
| Sidetone | `[0x06, 0x39, raw]` | 0–3 |
| Wireless mode | `[0x06, 0xC3, raw]` | 0=Speed, 1=Range |
| Microphone gain | `[0x06, 0x27, raw]` | 0x01=Low, 0x02=High |
| Auto-off timeout | `[0x06, 0xC1, raw]` | 0–6 |
| Sonar EQ enable | `[0x06, 0x8D, raw]` | 0=Off, 1=On |
| NC level | `[0x06, 0x33, raw, raw, raw]` | 0–10 (3-byte payload) |

Auto-off timeout values: 0=Off, 1=1 min, 2=5 min, 3=10 min, 4=15 min, 5=30 min, 6=60 min.

---

## Equalizer

10 frequency bands (31 Hz – 16 kHz). Raw encoding: `dB = (raw - 20) × 0.5`, range ±10 dB in 0.5 dB steps (raw 0x14 = 0 dB).

Five presets: Flat, Bass Boost, Reference, Smiley, Custom. To set custom bands: switch to preset 4 (`[0x06, 0x2E, 0x04]`), then send band values (`[0x06, 0x33, b0..b9]`), then persist.

Note: command byte `0x33` is shared between NC level (3-byte payload) and EQ band write (10-byte payload). The device distinguishes them by payload length.

---

## ChatMix

Unsolicited packets with prefix `[0x07, 0x45]` are sent when the ChatMix dial is turned. Byte 2 = game volume, byte 3 = chat volume (0–100 each). HaloDaemon creates two PipeWire/PulseAudio virtual sinks and adjusts their volumes in response.

---
