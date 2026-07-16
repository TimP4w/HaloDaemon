# LPCIO Transport

Scoped Windows access to motherboard Super I/O monitoring and fan-control
hardware for plugins.

**Platform:** Windows only

## Overview

Super I/O chips provide motherboard temperatures, fan speeds, and PWM fan
control through LPC I/O. This access requires elevated hardware operations, so
HaloDaemon routes requests through its privileged Windows broker while the
daemon and plugin remain unprivileged.

A plugin must request the `lpcio` permission and declare the chip families it
supports. The transport exposes typed LPCIO operations rather than a raw broker
or unrestricted system I/O interface.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `lpcio_select_slot(slot)` | Select a candidate Super I/O configuration slot. |
| `lpcio_find_bars()` | Locate the selected chip's monitoring interface. |
| `lpcio_prepare_hwm(slot, unlock)` | Prepare the monitoring interface for access. |
| `lpcio_read_port(port)` | Read one byte from an allowed I/O port. |
| `lpcio_write_port(port, data)` | Write one byte to an allowed I/O port. |
| `lpcio_hwm_read(base, offset)` | Read one byte from the monitoring interface. |
| `lpcio_hwm_write(base, offset, data)` | Write one byte to the monitoring interface. |
| `lpcio_superio_inb(offset)` | Read one byte during Super I/O discovery. |
| `lpcio_superio_outb(offset, data)` | Write one byte during Super I/O discovery. |

Plugins use these operations to identify supported chips and expose sensors and
fan channels. Vendor-specific chip interpretation belongs in the plugin.

## Access requirements

PawnIO and HaloDaemon's LPCIO module must be installed. Only the broker is
elevated, and access is serialized so operations from different plugin calls do
not interleave on the stateful interface.

## Limitations

- Windows only.
- Requires PawnIO and the HaloDaemon broker.
- A plugin must explicitly declare the supported chip identifiers.
- The transport does not expose arbitrary kernel-driver or broker operations.
