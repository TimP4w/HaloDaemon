# hwmon Transport

Scoped access to Linux hardware-monitoring sensors and fan controls for
integration plugins.

**Platform:** Linux only

## Overview

Linux hwmon provides temperatures, fan speeds, and PWM controls for hardware
supported by kernel drivers. HaloDaemon discovers the available hwmon devices
and presents them to an integration plugin as an allowlisted collection.

Plugins receive opaque device keys and attribute names, never filesystem paths.
Reads are limited to supported sensor and fan attributes, while writes are
limited to PWM control. HaloDaemon restores fan-control modes when the transport
is closed, including after plugin failure.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `hwmon_list()` | List the scoped hwmon devices and their available attributes. |
| `hwmon_read(key, attribute)` | Read an allowed sensor, fan, or PWM attribute. |
| `hwmon_write(key, attribute, data)` | Write an allowed PWM or PWM-mode attribute. |

Each listed device includes an opaque key, a stable identifier, a display name,
and the attributes available to the plugin. Unsupported attributes and devices
outside the supplied collection cannot be accessed.

## Discovery and scope

Available devices depend on the Linux kernel modules loaded for the host's CPU,
GPU, storage, and motherboard monitoring chips. Missing drivers produce missing
devices rather than granting broader filesystem access.

The official hwmon integration turns the scoped collection into HaloDaemon
sensor and fan devices.

## Access requirements

Sensor reads normally work for regular users. Fan control requires write
permission supplied by the HaloDaemon installation rules and may require the
user to belong to the configured hardware-control group.

## Limitations

- Linux only.
- Requires an enabled hwmon integration plugin.
- Only attributes selected by the host are exposed.
- Only PWM and PWM-mode attributes are writable.
- Hardware not supported by a loaded kernel driver does not appear.
