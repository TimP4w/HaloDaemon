# USB Transport

Scoped USB endpoint and control-transfer access for device plugins.

**Platform:** Linux and Windows

## Overview

Some devices use vendor-specific USB interfaces instead of HID. A plugin
declares the devices, interfaces, endpoints, transfer types, size limits, and
timeouts it needs. HaloDaemon opens and claims only those resources.

The primary USB device comes from the plugin's hardware match. A manifest may
also declare companion USB devices when one product uses multiple USB
interfaces or identities.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `usb_write(endpoint, data, timeout, device_id)` | Write to an allowed bulk or interrupt endpoint. |
| `usb_read(endpoint, size, timeout, device_id)` | Read from an allowed bulk or interrupt endpoint. |
| `usb_control(...)` | Perform an allowed control transfer on endpoint zero. |

`device_id` selects a manifest-declared device and defaults to the primary
device. Endpoint reads may return fewer bytes than requested. Writes either
send the complete payload or fail.

Control transfers are disabled unless the selected device has an explicit
control-transfer declaration. Direction, payload size, and timeout must fit the
manifest scope.

## Discovery and scope

USB devices are matched by vendor and product identity and kept distinct by
their physical location and serial information. Plugins can access only the
interfaces, endpoints, companion devices, transfer sizes, and timeout ceilings
declared in the manifest.

## Access requirements

Linux installations need an appropriate udev rule for user access. HaloDaemon
may temporarily detach a kernel driver when claiming an explicitly declared
interface and restores normal ownership when the transport closes.

## Limitations

- Only bulk, interrupt, and control transfers are exposed.
- Every endpoint and companion device must be declared in advance.
- Control transfers require an explicit manifest allowlist.
- USB access is separate from HID access, even when both belong to the same
  physical product.
