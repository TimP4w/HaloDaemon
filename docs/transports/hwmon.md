# hwmon Transport

Linux hardware monitoring sysfs transport for temperature sensors.

**Source:** `src/daemon/src/drivers/transports/hwmon.rs`

**Platform:** Linux only

---

## Overview

The Linux `hwmon` subsystem exposes sensor data from CPUs, GPUs, NVMe drives, and embedded controllers at `/sys/class/hwmon/hwmon*/`. HaloDaemon uses this transport for two purposes: reading `temp*_input` files (millidegrees Celsius) to surface temperatures as fan curve sources, and writing `pwm*` / `pwm*_enable` sysfs files to control motherboard fan header duty cycles. The udev rules in `udev/60-halod.rules` grant write permission to these files on device add.

---

## Discovery

`HwmonTransport::discover()` enumerates all `/sys/class/hwmon/hwmon*` directories, creates a `HwmonDevice` for each, and calls `initialize()`.

---

## Polling

Each `HwmonDevice` spawns a Tokio task that reads all `temp*_input` files under its sysfs path every 1 second. The latest readings are cached in a `Mutex<Vec<Sensor>>`; the IPC serializer reads from this cache without triggering sysfs I/O.

---

## Stable IDs

The `hwmonN` index suffix is dynamic and changes across reboots. HaloDaemon derives a stable ID by resolving the sysfs symlink to its canonical `/sys/devices/...` path, stripping the leading prefix and trailing `hwmonN` component, and replacing non-alphanumeric characters with underscores.

Example: `/sys/devices/pci0000:00/0000:00:18.3/hwmon/hwmon6` → `pci0000_00_0000_00_18_3`

This ID is used in fan curve sensor assignments so they survive reboots.

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
- Module dependency — sensors and fan headers only appear if the corresponding kernel module is loaded. Missing modules produce missing devices, not errors.
