# HID++ 1.0 — register protocol

The older, **register-based** half of [HID++](hidpp.md). A flat address space of 9-bit *registers* read/written with sub-IDs `0x80`–`0x83`. Used by every Unifying/Lightspeed receiver (pairing, device list, connection notifications) and the G560 speaker's vendor commands. Modern feature-based devices use [HID++ 2.0](hidpp2.md) instead.

Code: `protocols/hidpp/v1/` — `Hidpp10` (the typed handle bound to one device number), `v1/receiver.rs` (receiver operations), and the register constants. See [hidpp.md](hidpp.md) for the shared frame format.

**Credits:** Solaar (GPL-2.0-or-later) — `hidpp10.py`, `receiver.py` by Daniel Pavel and contributors.

---

## Packet shape

A register access is a **short (7-byte) report** (`build_packet`, see [hidpp.md](hidpp.md) for the report sizes):

```
byte 0    0x10                report ID (HIDPP_SHORT)
byte 1    dd                  device number (0xFF = receiver itself; 1–6 = paired slot)
byte 2    sub-ID              0x80/0x81/0x82/0x83 (write/read × register bit 9)
byte 3    addr                register address low byte (R & 0xFF)
byte 4-6  p0 p1 p2            params, zero-padded to 7 bytes
```

The G560 vendor commands (§4) instead use a **long (20-byte) report** with a vendor sub-ID and function in bytes 2–3. (A short register request may have its *reply* delivered as a long report on the long collection — see [hidpp.md](hidpp.md).)

---

## 1. Register access

Sub-ID encodes direction and register bit 9 (`HidppMessenger::hidpp10_read` / `hidpp10_write`, wrapped by `Hidpp10::read`/`write`). For register `R`: `sub_id = base | ((R >> 8) & 0x02)`, address = `R & 0xFF`, params padded to 3 bytes, **short report**.

| Op | Sub-ID base | Bytes sent | Notes |
|----|-------------|-----------|-------|
| Write, reg bit9=0 | `0x80` | `10 dd 80 <addr> p0 p1 p2` | fire-and-forget |
| Read,  reg bit9=0 | `0x81` | `10 dd 81 <addr> p0 p1 p2` | awaits reply |
| Write, reg bit9=1 | `0x82` | `10 dd 82 <addr> p0 p1 p2` | |
| Read,  reg bit9=1 | `0x83` | `10 dd 83 <addr> p0 p1 p2` | |

Registers (`v1/mod.rs`):

| Register | Const | Use |
|----------|-------|-----|
| `0x0002` | `REG_DEVICE_COUNT` | write `[0x02]` → receiver re-broadcasts connect status; read → paired count in reply **byte 1** |
| `0x02B5` | `REG_RECEIVER_INFO` | pairing records; sub-param `INFO_PAIRING 0x20` / `INFO_EXTENDED_PAIRING 0x30` / `INFO_DEVICE_NAME 0x40`, each `+ devnum − 1` |
| `0x00B2` | `REG_RECEIVER_PAIRING` | open/close the pairing lock and unpair a slot (Unifying-style); see §2 |

---

## 2. Receiver operations

All addressed to the receiver itself (`devnum = 0xFF`); the typed wrappers live in `v1/receiver.rs` on `Hidpp10`.

### `notify_devices` / `device_count`

`notify_devices` writes `REG_DEVICE_COUNT [0x02]` → the receiver re-broadcasts every paired slot's connection status as unsolicited notifications (see §3). `device_count` reads `REG_DEVICE_COUNT` and returns reply **byte 1** (not byte 0).

### `paired_info(slot)` — pairing record for a slot (1-based)

1. Read `REG_RECEIVER_INFO` with param `[INFO_PAIRING + slot − 1]` → `10 FF 83 B5 (0x20+slot-1) 00 00`. Reply must be ≥ 8 bytes.
2. **WPID** is bytes `[3:5]` big-endian (Solaar `extract_wpid` reverses `pair[3:5]`). A WPID of `0x0000` or `0xFFFF` means the slot is empty → `None`.
3. Read `REG_RECEIVER_INFO` with param `[INFO_EXTENDED_PAIRING + slot − 1]` → the **serial**: 4 bytes at `ext[1:5]`, formatted as 8 hex chars. All-zero or all-`0xFF` is the unset sentinel (`parse_extended_serial` → `None`).

Returns `PairedDevice { devnum, wpid, serial }`.

### Pairing — `open_pairing_lock` / `close_pairing_lock` / `unpair`

Writes to `REG_RECEIVER_PAIRING` (`0x00B2`), fire-and-forget. This Lightspeed family uses the Unifying-style register (`may_unpair = true`, `re_pairs = false`); no Bolt discovery is involved.

| Operation | Bytes sent | Params |
|-----------|-----------|--------|
| Open pairing lock | `10 FF 80 B2 01 00 <timeout>` | `[0x01, 0x00, timeout_secs]` |
| Close pairing lock | `10 FF 80 B2 02 00 00` | `[0x02, 0x00, 0x00]` |
| Unpair slot | `10 FF 80 B2 03 <slot>` | `[0x03, slot]` (slot 1-based) |

While the lock is open the receiver emits `0x4A` lock-status notifications (§3) and, when a device pairs, its `0x41` connection notification under the new device number.

---

## 3. Notifications

The receiver emits unsolicited HID++ 1.0 notifications on the broadcast channel, keyed by device number `1..=6`. (This page describes the wire packet only, not the daemon's device-list reaction.)

### Device connection — `0x41`

Sent for **both** connect and disconnect; the link state is in the payload. `decode_link_established(data)`: bit `0x40` of `data[0]` is "link **not** established" — set on power-off, clear on power-on, so `link_established = !(data[0] & 0x40)`. An empty payload reads as disconnected.

Live captures (trailing bytes vary per device, irrelevant):

| Payload | Meaning |
|---------|---------|
| `71 b0 40` / `72 99 40` | device 1 / 2 powered **off** (bit 0x40 set) |
| `b1 b0 40` / `b2 99 40` | device 1 / 2 powered **on** (bit 0x40 clear) |

### Pairing-lock status — `0x4A`

Emitted while a pairing lock is open/closing (`decode_pairing_lock(address, data)`). The "lock **open**" flag and the error code live in **two different bytes**, so an error code whose low bit is set (e.g. `0x03`) is never mistaken for the open flag:

- **`address` byte** (packet byte 3) — bit `0x01` set ⇒ lock open (listening for a device).
- **`data[0]`** (packet byte 4) — once the lock is closed, a **nonzero** value is a `PairingError` code; `0x00` means a device paired cleanly. Ignored while the lock is open.

| `data[0]` (lock closed) | `PairingError` |
|-------------------------|----------------|
| `0x00` | none — device paired |
| `0x01` | device-timeout |
| `0x02` | not-supported |
| `0x03` | too-many-devices |
| `0x06` | sequence-timeout |

---

## 4. G560 Gaming Speaker — vendor commands

The G560 RGB Gaming Speaker (PID `0x0A78`) does **not** use HID++ 2.0 feature enumeration. It is driven by two fixed **HID++ 1.0 vendor long reports** (`0x11`), addressed to device number `0xFF`, sent fire-and-forget (no reply awaited) via `hidpp_long_fire`. Here byte 2 is a vendor sub-ID (`0x04` lighting, `0x09` audio) and byte 3 a vendor function — **not** the register-access sub-IDs (`0x80`–`0x83`) of §1.

**Discovery.** Matched on its vendor RGB HID collection: VID `0x046D`, PID `0x0A78`, interface 2, usage page `0xFF43`, usage `0x0202` (Windows). On Linux the single hidraw node reports usage page/usage `0`, and discovery falls back to it.

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| Set zone colour (`0x04 0x3A`) | `11 FF 04 3A <zone> 01 <R> <G> <B> 02 ··` | zone (see table), RGB | mode byte `0x01` = fixed colour; trailing `0x02` constant |
| Set subwoofer volume (`0x09 0x1C`) | `11 FF 09 1C <vol> ··` | `vol` 0–100 | clamped to ≤ 100 |

**Set-zone-colour flow** (`send_zone_color`) — paint zone *Left Primary* (`0x02`) magenta `(255,0,128)`:

1. Build the payload `[zone, 0x01, R, G, B, 0x02]`.
2. Send the long vendor report `11 FF 04 3A 02 01 FF 00 80 02 00 …00` (zero-padded to 20 bytes); no reply awaited.
3. A whole-device static colour is applied by sending this once per zone, iterating all four zone bytes.

**Zone bytes** (`ZONES`):

| Zone byte | Zone |
|-----------|------|
| `0x00` | Left Secondary |
| `0x01` | Right Secondary |
| `0x02` | Left Primary |
| `0x03` | Right Primary |

Only the fixed-colour mode (`0x01`) is used; the driver exposes no native effects, so breathing/cycle mode-byte values are not driven by HaloDaemon. Subwoofer volume is the device's only Range capability.
