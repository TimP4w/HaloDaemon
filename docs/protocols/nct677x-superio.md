# NCT677x Super I/O Protocol

Nuvoton/Winbond NCT677x Super I/O hardware-monitor register protocol — temperature/fan/PWM monitoring and manual PWM fan control over the LPC Super I/O index/data ports and a bank-addressed runtime HWM I/O window.

**Credits:** MPL-2.0, LibreHardwareMonitor contributors — derived from `Nct677X.cs` and `LpcPort.cs` ([LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor)).

Byte movement runs over the [LpcIO transport](../transports/lpcio.md) — that doc covers `superio_inb`/`superio_outb`, `read_port`/`write_port`, `select_slot`, and `find_bars`; this doc covers the register protocol layered on top.

---

## Overview

The NCT677x is a Nuvoton (formerly Winbond) Super I/O chip found on most consumer AMD/Intel motherboards. At protocol level it exposes a hardware monitor (HWM) for temperatures, fan tachometers, and PWM fan control, reached over the classic LPC **Super I/O index/data port model**: write a register index to an index port, then read or write a data port. **Windows only** (`#![cfg(target_os = "windows")]`); the daemon moves bytes through PawnIO's `LpcIO.bin` module via the [LpcIO transport](../transports/lpcio.md) (root/Administrator required).

There are **two register spaces**:

1. **Super I/O config space** — the index/data port pair at `0x2E`/`0x2F` (slot 0) or `0x4E`/`0x4F` (slot 1). Entered with the magic `0x87 0x87` sequence (extended-function mode, EFM). Used to read chip ID + revision and to look up the runtime HWM base address.
2. **Runtime HWM window** — a small I/O window discovered via the config space, accessed at `hwm_base + 5` (index/address port) and `hwm_base + 6` (data port). HWM registers are **bank-addressed**: select a bank, then index a register within it.

The daemon probes both LPC ports (`PROBE_PORTS = [0x2E, 0x4E]`), each with its own `LpcIoBus` instance so per-slot BAR registration from one chip doesn't clobber the other.

### Chip families

The chip is identified by two config registers — the chip ID at CR `0x20` and the revision at CR `0x21` — and that pair selects the variant, which in turn selects every register map. The variants fall into a few families that behave differently at protocol level:

- **classic / low-end banked** (NCT6771F, NCT6776F, NCT610XD) — bank-addressed HWM, no I/O-space lock.
- **modern banked** (NCT6779D) — bank-addressed HWM, no I/O-space lock.
- **modern banked (lock)** (the NCT679xD line, e.g. NCT6796D/6796DR/6799D/6701D) — bank-addressed HWM with a re-engaging I/O-space lock that must be cleared and re-asserted (§2 *I/O-space unlock*).
- **EC-family** (NCT6683D/6686D/6687D) — a different register layout; per-channel fan control and temperature decode are not supported here.

`id == 0x00`/`0xFF` ⇒ no chip; an unrecognised `(id, revision)` ⇒ probe abandoned. **The full chip-ID/revision → variant table (the source of truth) is in [§3 Supported chip IDs](#supported-chip-ids).**

---

## 1. Packet layout

There is no multi-byte packet; the wire unit is a single index-then-data port transaction. Operations are sequences of these.

### EFM framing (config space)

Config space must be unlocked before any CR access and re-locked after:

- **Enter EFM** (`enter`): `write_port(port, 0x87)` then `write_port(port, 0x87)` (two writes of `0x87` to the index port `0x2E` or `0x4E`).
- **Exit EFM** (`exit`): `write_port(port, 0xAA)`.

### Config-space register access

After EFM is entered, config registers (CRs) are 8-bit and accessed via the transport's `superio_inb(reg)` / `superio_outb(reg, val)`, which knows the index/data port pair from the most recent `select_slot`. The PawnIO module writes `reg` to the index port and reads/writes the data port internally.

Key config registers:

| Register | CR | Purpose |
|----------|-----|---------|
| `CHIP_ID_REGISTER` | `0x20` | Chip ID byte |
| `CHIP_REVISION_REGISTER` | `0x21` | Revision byte |
| `DEVICE_SELECT_REGISTER` | `0x07` | Logical device number (LDN) select |
| `BASE_ADDRESS_REGISTER` | `0x60` | HWM base address high byte (low byte at `0x61`) |
| `NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK` | `0x28` | I/O-space lock control (bit 4 = lock) |
| `WINBOND_NUVOTON_HARDWARE_MONITOR_LDN` | `0x0B` | HWM logical device number |

### Bank-addressed HWM access

HWM registers are 16-bit values where **the high byte is the bank number and the low byte is the register within that bank**. Runtime offsets from the HWM base:

- `ADDRESS_REGISTER_OFFSET = 0x05` → index/address port at `hwm_base + 5`
- `DATA_REGISTER_OFFSET = 0x06` → data port at `hwm_base + 6`
- `BANK_SELECT_REGISTER = 0x4E` → the index value that selects "set the bank"

**`read_hwm(bus, hwm_base, register)`**:
```
addr = hwm_base + 0x05
data = hwm_base + 0x06
bank = register >> 8
reg  = register & 0xFF
write_port(addr, 0x4E)    // address the bank-select register
write_port(data, bank)    // select bank
write_port(addr, reg)     // address the target register
read_port(data)           // → u8
```

**`write_hwm(bus, hwm_base, register, value)`** is identical but ends with `write_port(data, value)` instead of a read. Every access re-selects the bank; there is no persistent "current bank" assumption.

A 16-bit count (e.g. a fan tachometer) lives in two consecutive registers: high byte at `reg`, low byte at `reg + 1`.

---

## 2. Functions

Notation: `inb(cr)` / `outb(cr,v)` are config-space `superio_inb`/`superio_outb` (valid only inside EFM); `outb_port(v)` is a raw `write_port` to the index port (`0x2E` or `0x4E`); `rd(reg16)` / `wr(reg16,v)` are the bank-addressed HWM `read_hwm`/`write_hwm` (each expands to the 4-step port sequence in *read_hwm* / *write_hwm* below). `reg16` is `(bank << 8) | register`.

| Function | Port/register sequence (exact values) | Params | Required sequence / notes |
|----------|----------------------------------------|--------|----------------------------|
| `enter` | `outb_port(0x87); outb_port(0x87)` | — | unlock config space before any CR access |
| `exit` | `outb_port(0xAA)` | — | re-lock config space |
| `detect` | `enter` → `inb(0x20)`,`inb(0x21)` → `find_bars` → `outb(0x07,0x0B)` → `inb(0x60)`,`inb(0x61)` → (lock clear) → `exit` | `port` | see **detect** |
| `read_hwm` | `outb_port'(0x4E)`→`data←bank`→`outb_port'(reg)`→read `data` | `hwm_base, reg16` | bank-then-index, see **read_hwm** |
| `write_hwm` | `outb_port'(0x4E)`→`data←bank`→`outb_port'(reg)`→`data←v` | `hwm_base, reg16, v` | see **write_hwm** |
| `keep_io_unlocked` | `is_io_unlocked`; if locked: `enter`→`inb(0x28)`→`outb(0x28, v & !0x10)`→`exit` | `chip` | re-assert each cycle, see **I/O-space unlock** |
| `is_io_unlocked` | `rd(hi_reg)`,`rd(lo_reg)`; true iff `(hi<<8) \| lo == 0x5CA3` | `chip` | vendor-ID regs `0x804F/0x004F` (modern) or `0x80FE/0x00FE` |
| `read_all_temperatures` | per slot: `rd(source_reg) & 0x1F`, `rd(int_reg)`, `rd(half_reg)` | `chip` | see **read temperature** |
| `read_rpm` | `hi=rd(reg)`, `lo=rd(reg+1)` | `chip, channel` | `reg = fan_count_regs[channel]`, see **read fan RPM** |
| `read_duty` | `raw = rd(fan_pwm_out_regs[channel])` | `chip, channel` | trivial — returns `(raw*100+127)/255` rounded (§3) |
| `read_ctrl_mode` | `rd(fan_ctrl_mode_regs[channel])` | `chip, channel` | trivial — saved at init; `None` for EC-family |
| `restore_ctrl_mode` | `wr(fan_ctrl_mode_regs[channel], mode)` | `chip, channel, mode` | trivial — written back on close; no-op for EC-family |
| `set_duty` | `keep_io_unlocked`→`wr(fan_ctrl_mode_regs[channel], 0)`→`wr(fan_pwm_cmd_regs[channel], duty*255/100)` | `chip, channel, duty 0–100` | mode-before-duty, see **set fan duty** |

In the `read_hwm`/`write_hwm` rows, `outb_port'` denotes a write to the HWM **index** port `hwm_base + 5`; `data` is the HWM **data** port `hwm_base + 6`.

### detect

Identifies the chip and discovers its runtime HWM window. The caller must already have run `bus.select_slot(port_slot(port))` so the transport drives the right index/data port pair.

1. **Enter EFM:** `outb_port(0x87); outb_port(0x87)`.
2. **Read identity:** `id = inb(0x20)` (chip ID), `revision = inb(0x21)`. If `id == 0x00` or `0xFF`, the slot is empty → `exit`, return "no chip". Map `(id, revision)` to a variant (§3 chip-ID table); on no match → `exit`, reject.
3. **Find BARs:** `find_bars()` — registers the runtime I/O BAR range with PawnIO. Must run while still in EFM.
4. **Select HWM logical device:** `outb(0x07, 0x0B)` — write LDN `0x0B` (the Winbond/Nuvoton hardware-monitor logical device) to the device-select CR `0x07`.
5. **Read base-address pair:** `hi = inb(0x60)`, `lo = inb(0x61)` → `hwm_base = (hi << 8) | lo` (e.g. `0x0290`). This is the base of the runtime HWM window; `0x0000` means "no HWM window".
6. **Clear the I/O-space lock** (modern locked variants only — see §3 chip-ID *Family* column): `inb(0x28)`; if bit 4 (`0x10`) is set, `outb(0x28, options & !0x10)`. Skipped otherwise.
7. **Exit EFM:** `outb_port(0xAA)`.

### read_hwm

Reads one byte from the bank-addressed HWM window. `reg16` splits into `bank = reg16 >> 8` and `register = reg16 & 0xFF`. With `addr = hwm_base + 5` (index port) and `data = hwm_base + 6` (data port):

1. `write_port(addr, 0x4E)` — point the index at the **bank-select** register (`BANK_SELECT_REGISTER`).
2. `write_port(data, bank)` — select the bank.
3. `write_port(addr, register)` — point the index at the target register.
4. `read_port(data)` → the register byte.

The bank is re-selected on every call; there is no persistent "current bank".

### write_hwm

Identical to *read_hwm* but the final step writes instead of reads:

1. `write_port(addr, 0x4E)`.
2. `write_port(data, bank)`.
3. `write_port(addr, register)`.
4. `write_port(data, value)`.

### I/O-space unlock (`keep_io_unlocked`)

The modern NCT679xD locked variants can **re-engage the HWM I/O-space lock between accesses** under BIOS power management; while locked, every read at `hwm_base + 5/+6` returns `0`. This routine re-opens the window and is run at the start of every poll cycle and before every fan write.

1. If the variant doesn't carry the lock (see §3 chip-ID *Family*), return immediately.
2. **Liveness check** (`is_io_unlocked`): `rd(hi_reg)`, `rd(lo_reg)` from the vendor-ID register pair; if `(hi << 8) | lo == 0x5CA3` (the Nuvoton vendor ID), the window is open → return.
3. Otherwise re-clear the lock: `enter` EFM, `options = inb(0x28)`, and if bit 4 (`0x10`) is set `outb(0x28, options & !0x10)`, then `exit`.

### read temperature (`read_all_temperatures`)

Each variant exposes a list of temperature **slots**, each a tuple `(int_reg, half_reg, half_bit, source_reg)` (§3 temperature decode). The chip dynamically maps physical sensors into slots, so each slot's source register must be read to label it. For every slot:

1. **Resolve the source:** `source = rd(source_reg) & 0x1F` (low 5 bits; upper bits reserved). If `source_reg == 0` (fixed slot, unlabelable) or `source == 0` (nothing mapped) → skip.
2. **Dedupe:** if a reading from this `source` byte was already collected this cycle → skip (tracked by a 64-bit `seen_sources` bitmask).
3. **Integer part:** `int_byte = rd(int_reg)`; interpret as **signed** (`i8`) and shift left 1: `value = (int_byte as i8) << 1` — i.e. value is now in **half-degree units**.
4. **Half-degree bit:** if `half_bit != 0xFF`, OR in the half-degree bit: `value |= (rd(half_reg) >> half_bit) & 1`.
5. **Scale and bound:** `temperature_c = value * 0.5`. Discard the reading if it falls outside `-55.0 ..= 125.0` °C (the sensor's valid range).

The kept readings are labelled via the source map (§3) and returned as `(source, label, temperature_c)`.

### read fan RPM (`read_rpm`)

The tachometer count is a 16-bit value in two consecutive registers. With `reg = fan_count_regs[channel]` (§3; returns `0` if `channel` is out of range or `hwm_base == 0`):

1. `high = rd(reg)` — high byte at the channel's count register.
2. `low = rd(reg + 1)` — low byte at the next register.
3. Decode to RPM via the count formula in §3, selected by `fan_count_is_16bit()`: NCT6771F uses the 16-bit `(high << 8) | low` decode (`rpm_from_count_bytes_16bit`); all other variants use the 13-bit decode (`rpm_from_count_bytes`). Counts outside the MIN/MAX window decode to `0` RPM (no/stalled fan).

### set fan duty (`set_duty`)

Switches a channel to manual PWM control and writes the duty. **Order matters: mode before duty** — the command register is only honoured once the channel is in manual mode.

1. `keep_io_unlocked` (re-open the HWM window if the chip re-locked it).
2. If `hwm_base == 0` → no-op. If the chip is **EC-family** (NCT668x) → return an error: those chips share one control-mode register across all channels (a plain write would clobber the others), so per-channel manual control is unsupported here (see §Notes).
3. **Enter manual control:** `wr(fan_ctrl_mode_regs[channel], 0)` — writing `0` selects manual mode.
4. **Write duty:** `wr(fan_pwm_cmd_regs[channel], ((duty * 255 + 50) / 100) as u8)` — the 0–100 % duty is scaled to the 0–255 PWM range with rounding (§3).

The original control-mode byte is captured at device init (`read_ctrl_mode`) and written back on `close` (`restore_ctrl_mode`) to hand the channel back to BIOS/automatic control.

---

## 3. Parameters

This section defines every value, range, formula, and register array in full; the code is cited only as provenance.

### Supported chip IDs

The chip is identified by two config registers: the **chip ID** at CR `0x20` and the **revision** at CR `0x21`. The pair selects the variant, which in turn selects every register array below. Revisions are matched either as a full byte or by their **high nibble** (`revision & 0xF0`) as noted. The *Family* column drives behaviour: **modern banked (lock)** variants carry the re-engaging HWM I/O-space lock (§2 *I/O-space unlock*); **EC-family** variants use a different register layout and do not support per-channel fan control or temperature decode.

| Variant | ID (`0x20`) | Revision (`0x21`) | Fans | Family |
|---------|-------------|-------------------|------|--------|
| NCT6771F | `0xB4` | high nibble `0x70` | 4 | classic banked |
| NCT6776F | `0xC3` | high nibble `0x30` | 5 | classic banked |
| NCT610XD | `0xC4` | high nibble `0x50` | 3 | low-end banked |
| NCT6779D | `0xC5` | high nibble `0x60` | 5 | modern banked |
| NCT6683D | `0xC7` | `0x32` | 8 | EC-family |
| NCT6791D | `0xC8` | `0x03` | 6 | modern banked (lock) |
| NCT6792D | `0xC9` | `0x11` | 6 | modern banked (lock) |
| NCT6792DA | `0xC9` | `0x13` | 6 | modern banked (lock) |
| NCT6793D | `0xD1` | `0x21` | 6 | modern banked (lock) |
| NCT6795D | `0xD3` | `0x52` | 6 | modern banked (lock) |
| NCT6796D | `0xD4` | `0x23` | 7 | modern banked (lock) |
| NCT6796DR | `0xD4` | `0x2A` | 7 | modern banked (lock) |
| NCT6797D | `0xD4` | `0x51` | 7 | modern banked (lock) |
| NCT6798D | `0xD4` | `0x2B` | 7 | modern banked (lock) |
| NCT6686D | `0xD4` | `0x40` or `0x41` | 8 | EC-family |
| NCT6687D | `0xD5` | `0x92` | 8 | EC-family |
| NCT6799D | `0xD8` | `0x02` | 7 | modern banked (lock) |
| NCT6701D | `0xD8` | `0x06` | 7 | modern banked (lock) |

`id == 0x00`/`0xFF` ⇒ no chip; any other `(id, revision)` ⇒ unrecognised, probe abandoned.

### Vendor ID

The Nuvoton vendor ID is **`0x5CA3`**. It is read back from the HWM window (high byte then low byte of the vendor-ID register pair, assembled as `(hi << 8) | lo`) purely to confirm the HWM I/O-space lock is currently clear — a successful match means runtime registers will return real data. The register pair depends on the variant:

| Variants | High-byte reg | Low-byte reg |
|----------|---------------|--------------|
| NCT610XD, NCT6776F, NCT6771F | `0x80FE` | `0x00FE` |
| all modern NCT67xxD (incl. NCT6701D, NCT6796DR, NCT6799D) | `0x804F` | `0x004F` |

### Temperature decode

A temperature **slot** is `(int_reg, half_reg, half_bit, source_reg)`:

- **`int_reg`** — HWM register holding the integer temperature as a **signed** byte (°C).
- **`half_reg`** / **`half_bit`** — register and bit index holding the half-degree (0.5 °C) bit. `half_bit == 0xFF` means "no half-degree precision for this slot".
- **`source_reg`** — register returning the source byte that says *which physical sensor* is mapped to this slot. `source_reg == 0` means the slot is unlabelable and is skipped.

**Decode formula** (per §2 *read temperature*): read `int_reg` as `i8`, shift left 1 to make room for the half bit, OR in the half-degree bit, multiply by 0.5:

```text
value = (int_reg_byte as i8) << 1
if half_bit != 0xFF: value |= (half_reg_byte >> half_bit) & 1
temperature_c = value * 0.5        // valid range -55.0 ..= 125.0 °C
```

**Worked example:** `int_reg` reads `0x2A` (= 42 as `i8`) and `half_reg` bit 7 is set. Then `value = 42 << 1 = 84`, `value |= 1 → 85`, `temperature_c = 85 * 0.5 = 42.5 °C`. A negative example: `int_reg = 0xFB` (= −5 as `i8`), half bit clear → `value = −5 << 1 = −10`, `temperature_c = −5.0 °C`.

**Slot tables.** Each variant family has its own slot list. The modern NCT67xxD default (7 slots) and the classic NCT6776F/NCT6771F (4 slots) tables:

| Slot | Modern `(int, half, bit, source)` | NCT6776F/NCT6771F `(int, half, bit, source)` |
|------|------------------------------------|-----------------------------------------------|
| 0 | `(0x073, 0x074, 7, 0x100)` | `(0x027, 0x000, 0xFF, 0x621)` |
| 1 | `(0x075, 0x076, 7, 0x200)` | `(0x073, 0x074, 7, 0x100)` |
| 2 | `(0x077, 0x078, 7, 0x300)` | `(0x075, 0x076, 7, 0x200)` |
| 3 | `(0x079, 0x07A, 7, 0x800)` | `(0x077, 0x078, 7, 0x300)` |
| 4 | `(0x07B, 0x07C, 7, 0x900)` | — |
| 5 | `(0x07D, 0x07E, 7, 0xA00)` | — |
| 6 | `(0x4A0, 0x49E, 6, 0xB00)` | — |

**NCT610XD** uses its own 4-slot table:

| Slot | `(int_reg, half_reg, half_bit, source_reg)` |
|------|---------------------------------------------|
| 0 | `(0x06B, 0x000, 0xFF, 0x621)` |
| 1 | `(0x010, 0x016, 0,   0x000)` |
| 2 | `(0x011, 0x01B, 1,   0x000)` |
| 3 | `(0x012, 0x01B, 2,   0x000)` |

(`half_bit = 0xFF` on slot 0 means no half-degree register — integer only. `source_reg = 0x000` on slots 1–3 means no source mapping.)

**EC-family** variants (`NCT6683D`, `NCT6686D`, `NCT6687D`, `NCT6687DR`) return an empty slot list — their temperatures are not decoded via this path.

**Source byte → label** (the low 5 bits of `source_reg`):

| Byte | Label | Byte | Label |
|------|-------|------|-------|
| `0` | (nothing mapped — skip) | `17` | CPU (PECI Agent 1) |
| `1` | Motherboard (SYSTIN) | `18` | PCH Chip CPU Max |
| `2` | CPU (CPUTIN) | `19` | PCH Chip |
| `3` | Auxiliary 0 | `20` | PCH CPU |
| `4` | Auxiliary 1 | `21` | PCH MCH |
| `5` | Auxiliary 2 | `22` | Agent 0 DIMM 0 |
| `6` | Auxiliary 3 | `23` | Agent 0 DIMM 1 |
| `7` | Auxiliary 4 | `24` | Agent 1 DIMM 0 |
| `8` | SMBus Master 0 | `25` | Agent 1 DIMM 1 |
| `9` | SMBus Master 1 | `26` | Byte Temp 0 |
| `10` | T-Sensor | `27` | Byte Temp 1 |
| `16` | CPU Package (PECI Agent 0) | `28` | PECI Agent 0 Calibrated |
| `31` | Virtual | `29` | PECI Agent 1 Calibrated |

Any other value ⇒ `Unknown`.

### Fan RPM (13-bit counter)

The tachometer is a 13-bit count assembled from two register bytes, then converted to RPM:

```text
count = (high << 5) | (low & 0x1F)          // 13-bit value
MIN = 0x15 (21)   MAX = 0x1FFF (8191)
if count < MIN or count >= MAX:  rpm = 0     // no fan / stalled / out of range
else:                            rpm = 1_350_000 / count
```

The `count` is the period of one tachometer pulse in clock ticks; `1_350_000` is the tach clock, so RPM falls as the period grows. **`MIN = 0x15`** rejects implausibly fast counts (noise); **`MAX = 0x1FFF`** is the counter's saturation value, reported when no fan is spinning. **Worked example:** `high = 28`, `low = 4` → `count = (28 << 5) | 4 = 900` → `rpm = 1_350_000 / 900 = 1500`.

> **Variant scope.** This 13-bit `(high << 5) | (low & 0x1F)` packing is the modern **NCT67xxD** scheme (also used by the **NCT6776F** and **NCT610XD**) and covers our actual target hardware. The split is subtler than "6776/6771 are 16-bit": the Linux `nct6775` driver reads a true **16-bit** count only for **NCT6775F**, and treats **NCT6776F** as **13-bit** like the modern parts. The one genuine 16-bit part in our variant table is the oldest **NCT6771F**, so `read_rpm` branches on `fan_count_is_16bit()`: NCT6771F decodes via `rpm_from_count_bytes_16bit` (`count = (high << 8) | low`, `0x0000`/`0xFFFF` → 0 RPM), everything else via the 13-bit formula above. NCT6775F itself is not in our variant table.

**Per-variant count registers** (`fan_count_regs`, high byte at the listed register, low byte at register `+1`):

| Family | Count registers |
|--------|-----------------|
| modern NCT67xxD (default) | `0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC` |
| NCT6776F / NCT6771F | `0x656, 0x658, 0x65A, 0x65C, 0x65E` |
| NCT610XD | `0x030, 0x032, 0x034` |
| EC-family (NCT668x) | `0x140, 0x142, 0x144, 0x146, 0x148, 0x14A, 0x14C, 0x14E` |

### PWM duty (0–255 ↔ 0–100%)

The chip stores duty as a raw byte **0–255** = 0–100 % linearly. The daemon exposes duty as a **percentage 0–100** and converts in both directions:

- **Read-back** (raw → percent): `percent = (raw * 100 + 127) / 255` (rounding integer division). E.g. `0xFF → 100`, `0x80 → 50`, `0x00 → 0`.
- **Write** (percent → raw): `raw = (duty * 255 + 50) / 100` (rounding integer division). E.g. `100 → 255`, `50 → 128`, `0 → 0`.

**PWM read-back registers** (`fan_pwm_out_regs`):

| Family | Read-back registers |
|--------|---------------------|
| modern NCT67xxD (default) | `0x001, 0x003, 0x011, 0x013, 0x015, 0x017, 0x029` |
| NCT6797D / 6798D / 6799D / 5585D | `0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09` |
| NCT610XD | `0x04A, 0x04B, 0x04C` |
| EC-family (NCT668x) | `0x160, 0x161, 0x162, 0x163, 0x164, 0x165, 0x166, 0x167` |

**PWM command (write) registers** (`fan_pwm_cmd_regs`, written in manual mode):

| Family | Command registers |
|--------|-------------------|
| modern NCT67xxD (default) | `0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09` |
| NCT610XD | `0x119, 0x129, 0x139` |
| EC-family (NCT668x) | `0xA28, 0xA29, 0xA2A, 0xA2B, 0xA2C, 0xA2D, 0xA2E, 0xA2F` |

### Fan control mode

Each channel has a control-mode register. **Writing `0` selects manual PWM control**; any other value is a hardware/automatic mode whose original byte is saved at init and restored on close (the daemon treats it as opaque). Registers (`fan_ctrl_mode_regs`):

| Family | Control-mode registers |
|--------|------------------------|
| modern NCT67xxD (default) | `0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02` |
| NCT610XD | `0x113, 0x123, 0x133` |
| EC-family (NCT668x) | `0xA00` for **all** channels (one shared register) |

Because the EC-family register is shared across channels (manual select is bit `1 << channel` within it), writing it for one channel would clobber the others — so per-channel manual control is rejected for EC-family chips (see §2 *set fan duty* and §Notes).

---

## 4. Responses

All reads return a single `u8` from the data port; multi-byte values are assembled host-side.

| Read | Source | Decode |
|------|--------|--------|
| Chip ID / revision | `inb(0x20)`, `inb(0x21)` | `(id, revision)` → `from_id` selects the variant + its register maps |
| HWM base address | `inb(0x60)` (hi), `inb(0x61)` (lo) | `hwm_base = (hi << 8) \| lo`; `0x0000`/EC-family ⇒ HWM path returns no data |
| I/O-space lock | `inb(0x28)` | bit 4 (`0x10`) set ⇒ HWM window locked; every `base+5/+6` read returns `0` until cleared |
| Vendor ID | `rd(0x804F/0x80FE)`, `rd(0x004F/0x00FE)` | `(hi<<8)\|lo == 0x5CA3` ⇒ HWM unlocked |
| Temperature | `rd(source_reg)`, `rd(int_reg)`, `rd(half_reg)` | source `& 0x1F` labels it; `(int as i8)<<1 \| half_bit`, ×0.5 → °C |
| Fan RPM | `rd(reg)`, `rd(reg+1)` | `count=(hi<<5)\|(lo&0x1F)`; `1_350_000/count`, clamped |
| PWM duty | `rd(fan_pwm_out_regs[ch])` | `(raw*100+127)/255` → 0–100% (rounded) |
| Fan mode | `rd(fan_ctrl_mode_regs[ch])` | raw mode byte, opaque; saved for restore |

On any HWM read error, or `hwm_base == 0`, RPM/duty default to `0` and temperatures to an empty list rather than surfacing an error.

---

## 5. Polling & notifications

None — all access is host-initiated register reads; HaloDaemon re-reads on its own cadence (sensor + fan devices poll once per second). The chip raises no asynchronous events. Note that on the modern NCT679xD family the I/O-space unlock must be **re-asserted each access cycle** before reading HWM registers (see §2 *I/O-space unlock*), because BIOS power management can re-engage the lock between accesses.

---

## Notes

- **Windows only.** The `superio` module and the LpcIO transport are gated `#![cfg(target_os = "windows")]`. Port I/O goes through PawnIO's signed `LpcIO.bin` and requires Administrator; if the blob is unavailable or the daemon isn't elevated, no SuperIO devices appear. See the [LpcIO transport](../transports/lpcio.md). (On Linux, motherboard fans/temps come from the [hwmon transport](../transports/hwmon.md) instead.)
- **Unmatched chip IDs are rejected**, not guessed. `id == 0x00/0xFF` ⇒ no chip; an unrecognised `(id, revision)` ⇒ probe abandoned. `Nct5585D` and `Nct6687DR` exist in the variant enum/tables but have **no `from_id` match arm**, so detection never produces them.
- **`0x4BA → 0x4CC` fan-register stride oddity.** The modern-family `fan_count_regs` default ends `…0x4B8, 0x4BA, 0x4CC` — the 7th channel breaks the `0x4B0, 0x4B2, …` +2 stride. Documented verbatim from the source (matches LibreHardwareMonitor); flagged as a likely upstream quirk rather than corrected.
- **EC-family (NCT6683D/6686D/6687D/6687DR) fan control is unsupported.** All channels share one control-mode register (`0xA00`) selected by bit `1 << channel`; a plain write would clobber other channels' bits, so `set_duty` errors and `read_ctrl_mode`/`restore_ctrl_mode` return early (`is_ec_family`). Their temp-slot table is empty, so EC-family temperatures are not decoded.
- **Read-modify-write hazard on CR `0x28`.** The lock clear reads then masked-writes the register; concurrent BIOS access between the read and write isn't guarded at the firmware level (the daemon holds the bus mutex within-process only).
