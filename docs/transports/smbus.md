# SMBus Transport

System Management Bus (I2C) transport for DRAM and GPU RGB controllers.

**Platform:** Linux (`/dev/i2c-*`), Windows (PawnIO chipset + NvAPI GPU)

---

## Overview

SMBus is a two-wire serial protocol derived from I2C, used on PC motherboards to communicate with voltage regulators, SPD EEPROMs, and RGB controllers. HaloDaemon uses it to address ENE RGB controllers on DRAM sticks and GPUs.

---

## Bus discovery

Two bus kinds:

| Kind | Detection |
|------|-----------|
| Chipset | Adapter name does not contain "nvidia", "amd radeon", or "radeon" |
| GPU | Adapter name contains "nvidia", "amd radeon", or "radeon" |

On Linux, buses are enumerated from `/dev/i2c-*` with adapter names read from `/sys/class/i2c-adapter/`. On Windows, chipset buses are enumerated via WMI, GPU buses via `NvAPI_EnumPhysicalGPUs`. Each `BusInfo` also carries the bus's PCI ids (vendor, device, subsystem vendor/device), read from sysfs on Linux and NvAPI on Windows.

### GPU-bus PCI gate

A GPU's I²C segment is shared with the monitor's DDC/EDID lines, so poking an RGB address on an unrecognised card can wedge the bus — tripping driver resets or knocking out display detection. The DRAM/chipset bus has no such coupling.

The scanner therefore **gates every GPU-bus scan on the card's PCI id**. A scan entry (native `SmBusScanEntry` or a plugin's `pci_match`) declares which cards it covers; before opening a GPU bus the scanner keeps only buses matching an entry:

- **no match** → the bus is never opened or probed;
- **`confirmed` match** → emitted with no probe transaction (curated whitelist);
- **any other match** → confirmed with the entry's probe — prefer `read_byte` over the intrusive `write_quick`.

A GPU scan with an empty gate is refused (rejected at load for plugins; skipped with a warning for native entries). Chipset buses are ungated by default.

---

## Operations

All SMBus calls are blocking. They are batched via `SmBusDevice::run_batch` — a closure receiving a `&mut dyn SmBusSyncOps` reference, dispatched in a single `tokio::task::spawn_blocking` call. This keeps the async executor unblocked and eliminates per-operation overhead.

| Method | SMBus operation |
|--------|-----------------|
| `read_byte(addr)` | Read a single byte |
| `read_byte_data(addr, cmd)` | Read one byte from register `cmd` |
| `write_byte_data(addr, cmd, val)` | Write one byte to register `cmd` |
| `write_quick(addr)` | Zero-length write (ACK probe) |
| `write_word_data(addr, cmd, val)` | Write a 16-bit word to register `cmd` |
| `write_block_data(addr, cmd, data)` | Write up to 32 bytes to register `cmd` |

Block writes are supported on Linux and Windows chipset buses (PawnIO), but **not** on Windows GPU buses (NvAPI). Callers fall back to byte-at-a-time automatically; an 8-LED DIMM costs ~49 transfers per frame instead of 2 in the fallback path.

---

## Platform files

| File | Target | Contents |
|------|--------|---------|
| `mod.rs` | all | Shared types, discovery, `run_batch` |
| `linux.rs` | Linux | i2c-dev ioctl interface |
| `windows/chipset.rs` | Windows | PawnIO chipset SMBus |
| `windows/nvapi.rs` | Windows | NvAPI GPU i2c |
| `fallback.rs` | other | Stub returning "not supported" |

---

## Access requirements

**Linux:** add your user to the `i2c` group and install the udev rule from `udev/60-halod.rules`:
```bash
sudo usermod -aG i2c $USER
sudo cp udev/60-halod.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

**Windows:** requires [PawnIO](https://pawnio.eu/) installed. The non-elevated daemon delegates chipset SMBus access to the on-demand LocalSystem `halod-broker` service; development runs without that service launch only the broker through UAC.

---

## Security note

On Windows `halod.exe` and its plugin host remain at medium integrity. Only `halod-broker.exe` loads PawnIO and performs register-bus operations. The broker pipe is restricted to the coordinator's exact SID/session and authenticated, capability-scoped requests.

---
