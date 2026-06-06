# DDC/CI Protocol

DDC/CI (Display Data Channel Command Interface) is a VESA standard for controlling monitor settings — brightness, contrast, input selection — over a bidirectional command channel. Standard VCP codes are defined by the MCCS specification; monitors may also implement vendor-specific codes.

**Credits:** VESA DDC/CI and MCCS standards — no reverse engineering required.

**Source:** `src/daemon/src/drivers/vendors/philips/protocols/philips_evnia.rs` (frame builders, DDC/CI framing, VCP encoding); `src/daemon/src/drivers/vendors/philips/devices/evnia_49.rs` (device driver)

---

## Usage in HaloDaemon

The Philips Evnia 49 (`49M2C8900`) exposes DDC/CI over USB via its internal USB hub chip (VID `0x2109`, PID `0x8884`). HaloDaemon sends DDC/CI commands as USB vendor control transfers via the [USB control transport](../transports/usb-control.md).

Control transfer parameters:

| Field | Value |
|-------|-------|
| `bmRequestType` | `0x40` (vendor / host-to-device / device recipient) |
| `bRequest` | `0xB2` |
| `wValue` | `0` |
| `wIndex` | `0` |

---

## Message format

### Standard write (8 bytes) — used for brightness and most VCP codes

```
Byte 0: 0x6E  — destination (monitor)
Byte 1: 0x51  — source (host)
Byte 2: 0x84  — 0x80 | length
Byte 3: 0x03  — Set VCP Feature opcode
Byte 4: VCP code high byte
Byte 5: VCP code low byte
Byte 6: Value
Byte 7: XOR checksum of bytes 0–6
```

### Extended write (10 bytes) — used for Philips-specific VCP codes with sub-commands

```
Byte 0: 0x6E
Byte 1: 0x51
Byte 2: 0x86  — 0x80 | length(6)
Byte 3: 0x03  — Set VCP Feature opcode
Byte 4: VCP code high byte
Byte 5: VCP code low byte
Byte 6: Sub-command high byte
Byte 7: Sub-command low byte
Byte 8: Value
Byte 9: XOR checksum of bytes 0–8
```

---

## VCP codes used

The "VCP Code" column shows `high_byte / low_byte` — the two bytes placed at positions 4 and 5 in the message frame. Standard single-byte codes use `0x00` as the high byte.

| VCP Code (hi/lo) | Feature | Range | Notes |
|------------------|---------|-------|-------|
| `0x00` / `0x10` | Brightness | 0–100 | Standard MCCS code |
| `0xE2` / `0xA0` | Crosshair | 0–2 | Philips-specific; 0=Off, 1=On, 2=Smart; uses extended 10-byte format with sub-command `0x04, 0x00` |

---
