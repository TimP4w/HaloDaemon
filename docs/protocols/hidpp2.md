# HID++ 2.0 — feature protocol

The modern, **feature-enumerated** half of [HID++](hidpp.md). A device exposes a table of *features* identified by 16-bit codes; each present feature is assigned a runtime *index* discovered through the ROOT feature (`0x0000`) before any call. Used by current mice, keyboards and headsets.

Code: `protocols/hidpp/v2/` — `Hidpp20` (the typed handle bound to a device number + feature table) plus one module per capability domain: `battery`, `audio`, `keys`, `rgb/` and `settings/` (report-rate / DPI / onboard). The device layer calls typed methods on `Hidpp20`; it never sees a function byte or a reply slice. See [hidpp.md](hidpp.md) for the shared frame format.

**Credits:** Solaar (GPL-2.0-or-later) — `hidpp20.py`, `settings_templates.py`, `settings_validator.py` by Daniel Pavel and contributors.

---

## Packet shape

A feature call is a **long (20-byte) report** (`build_packet`, see [hidpp.md](hidpp.md)); only a ROOT call rides a short report (§1):

```
byte 0     0x11               report ID (HIDPP_LONG; 0x10 for a short ROOT call)
byte 1     dd                 device number (1–6 wireless via receiver; 0xFF wired direct)
byte 2     fi                 feature index (resolved via ROOT; 0 = ROOT itself)
byte 3     func | swid        function high-nibble | software-id (always 1) → e.g. 0x31
byte 4…    params, zero-padded to 20 bytes
```

This is the shape behind every `11 dd fi <func> ··` template in the tables below.

---

## 1. Function + software-ID byte (byte 3)

Every 2.0 function byte is `function | software_id`, where the function occupies the **high nibble** (`0x00`, `0x10`, `0x20`, … `0xF0`) and the software ID the low nibble. HaloDaemon uses a **fixed software ID of 1**, so function `0x30` is sent as byte 3 = `0x31`, function `0x00` as `0x01`. The device echoes the same byte, letting the host correlate replies. (`feature_request` takes the high-nibble byte and stamps swid 1.)

In the tables below, outgoing byte templates are the full report as `build_packet` emits it: `dd` = device number, `fi` = feature index (resolved via ROOT), `··` = zero padding. So a function shown as `0x30` is sent with byte 3 = `0x31`.

**Short vs long selection.** A request is **short** (`0x10`, 7 bytes) only when *both* the feature index is `0` and the payload is ≤ 3 bytes — in practice only ROOT calls. Every other feature has a non-zero index and always uses **long** (`0x11`, 20 bytes). Force-long devices (LIGHTSPEED headsets) route ROOT onto long too — see [hidpp.md](hidpp.md).

---

## 2. Feature discovery

> **Required sequence (all 2.0 calls):** resolve the feature's index via ROOT *before* any call to it.

### ROOT (`0x0000`) — fixed index 0

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getFeature (`0x00`) | `10 dd 00 01 <code_hi> <code_lo> 00` | feature code (BE u16) | reply byte 0 = runtime index, `0` = absent. Sent short (fi=0, ≤3 params). |

Resolving ADJUSTABLE_DPI (`0x2201`) on device `0x01`: send `10 01 00 01 22 01 00`; the reply's byte 0 is the index (e.g. `04`), used as byte 2 (`fi`) in every later call to that feature.

### FEATURE_SET (`0x0001`) — full enumeration

`Hidpp20::enumerate` resolves FEATURE_SET via ROOT, reads its count, then loops `getFeatureId` for each index, building the whole `code → index` map at discovery.

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getCount (`0x00`) | `11 dd fi 01 ··` | none | reply byte 0 = feature count |
| getFeatureId (`0x10`) | `11 dd fi 11 <index> ··` | table index | reply bytes 0..2 = feature code (BE) |

### Feature codes (`v2/mod.rs`)

ROOT `0x0000`, FEATURE_SET `0x0001`, FIRMWARE_VERSION `0x0003`, DEVICE_NAME `0x0005`, DEVICE_FRIENDLY_NAME `0x0007`, BATTERY_VOLTAGE `0x1001`, UNIFIED_BATTERY `0x1004`, WIRELESS_DEVICE_STATUS `0x1D4B`, ADC_MEASUREMENT `0x1F20`, HIRES_WHEEL `0x2121`, ADJUSTABLE_DPI `0x2201`, K375S_FN_INVERSION `0x40A3`, KEYBOARD_LAYOUT_2 `0x4540`, REPROG_CONTROLS_V4 `0x1b04`, BRIGHTNESS_CONTROL `0x8040`, REPORT_RATE `0x8060`, EXT_REPORT_RATE `0x8061`, RGB_EFFECTS `0x8071`, PER_KEY_LIGHTING_V2 `0x8081`, GKEY `0x8010`, ONBOARD_PROFILES `0x8100`, MOUSE_BUTTON_SPY `0x8110`, SIDETONE `0x8300`, EQUALIZER `0x8310`. Declared but unused: `0x2202`, `0x1b10`, `0x1bc0`, `0x1b05`, `0x1D4B` (WIRELESS_DEVICE_STATUS is declared for enumeration logging only).

### DEVICE_NAME (`0x0005`) / DEVICE_FRIENDLY_NAME (`0x0007`)

`Hidpp20::device_name` reads func `0x00` (name length) then func `0x10` + offset for up to 16 chars per chunk until the length is reached. `device_friendly_name` (`0x0007`) uses the same length-then-chunk shape for newer devices (MX Keys, Craft, …) but the chunk reply **echoes the offset in byte 0**, so it copies from `reply[1..]`. `init_name` prefers `0x0007` when present and falls back to `0x0005`.

---

## 3. Battery — `v2/battery.rs`

`Hidpp20::battery_source` resolves the source in priority order (UNIFIED preferred), then `read_battery` decodes it.

### UNIFIED_BATTERY (`0x1004`) — percentage

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getStatus (`0x10`) | `11 dd fi 11 ··` | none | reply `[level, ?, charging, …]`: byte 0 = percent, byte 2 ≠ 0 → charging |

### ADC_MEASUREMENT (`0x1F20`) / BATTERY_VOLTAGE (`0x1001`) — cell voltage

LIGHTSPEED headsets report a **cell voltage**, not a percentage (the PRO X `0x0ABA` advertises only ADC_MEASUREMENT).

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getVoltage (`0x00`) | `11 dd fi 01 00 ··` | `[0x00]` | reply: BE-u16 mV in bytes 0–1; byte 2 status (`0x01` discharging, `0x03` charging) |

`voltage_to_percent` maps mV → 0–100% by piecewise-linear interpolation over a discharge curve. A `0` mV reading means the headset is **asleep** (dongle on, earcups off) — the same condition makes SIDETONE/EQUALIZER reads return `logitech_internal` (`0x05`); all are treated as "not yet available" and the 30 s battery poll retries. The two voltage features share this exact reply layout.

---

## 4. Settings — `v2/settings/`

### REPORT_RATE (`0x8060`) — `settings/report_rate.rs`

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getReportRateList (`0x00`) | `11 dd fi 01 ··` | none | reply byte 0 = bitmask; bit *i* ⇒ rate `(i+1)` ms supported |
| getReportRate (`0x10`) | `11 dd fi 11 ··` | none | reply byte 0 = current rate (ms) |
| setReportRate (`0x20`) | `11 dd fi 21 <ms> ··` | `[ms]` | requires host mode (see ONBOARD_PROFILES setMode) |

### EXT_REPORT_RATE (`0x8061`)

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getReportRateList (`0x10`) | `11 dd fi 11 ··` | none | reply BE-u16 flags; bit *i* ⇒ rate-table index *i* supported |
| getReportRate (`0x20`) | `11 dd fi 21 ··` | none | reply byte 0 = current rate index |
| setReportRate (`0x30`) | `11 dd fi 31 <idx> ··` | `[rate_idx]` | requires host mode |

**Report-rate bitmaps.** REPORT_RATE units are **milliseconds** (bit *i* ⇒ `(i+1)` ms). EXT_REPORT_RATE units are **table indices**: the 7-entry `EXT_REPORT_RATES` table maps each index to a `(label, ms)` (8/4/2/1 ms, then 500/250/125 µs which all store ms = 0). `read_report_rates` normalises both into a `Vec<ReportRateOption { wire_index, ms, label }>` so the device matches the current selection by `wire_index`, never re-deriving sub-ms rates from ms.

**Host-mode dance.** A rate change is only honoured in host mode, so `ChoiceCapability::set_choice` brackets it: read mode → if onboard, `set_onboard_mode(host=true)` → `set_report_rate(wire_index, ext)` → `set_onboard_mode(host=false)` → re-enable SW RGB control (the firmware reclaims the LEDs on the onboard transition). Already-host devices skip the bracketing.

### ADJUSTABLE_DPI (`0x2201`) — `settings/dpi.rs`

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getSensorDpiList (`0x10`) | `11 dd fi 11 00 00 <chunk> ··` | `[sensor=0, dir=0, chunk_idx]` | reply byte 0 = sensor echo, rest = DPI-list bytes; iterate chunks (max 16) until `0x0000` |
| getSensorDpi (`0x20`) | `11 dd fi 21 ··` | none | reply carries current DPI (BE), with/without sensor echo |
| setSensorDpi (`0x30`) | `11 dd fi 31 00 <dpi_hi> <dpi_lo> ··` | `[sensor=0, dpi_hi, dpi_lo]` (`encode_set_dpi`) | 3-byte form; a 5-byte variant is rejected by G502 X Plus (`INVALID_ARGUMENT`) |

**DPI byte order** depends on where the value lives: on the wire (`encode_set_dpi`) it is **big-endian** (`1600 = 0x0640 → 06 40`); in a profile flash sector it is **little-endian** (`40 06`). `read_current_dpi` returns `None` for a decoded `0` (never genuinely reported).

**DPI-list range markers** (`parse_dpi_list`). `getSensorDpiList` returns BE-u16 entries terminated by `0x0000`. A u16 whose top 3 bits are all set (`value >> 13 == 0b111`) is a **range marker**: low 13 bits = step, the *next* u16 = inclusive end; the range expands from the last explicit value adding `step` while `≤ end`. E.g. `400`, marker `0xE190` (step 400), end `0x0640` → 800, 1200, 1600.

### ONBOARD_PROFILES (`0x8100`) — `settings/onboard.rs`

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getInfo (`0x00`) | `11 dd fi 01 ··` | none | `info[4]` = ROM profile count, `info[7:9]` = sector size (BE) — `parse_onboard_caps` |
| setMode (`0x10`) | `11 dd fi 11 <mode> ··` | `[0x02]`=host, `[0x01]`=onboard | |
| getMode (`0x20`) | `11 dd fi 21 ··` | none | reply byte 0: `0x02`=host, `0x01`=onboard |
| setCurrentProfile (`0x30`) | `11 dd fi 31 00 <slot> 00 ··` | `[0x00, slot, 0x00]` | |
| getCurrentProfile (`0x40`) | `11 dd fi 41 ··` | none | reply bytes 0..2 = active sector (BE); `0xFFFF`/`0x0000` = none (host) → `None` |
| memoryRead (`0x50`) | `11 dd fi 51 <sec_hi sec_lo off_hi off_lo> ··` | sector + offset (BE u16) | reply = 16-byte chunk |
| memoryAddrWrite / erase (`0x60`) | `11 dd fi 61 <sec_hi sec_lo> 00 00 <sz_hi sz_lo> ··` | `[sector, 0,0, size]` | addresses the writable RAM sector |
| memoryWrite (`0x70`) | `11 dd fi 71 <16 data bytes>` | 16-byte chunk | repeated per slice |
| memoryWriteEnd / commit (`0x80`) | `11 dd fi 81 ··` | none | commits the staged sector |

**Mode byte:** `0x01` = onboard (firmware drives DPI/LEDs/buttons), `0x02` = host (host software drives them). Host mode is required for report-rate changes and per-control divert.

**Sector read/write** (`Hidpp20::read_profile_sector` / `write_profile_sector`). A read loops memoryRead at 16-byte offsets with a final tail read clamped to the sector size. A write must target a **writable RAM sector** (`0x000N`), never a read-only `0x01xx` ROM address: erase (0x60) → write 16-byte chunks (0x70) → commit (0x80). The sector's trailing 2 bytes must be a valid CRC16 (see [hidpp.md](hidpp.md)) or the device silently discards the write.

**Profile-sector layout** (typically 255 bytes on the G502 X Plus):

| Byte(s) | Field | Meaning |
|---------|-------|---------|
| 0 | sector id | low byte = RAM slot index |
| 1 | resolution default index | active DPI step; clamped to `step_count − 1` on write |
| 2 | resolution shift index | DPI-shift step; reset to 0 when out of range |
| 3..13 | DPI steps | 5 × u16 **little-endian** (`0x0000`/`0xFFFF` = unused) |
| 13..(size−2) | button / G-button bindings | preserved verbatim on a DPI patch |
| (size−2)..size | CRC16 | big-endian, over `bytes[0 .. size−2]` |

### HIRES_WHEEL (`0x2121`) — `v2/mod.rs`

Scroll-wheel resolution/direction/diversion + ratchet switch. `read_hires_wheel` reads three functions and decodes them via `parse_hires_wheel` into a `HiresWheel` struct; writes go through `set_hires_wheel_mode`, a read-modify-write of the mode byte.

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getCapability (`0x00`) | `11 dd fi 01 ··` | none | reply byte 0 = ratchet multiplier; byte 1 flags: `0x08` = has-invert, `0x04` = has-ratchet |
| getMode (`0x10`) | `11 dd fi 11 ··` | none | reply byte 0 mode bits: `0x04` = invert, `0x02` = hi-res, `0x01` = HID++ target (diversion) |
| setMode (`0x20`) | `11 dd fi 21 <mode> 00 ··` | `[mode, 0]` | RMW: read byte 0, clear `mask`, OR in `value & mask`, write back |
| getRatchet (`0x30`) | `11 dd fi 31 ··` | none | reply byte 0 bit `0x01` = ratchet engaged |

Feature presence over-declares: a keyboard may advertise `0x2121` with no physical wheel. `read_hires_wheel` requires all three reads to succeed with ≥2 bytes, so a device that errors on getRatchet decodes to `None` and exposes no wheel controls.

### K375S_FN_INVERSION (`0x40A3`) — `v2/mod.rs`

F-row inversion (media keys vs F1–F12) on MX Keys / Craft-class keyboards. `parse_fn_inversion` decodes the low bit of bytes 0 (current) and 1 (default) of the getState reply.

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getState (`0x00`) | `11 dd fi 01 ··` | none | reply byte 0 bit `0x01` = inverted, byte 1 bit `0x01` = default-inverted |
| setState (`0x10`) | `11 dd fi 11 <0/1> ··` | `[0/1]` | `0x01` = F-keys send media by default |
| stateChanged notif | `10 dd fi <data>` | — | `handle_fn_inversion_notif` reads **data byte 1** low bit |

Version gate (`fn_inversion_version`, via FEATURE_SET getFeatureId byte 3): version ≥ 2 supports setState; version 1 is read-only, so the reported bit is echoed back into state on notification without a write.

> Solaar's `K375sFnSwap` additionally sends a current-host prefix byte (from HOSTS_INFO) as a workaround for an MX Keys S firmware bug that reports the wrong host's state. HaloDaemon sends no prefix — reply byte 0 is the state directly — which is correct for tested devices; MX Keys S multi-host may need the prefix.

### BRIGHTNESS_CONTROL (`0x8040`) — `v2/mod.rs`

Keyboard backlight level + optional on/off. `parse_brightness_info` decodes getInfo into a `BrightnessInfo { min, max, has_on_off, steps }`.

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getInfo (`0x00`) | `11 dd fi 01 ··` | none | reply: `[max_be, steps_and_flags, caps, min_be]`; `steps = steps_and_flags & 0x0F`, `has_on_off = caps & 0x04` |
| getBrightness (`0x10`) | `11 dd fi 11 ··` | none | reply bytes 0–1 = level (BE u16) |
| setBrightness (`0x20`) | `11 dd fi 21 <level_be> ··` | level BE u16 | clamped to `[min, max]` before the write |
| getOnOff (`0x30`) | `11 dd fi 31 ··` | none | reply byte 0 bit `0x01` = on (only meaningful when `has_on_off`) |
| setOnOff (`0x40`) | `11 dd fi 41 <0/1> ··` | `[0/1]` | on/off toggle |

---

## 5. RGB — `v2/rgb/`

### RGB_EFFECTS (`0x8071`) — `rgb/effects.rs`

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getInfo / global (`0x00`) | `11 dd fi 01 FF FF 00 ··` | `[0xFF,0xFF,0x00]` | reply byte 2 = zone count |
| getInfo / zone (`0x00`) | `11 dd fi 01 <z> FF 00 ··` | `[zone,0xFF,0x00]` | reply byte 4 = effect count |
| getInfo / effect (`0x00`) | `11 dd fi 01 <z> <slot> 00 ··` | `[zone,slot,0x00]` | reply bytes 2..4 = effect_id (BE); slot with `0x0001` = static |
| setEffect (`0x10`) | `11 dd fi 11 <15-byte block> ··` | 15-byte block | static via `encode_set_effect_static`; native via `NATIVE_EFFECTS` |

Zone discovery (`rgb_build_zones`) reads the global zone count, then per zone the effect count, then scans each slot's effect_id to find the **static** slot (effect_id `0x0001`).

**setEffect 15-byte block** — for a static colour (`encode_set_effect_static`):

```
byte 0   zone index   (0xFF = all zones)
byte 1   effect slot  (the slot whose effect_id is 0x0001)
byte 2-4 R G B
byte 5-14  0x64 0x0B 0xB8 0x64 0x00 0x00 0x00 0x01 0x00 0x00   (static preset, from G HUB)
```

For a **native firmware effect** the block is the effect's 15-byte `base` (byte 0 = zone, byte 1 = slot) with params overlaid (`NATIVE_EFFECTS`): *Color Wave* (fixed preset, no params); *Ripple* (`background` colour bytes 2–4 default `5E 5E 5E`; `rate` ms byte 9, range 2–200; `saturation` byte 5, UI 0–100 scaled ×2.55). Out-of-range values clamp before encoding.

### PER_KEY_LIGHTING_V2 (`0x8081`) — `rgb/per_key.rs`

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getInfo / LED bitmap (`0x00`) | `11 dd fi 01 00 00 <page> ··` | `[0x00,0x00,page]`, pages 0..3 | 3 pages × 13 bytes; set bits = firmware LED IDs 1..31 (`parse_pk_led_bitmap`) |
| setIndividual (`0x10`) | `11 dd fi 11 <k r g b ×4>` | up to 4 × `(key,r,g,b)` | `encode_individual_pairs`; unused entries zero |
| setConsecutive (`0x20`) | `11 dd fi 21 <firstId> <r g b ×5>` | run start + ≤5 colours | streaming-frame run-length (`encode_frame`) |
| setRange (`0x50`) | `11 dd fi 51 00 FF r g b ··` | `[0x00,0xFF,r,g,b]` | paints whole key range |
| frameEnd / commit (`0x70`) | `11 dd fi 71 00 ··` | `[0x00]` | applies queued changes |

**Streaming frames** (`encode_frame`). A canvas frame is diffed against the last sent frame, then run-length-encoded: equal-colour spans → `setRange` (0x50), consecutive-id varying runs → `setConsecutive` (0x20), each frame ending in `frameEnd` (0x70). An unchanged frame emits no packets, so the bus write is skipped. (Delta-compressed funcs 0x30/0x40 are gated off — the wire layout is unverified against hardware.)

**Explicit per-key writes** (`encode_individual_pairs`) batch up to four `(key, r, g, b)` per `setIndividual`, padding a short final batch with its last pair (so key 0 is never zero-keyed), then `commit`.

### KEYBOARD_LAYOUT_2 (`0x4540`)

`Hidpp20::read_keyboard_layout` reads func `0x00`; reply byte 0 is a country code (1 → US, 13 → CH, 14 → IT).

---

## 6. Keys / remap — `v2/keys.rs`

### REPROG_CONTROLS_V4 (`0x1b04`)

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getControlCount (`0x00`) | `11 dd fi 01 ··` | none | reply byte 0 = control count |
| getCidInfo (`0x10`) | `11 dd fi 11 <index> ··` | `[index]` | reply `[cid_hi,cid_lo,task_hi,task_lo,flags,pos,group,gmask,…]`; flags bit 3 (`0x08`) = divertable |
| setCidReporting (`0x30`) | `11 dd fi 31 <cid_hi cid_lo flags 00 …0>` | 16-byte block (`encode_set_cid_reporting`) | flags bit 0 (`0x01`) = divert; remap target zero (native) |

**setCidReporting 16-byte block:** bytes 0–1 = CID (BE), byte 2 = flags (bit 0 = divert), bytes 3–15 = `0x00` (remap target zero → native action retained). Divert only takes effect in host mode and only for controls whose getCidInfo flags have bit 3 set; to un-divert, resend with flags `0x00`.

### GKEY (`0x8010`) / MOUSE_BUTTON_SPY (`0x8110`)

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| GKEY getCount (`0x00`) | `11 dd fi 01 ··` | none | reply byte 0 = G-key count (only first 16 addressable) |
| GKEY enableSoftwareControl (`0x20`) | `11 dd fi 21 <enabled> ··` | `[0/1]` | **global** toggle, enabled when any mapping exists |
| SPY setSpyState (`0x10`) | `11 dd fi 11 <enabled> ··` | `[0/1]` | **global** host-side press reporting |

Both GKEY and MOUSE_BUTTON_SPY use a single **global** software-control toggle (no per-control divert, unlike REPROG_CONTROLS_V4). The MOUSE_BUTTON_SPY per-button divert (func 0x40) is intentionally **not** sent — its button-id byte is not the bitmap bit index (G HUB addresses right-click, CID 10, as id 5), so addressing by bit index diverted the wrong buttons; until the id encoding is reverse-engineered, a remapped button also fires its native action.

### Button-event notifications

| Source | Payload | Decoder |
|--------|---------|---------|
| REPROG_CONTROLS_V4 `divertedButtonsEvent` | up to 4 × BE-u16 pressed CIDs, zero-padded | `parse_diverted_buttons_event` |
| GKEY / MOUSE_BUTTON_SPY | 16-bit LE press bitmap; bit *N* = CID `N+1` | `parse_button_bitmap_event` |

---

## 7. Audio — `v2/audio.rs`

### SIDETONE (`0x8300`) — gaming headsets

Mic feedback into the earcups: a single unsigned byte, `0–100`.

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| get (`0x00`) | `11 dd fi 01 ··` | none | reply byte 0 = current level |
| set (`0x10`) | `11 dd fi 11 <level> ··` | `[level]` | clamped 0–100 |

### EQUALIZER (`0x8310`) — gaming headsets

A graphic equalizer of **N signed-dB bands** read from the device; no named presets (one editable custom curve). `Hidpp20::read_equalizer` returns `EqReading { info, freqs, bands }`.

| Function | Bytes sent | Params | Notes |
|----------|-----------|--------|-------|
| getInfo (`0x00`) | `11 dd fi 01 ··` | none | reply `[count, dbRange, _, dbMin, dbMax]` |
| getFrequencies (`0x10`) | `11 dd fi 11 <start> ··` | start band index (`group·7`) | reply byte 0 = echo, then per-band freq BE-u16 at offset `2·b+1` |
| getBands (`0x20`) | `11 dd fi 21 00 ··` | read-prefix `0x00` | reply has **no** echoed prefix — `count` signed `i8` dB from byte 0 |
| setBands (`0x30`) | `11 dd fi 31 02 <b0> <b1> … ··` | `0x02` then `count` signed `i8` | write-prefix `0x02` selects the custom band set |

**getInfo fix-up:** `dbMin`/`dbMax` of 0 mean "use ∓`dbRange`" — `[4, 12, 0, 0, 0]` → 4 bands spanning −12..+12 dB. **Band encoding:** the wire byte *is* the signed dB (`-12 → 0xF4`, `+6 → 0x06`), clamped to `[dbMin, dbMax]` — no bias or scale. A round-trip property test lives in `v2/audio.rs`.

**Headset discovery & transport.** LIGHTSPEED headsets (PRO X `0x0ABA`, PRO X 2 `0x0AF7`, G733 `0x0AB5`/`0x0AFE`, G535 `0x0AC4`, G935 `0x0A87`, G533 `0x0A66`) enumerate as a **single composite USB device** — headset and dongle share one PID, no separate receiver. HID++ rides **interface 3** (vendor usage page `0xFF43`; `0` on Linux hidraw), addressed at device index `0xFF`, and the interface declares **no short report** — `open_wired` with `DirectReport::LongOnly` forces every request long. On Linux one hidraw node carries both report IDs as a single handle.

---

## 8. Responses

- **getFeature** (ROOT `0x00`) — byte 0 = the feature's runtime index; `0` = absent.
- **Battery** (UNIFIED getStatus) — byte 0 = percent, byte 2 ≠ 0 → charging; voltage features decode BE-u16 mV + status byte.
- **DPI** (`parse_current_dpi`) — DPI big-endian; longer replies echo the sensor in byte 0 (DPI in 1..3), shorter put DPI in 0..2. A decoded `0` reads as unknown (`None`).
- **Error replies** — sub-ID `0x8F`/`0xFF`; see [hidpp.md](hidpp.md).
- **Firmware / device name** — resolved at enumeration; firmware-string decode is not invoked.
