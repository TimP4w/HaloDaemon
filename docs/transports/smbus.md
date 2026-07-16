# SMBus Transport

System Management Bus (SMBus/I2C) access for plugins that support motherboard,
DRAM, and GPU controllers.

**Platform:** Linux and Windows

## Overview

SMBus is a two-wire bus commonly used for PC hardware management. HaloDaemon
discovers chipset and GPU buses and gives plugins scoped access to only the bus
and addresses declared in their manifests.

Operations are grouped into a batch so a plugin can complete a related sequence
while holding one bus lock. This prevents transactions from different devices
from interleaving.

## Bus discovery and scope

Plugins declare whether a device is expected on a chipset or GPU bus, the
addresses they may access, and how discovery may probe them. A plugin cannot
access an address outside that allowlist.

GPU buses may also carry display-management traffic, so plugins must restrict
GPU discovery to explicitly supported PCI devices. Unknown GPU buses are not
opened or probed. Chipset buses do not require that PCI allowlist unless a
plugin chooses to provide one.

## Operations for plugins

SMBus operations are available only inside `dev.transport:batch(function(ops)
... end)`.

| Operation | Purpose |
|---|---|
| `read_byte(address)` | Read a single byte. |
| `read_byte_data(address, command)` | Read one byte using an SMBus command. |
| `write_quick(address)` | Perform an acknowledgement probe. |
| `write_byte_data(address, command, data)` | Write one byte using an SMBus command. |
| `write_word_data(address, command, data)` | Write one word using an SMBus command. |
| `write_block_data(address, command, data)` | Write a short data block using an SMBus command. |
| `supports_block_write()` | Check whether the current bus supports block writes. |

Failed reads return no data and failed writes report failure. The scoped
operation object is valid only for the duration of the batch.

## Access requirements

**Linux:** the user needs access to the system's I2C devices, normally through
the `i2c` group and the HaloDaemon udev rules.

**Windows:** chipset SMBus access requires PawnIO. The non-elevated daemon sends
scoped operations to the privileged HaloDaemon broker. GPU-bus availability
depends on the installed graphics driver and supported platform interface.

## Security and limitations

- Bus kind, address range, probe behavior, and optional PCI matches are fixed by
  the plugin manifest.
- GPU buses without an explicit supported-device match are not probed.
- The daemon and plugin remain unprivileged on Windows; only the broker performs
  privileged chipset access.
- Not every platform backend supports every optional SMBus operation. Plugins
  can query block-write support before using it.
