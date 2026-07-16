# Computer Device

A synthetic device representing the host operating system.

**Platform:** Linux and Windows

## Overview

The computer device groups features supplied by the operating system rather
than by a physical peripheral. It does not move bytes over a hardware transport,
has no USB identity, and requires no plugin permission.

HaloDaemon currently exposes host metrics, power-profile selection, and a
keep-awake control through this built-in device. Individual features appear
only when the operating system provides the required service.

## Operations for plugins

None. The computer device is built into HaloDaemon and is not a transport
available to Lua plugins. Plugins that need a host service should use an
integration plugin and one of the explicitly supported scoped APIs.

## Available capabilities

| Capability | Purpose |
|---|---|
| Host metrics | Report CPU load, memory use, CPU frequency, and uptime. |
| Power profile | Select a performance, balanced, or power-saving host profile. |
| Keep awake | Prevent idle sleep while enabled. |

## Limitations

- Availability varies with the operating system and installed host services.
- It cannot be extended with vendor-specific hardware behavior.
- It exposes no raw operating-system, command, filesystem, or hardware access
  to plugins.
