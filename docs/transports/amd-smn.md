# AMD SMN Transport

Read-only access to the AMD System Management Network (SMN) for plugins that
provide CPU telemetry.

**Platform:** Windows only

## Overview

SMN is an internal interface used by AMD processors for telemetry and hardware
management. Access requires elevated hardware I/O, so HaloDaemon routes requests
through its privileged Windows broker while the daemon and plugin remain
unprivileged.

Plugins must request the `amd_smn` permission and declare an `amd_smn` device
match. The transport becomes available only on supported AMD systems with the
required PawnIO components installed.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `amd_smn_read(offset)` | Read one 32-bit SMN location. |

The transport is intentionally read-only. It does not expose writes, model-
specific decoding, MSR access, PCI configuration access, or a raw broker
handle. The plugin is responsible for interpreting successful reads for the
processor models it supports.

## Access requirements

PawnIO and HaloDaemon's AMD SMN module must be installed. Only the broker is
elevated; plugins invoke the scoped operation through the daemon.

If the required platform support is unavailable, matching plugins remain
inactive rather than attempting another hardware-access path.

## Limitations

- Windows only.
- Available only on supported AMD processor families.
- Read-only and limited to 32-bit SMN reads.
- Hardware access may be serialized with other privileged motherboard-bus
  operations.
