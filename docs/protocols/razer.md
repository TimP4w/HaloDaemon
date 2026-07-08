# Razer USB HID Protocol

Razer's vendor HID protocol, spoken by its USB peripherals (mice, keyboards, and other Chroma devices). Every exchange is a fixed **90-byte report** carried over HID **Feature reports** (`SET_REPORT` / `GET_REPORT` control transfers); a command **class**/**id** pair selects the operation and an XOR **CRC** guards the payload.

The wire frame, checksum, transfers, and command catalog below are **device-independent**. What varies per device — the interface index, the transaction id, the matrix kind and dimensions, which LED zones exist, and which commands are supported — is declared by that device's descriptor, not fixed by the protocol. Adding a Razer device means filling in those parameters and reusing the shared builders below; see [§7](#7-per-device-parameters).

**Credits:** reverse-engineered by the [OpenRazer](https://github.com/openrazer/openrazer) kernel driver (GPL-2.0-or-later — `driver/razercommon.*`, `driver/razerchromacommon.*`) and cross-validated against [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB)'s `RazerController` (GPL-2.0-or-later).

---

## 1. Report layout

The `razer_report` struct (`razercommon.h`), 90 bytes, `static_assert`-checked upstream. Offsets are 0-based; unlisted bytes are zero.

```text
byte 0        status            00 on host→device; on reply see status table
byte 1        transaction_id    per-device / per-command value (§4)
bytes 2–3     remaining_packets BIG-ENDIAN u16; 0x0000 for single-packet commands
byte 4        protocol_type     0x00
byte 5        data_size         number of valid bytes in arguments[]
byte 6        command_class     command group (§5)
byte 7        command_id        command within the class; high bit set = "get" variant
bytes 8–87    arguments[80]     command payload
byte 88       crc               XOR of bytes 2..87 (§3)
byte 89       reserved          0x00
```

A report is built by `get_razer_report(command_class, command_id, data_size)`, which zeroes the struct and sets those three fields plus `status=0`, `transaction_id=0`, `remaining_packets=0`, `protocol_type=0`. The caller then overrides `transaction_id` (§4) and fills `arguments[]`.

Setting the high bit of the command id (`0x8x`/`0xCx`) turns a "set" into the matching "get". The host writes a report, waits a settle delay (§6), then — for reads — issues a `GET_REPORT` to pull the reply back. There is no sequence/ACK handshake beyond the status byte and an optional retry loop.

> Some stacks (e.g. OpenRGB) prepend one leading `report_id` byte (`0x00`), making the on-wire buffer 91 bytes; that byte is the HIDAPI report number, not part of the vendor payload. All offsets here are for the bare **90-byte** structure.

### Status byte (byte 0)

Host→device commands send `0x00`. The device replies with:

| Value | Meaning |
|-------|---------|
| `0x01` | Busy (`RAZER_CMD_BUSY`) |
| `0x02` | Successful (`RAZER_CMD_SUCCESSFUL`) |
| `0x03` | Failure (`RAZER_CMD_FAILURE`) |
| `0x04` | Timeout (`RAZER_CMD_TIMEOUT`) |
| `0x05` | Not supported (`RAZER_CMD_NOT_SUPPORTED`) |

---

## 2. Control transfers

HID class control transfers on the device's vendor interface (`razercommon.c`). The **interface index** goes in `wIndex` and is a per-device parameter (§7).

### SET — host → device (`razer_send_control_msg`)

| Field | Value |
|-------|-------|
| `bmRequestType` | `0x21` (class, interface, host→device) |
| `bRequest` | `0x09` (`HID_REQ_SET_REPORT`) |
| `wValue` | `0x0300` (report type `0x03` = Feature, report id `0x00`) |
| `wIndex` | device interface index (§7) |
| `wLength` | `90` (`sizeof(razer_report)`) |

Followed by a settle delay (§6).

### GET — device → host (`razer_get_usb_response`)

The host first sends the request via the SET above, waits the settle delay, then reads the reply:

| Field | Value |
|-------|-------|
| `bmRequestType` | `0xA1` (class, interface, device→host) |
| `bRequest` | `0x01` (`HID_REQ_GET_REPORT`) |
| `wValue` | `0x0300` |
| `wIndex` | device interface index (§7) |
| `wLength` | `90` |

OpenRGB uses HIDAPI `hid_send_feature_report` / `hid_get_feature_report`, the exact equivalents.

---

## 3. Checksum (CRC)

`razer_calculate_crc` (`razercommon.c`) XOR-folds the middle of the report:

```
crc = 0
for i in 2..=87:          # remaining_packets high byte … last arguments byte
    crc ^= report[i]
```

Stored in byte 88. **`status` (0), `transaction_id` (1), and `reserved` (89) are excluded.**

---

## 4. Transaction id

Byte 1 is a **transaction id** that the device matches against; a wrong value makes the device NAK the report while still accepting others. It is **not fixed by the protocol** — OpenRazer's builders always emit `0`, and the device layer stamps a value chosen by **(device, command group)**.

The byte is a bitfield union (`transaction_id_union`):

```text
bits 0–4  transaction id (5 bits)
bits 5–7  device id       (3 bits)
```

This explains the observed family values: `0x1F` = device 1, id 31; `0x3F` = device 1, id 63; `0xFF` is the power-on/reset default (all bits set). A given device may use one value uniformly or split it by command group (e.g. one id for control/config and another for the LED-frame write).

Because the value is device-specific, each device descriptor supplies its transaction id(s) — see [§7](#7-per-device-parameters). Treat a single fixed value as the conservative default and override per command group only where a device requires it.

---

## 5. Command catalog

Notation: `(class, id, data_size)` then the non-zero `arguments[]` bytes. `r,g,b` are 8-bit colour components. Store/LED constants (`razercommon.h`): `NOSTORE=0x00`, `VARSTORE=0x01`; LED-zone ids (common subset — devices declare which they support):

| ID | Constant |
|----|----------|
| `0x00` | `ZERO_LED` |
| `0x01` | `SCROLL_WHEEL_LED` |
| `0x03` | `BATTERY_LED` |
| `0x04` | `LOGO_LED` |
| `0x05` | `BACKLIGHT_LED` |
| `0x07` | `MACRO_LED` |
| `0x08` | `GAME_LED` |
| `0x0C`–`0x0E` | profile LEDs (red, green, blue) |
| `0x10`–`0x11` | side LEDs (right, left) |
| `0x1A`–`0x1F` | ARGB channels 1–6 |
| `0x20`–`0x22` | charging / fast-charging / fully-charged LED |

A device implements a subset of the classes below. RGB devices are either **standard matrix** (class `0x03`) or **extended matrix** (class `0x0F`) — the two are mutually exclusive per device and a device declares which it speaks (§7).

### Class `0x00` — info & misc

| Operation | `(class, id, data_size)` | Arguments / notes |
|-----------|--------------------------|-------------------|
| Firmware version | `(0x00, 0x81, 0x02)` | reply `args[0].args[1]` = major.minor |
| Serial number | `(0x00, 0x82, 0x16)` | 22-byte ASCII in reply args |
| Set device mode | `(0x00, 0x04, 0x02)` | `args[0]=mode` (`0x00` normal / `0x03` driver; `0x02` blocked), `args[1]=0x00` |
| Get device mode | `(0x00, 0x84, 0x02)` | |
| Set polling rate | `(0x00, 0x05, 0x01)` | `args[0]` = code: `0x01`=1000 Hz, `0x02`=500 Hz, `0x08`=125 Hz |
| Get polling rate | `(0x00, 0x85, 0x01)` | |

A newer HyperPolling variant — set `(0x00, 0x40, 0x02)` / get `(0x00, 0xC0, 0x01)`, bitcodes `0x01`=8000 Hz … `0x40`=125 Hz — exists on high-polling-rate devices.

### Class `0x02` — input / scroll wheel

For devices with a configurable scroll wheel. Each is `[0]=VARSTORE, [1]=value(0/1)`:

| Operation | `(class, id, data_size)` |
|-----------|--------------------------|
| Scroll mode (tactile/free-spin) | `(0x02, 0x14, 0x02)` |
| Scroll acceleration | `(0x02, 0x16, 0x02)` |
| Scroll smart-reel (auto free-spin) | `(0x02, 0x17, 0x02)` |

(Horizontal-tilt auto-repeat controls exist in the same class for tilt-wheel devices.)

### Class `0x04` — DPI / misc

| Operation | `(class, id, data_size)` | Arguments |
|-----------|--------------------------|-----------|
| Set DPI (X/Y) | `(0x04, 0x05, 0x07)` | `[0]=VARSTORE, [1..2]=dpi_x BE u16, [3..4]=dpi_y BE u16` |
| Get DPI | `(0x04, 0x85, 0x07)` | `[0]=varstore` |
| Set DPI stages | `(0x04, 0x06, 0x26)` | `[0]=varstore, [1]=active_stage, [2]=count`, then 7 bytes/stage `{stage#, Xhi, Xlo, Yhi, Ylo, 0, 0}` |
| Get DPI stages | `(0x04, 0x86, 0x26)` | |
| Set DPI (byte-scaled, legacy) | `(0x04, 0x01, 0x03)` | older devices |
| Get DPI (byte-scaled, legacy) | `(0x04, 0x81, 0x03)` | |

DPI is transmitted **big-endian**, X then Y, each preceded by a storage byte. Devices clamp to their own hardware range.

### Class `0x0F` — extended matrix (LED)

The modern RGB path. Effects and brightness carry a **storage** byte and an **`led_id`** selecting the target zone, so a device with multiple LED zones (e.g. a backlight plus a logo and scroll zone) drives each zone with the same builders and a different `led_id`.

Effects — `(0x0F, 0x02, ·)`, base layout `[0]=storage, [1]=led_id, [2]=effect_id`:

| Effect | data_size | arguments |
|--------|-----------|-----------|
| None | `0x06` | `[2]=0x00` |
| Static | `0x09` | `[2]=0x01, [5]=0x01, [6..8]=r,g,b` |
| Breathing (random) | `0x06` | `[2]=0x02` |
| Breathing (single) | `0x09` | `[2]=0x02, [3]=0x01, [5]=0x01, [6..8]=r,g,b` |
| Breathing (dual) | `0x0C` | `[2]=0x02, [3]=0x02, [5]=0x02, [6..8]=rgb1, [9..11]=rgb2` |
| Spectrum | `0x06` | `[2]=0x03` |
| Wave | `0x06` | `[2]=0x04, [3]=dir(0..2), [4]=0x28` |
| Reactive | `0x09` | `[2]=0x05, [4]=speed(1..4), [5]=0x01, [6..8]=r,g,b` |
| Starlight (random) | `0x06` | `[2]=0x07, [4]=speed(1..3)` |
| Starlight (single) | `0x09` | `[2]=0x07, [4]=speed, [5]=0x01, [6..8]=rgb1` |
| Starlight (dual) | `0x0C` | `[2]=0x07, [4]=speed, [5]=0x02, [6..8]=rgb1, [9..11]=rgb2` |
| Wheel | `0x06` | `[2]=0x0A, [3]=dir(1..2), [4]=0x28` |

Brightness — set `(0x0F, 0x04, 0x03)` / get `(0x0F, 0x84, 0x03)`: `[0]=storage, [1]=led_id, [2]=brightness (0..255)`.

> **Mouse devices** use a separate extended-matrix path: class `0x03` command `0x0D` with the same effect ids remapped (Static=`0x06`, Spectrum=`0x04`, Reactive=`0x02`) and a shorter payload layout omitting the speed byte. Check the device descriptor for which path to use.

Direct per-LED control is a two-step dance:

1. **Enable custom-frame mode** — `(0x0F, 0x02, 0x0C)` with `[0]=0x00, [1]=0x00, [2]=0x08` (effect id `0x08`).
2. **Write a row of LEDs** — `(0x0F, 0x03, N)` where `N` is the payload size, typically `0x47` (max) or `row_length + 5` (exact):
   ```text
   args[2]  = row_index
   args[3]  = start_col
   args[4]  = stop_col
   args[5…] = RGB bytes, 3 per column: (stop_col + 1 − start_col) × 3 bytes
   ```
   Multi-row matrices are written one row per report, with a short gap between rows (§6). The row/column dimensions are a per-device parameter (§7).

### Class `0x03` — standard matrix (legacy LED)

The older RGB path, used by devices that predate the extended matrix. Mutually exclusive with class `0x0F`. Also hosts the **mouse extended matrix** path (`0x03, 0x0D`, see class `0x0F` note above).

**Per-LED controls** (class `0x03`, non-matrix — used for discrete single-colour zones):

| Operation | Command | Arguments |
|-----------|---------|-----------|
| Set LED state | `(0x03, 0x00, 0x03)` | `[0]=storage, [1]=led_id, [2]=on/off(0/1)` |
| Get LED state | `(0x03, 0x80, 0x03)` | `[0]=storage, [1]=led_id` |
| Set LED RGB | `(0x03, 0x01, 0x05)` | `[0]=storage, [1]=led_id, [2..4]=r,g,b` |
| Get LED RGB | `(0x03, 0x81, 0x05)` | `[0]=storage, [1]=led_id` |
| Set LED effect | `(0x03, 0x02, 0x03)` | `[0]=storage, [1]=led_id, [2]=effect(0..5)` |
| Get LED effect | `(0x03, 0x82, 0x03)` | `[0]=storage, [1]=led_id` |
| Set LED blinking | `(0x03, 0x04, 0x04)` | `[0]=storage, [1]=led_id, [2..3]=0x05,0x05` |

**Matrix effects** — `(0x03, 0x0A, ·)`, `[0]=effect_id`:

| Effect | data_size | arguments |
|--------|-----------|-----------|
| None | `0x01` | `[0]=0x00` |
| Wave | `0x02` | `[0]=0x01, [1]=dir(1..2)` |
| Reactive | `0x05` | `[0]=0x02, [1]=speed(1..4), [2..4]=r,g,b` |
| Breathing (random) | `0x08` | `[0]=0x03, [1]=0x03` |
| Breathing (single) | `0x08` | `[0]=0x03, [1]=0x01, [2..4]=r,g,b` |
| Breathing (dual) | `0x08` | `[0]=0x03, [1]=0x02, [2..4]=rgb1, [5..7]=rgb2` |
| Spectrum | `0x01` | `[0]=0x04` |
| Custom frame (enable) | `0x02` | `[0]=0x05, [1]=frame_id` |
| Static | `0x04` | `[0]=0x06, [1..3]=r,g,b` |
| Starlight (random) | `0x04` | `[0]=0x19, [1]=0x03, [2]=speed(1..3)` |
| Starlight (single) | `0x09` | `[0]=0x19, [1]=0x01, [2]=speed, [3..5]=rgb1` |
| Starlight (dual) | `0x09` | `[0]=0x19, [1]=0x02, [2]=speed, [3..5]=rgb1, [6..8]=rgb2` |

**Custom frame write** — `(0x03, 0x0B, N)` where `N` is the payload size (max `0x46`, or `row_length + 4` exact): `[0]=0xFF, [1]=row, [2]=start, [3]=stop, [4…]=rgb`.

**Brightness** — set `(0x03, 0x03, 0x03)` / get `(0x03, 0x83, 0x03)`: `[0]=storage, [1]=led_id, [2]=brightness(0..255)`.

---

## 6. Timing

- **Settle delay** — after every `SET_REPORT` the host waits a fixed delay before the follow-up `GET_REPORT` or the next write. The exact value is a device-class constant (sub-millisecond, e.g. `RAZER_MOUSE_WAIT_US = 600` µs for mice).
- **Retry loop** — `razer_send_payload` retries up to **5 times**; on a busy/failed status it sleeps **10 ms** before retrying.
- **Per-row gap** — custom-frame rows are written with a short (~1 ms) gap between them; larger gaps (2–5 ms) around init and mode changes.

---

## 7. Per-device parameters

Everything the protocol leaves open. A Razer device is fully described by:

| Parameter | Meaning |
|-----------|---------|
| VID:PID | USB identity (VID is always `0x1532`). |
| Interface index | `wIndex` for the control transfers (§2). |
| Matrix kind | **standard** (class `0x03`) or **extended** (class `0x0F`) — which RGB path the device speaks. |
| Matrix dimensions | rows × columns of the custom-frame LED grid. |
| LED zones | which `led_id`s exist (main backlight, logo, scroll wheel, …). |
| Transaction id(s) | byte 1 value, possibly split by command group (§4). |
| Supported commands | subset of §5 the device implements (DPI, DPI stages, scroll controls, polling rate, battery, …). |
| Power | wired vs wireless; only wireless devices expose battery/charge commands. |

Adding a device: capture these parameters in its descriptor, pick the matching §5 builders, and default the transaction id to a single value unless the device NAKs a specific command group.

---

## Notes

- **Two RGB paths, pick one per device.** A device speaks either the standard (`0x03`) or extended (`0x0F`) matrix, never both; the extended path additionally carries a per-zone `led_id`.
- **Transaction id is device data, not a constant** — default it, override per command group only when required (§4).
- **DPI is big-endian**, X then Y, each preceded by a storage byte.
- **No ACK/sequence** beyond the status byte and the retry loop; `remaining_packets` is `0` for the single-packet commands here.
- **Battery/charge commands** exist only on wireless devices and are out of scope for wired-only ones.
