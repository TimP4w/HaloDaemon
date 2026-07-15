# hwmon Plugin Transport

Scoped Linux hardware-monitoring transport for integration plugins.

**Platform:** Linux only

---

## Overview

The Linux `hwmon` subsystem exposes sensor data from CPUs, GPUs, NVMe drives,
and embedded controllers at `/sys/class/hwmon/hwmon*/`. The official
[`hwmon`](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/hwmon)
integration uses this host transport to surface temperatures and motherboard
fan headers. The udev rules in `udev/60-halod.rules` grant write permission to
approved PWM attributes on device add.

---

## Discovery

The host enumerates every `hwmon*` directory and gives the integration an
opaque key, stable identifier, display name, and allowlisted attribute names.
Filesystem paths are never exposed to Lua. Reads are limited to `name`,
`tempN_input`/`label`, `fanN_input`/`label`, and `pwmN`/`pwmN_enable`; only PWM
and PWM-enable attributes are writable.

---

## Polling

The generic plugin worker samples sensor and fan callbacks every second. The
integration also caches each result for one second so multiple consumers share
the same sysfs snapshot.

Before the first `pwmN_enable` mutation, the host records its original value.
Transport teardown restores every recorded value independently of Lua cleanup,
including after a callback timeout or error.

---

## Stable IDs

The `hwmonN` index suffix is dynamic and changes across reboots. HaloDaemon derives a stable ID by resolving the sysfs symlink to its canonical `/sys/devices/...` path, stripping the leading prefix and trailing `hwmonN` component, and replacing non-alphanumeric characters with underscores.

Example: `/sys/devices/pci0000:00/0000:00:18.3/hwmon/hwmon6` → `pci0000_00_0000_00_18_3`

The integration retains the former built-in device and sensor ID formats, so
fan curve assignments and visibility settings survive the port.

---

## Kernel module dependency

Sensors only appear if the corresponding kernel module is loaded:

| Sensor | Module |
|--------|--------|
| AMD CPU | `k10temp` |
| Intel CPU | `coretemp` |
| Nuvoton NCT677x SuperIO (motherboard) | `nct6775` |
| ITE SuperIO | `it87` |

Missing modules produce missing sensors, not errors. NixOS users: the NixOS module loads `nct6775` at boot automatically.

---

## Limitations

- Linux only — excluded from Windows builds at the compiler level.
- Opt-in plugin — install and approve the official hwmon integration before
  sensors and fan headers appear.
- Module dependency — sensors and fan headers only appear if the corresponding kernel module is loaded. Missing modules produce missing devices, not errors.
