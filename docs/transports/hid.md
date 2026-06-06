# HID Transport

USB Human Interface Device (HID) transport via the `hidapi` library. Used by the majority of supported devices.

**Source:** `src/daemon/src/drivers/transports/hid.rs`

**Platform:** Linux, Windows

---

## Overview

USB HID is reused by peripheral manufacturers to carry vendor-specific binary protocols without requiring a custom kernel driver. On Linux the kernel exposes each HID interface as a `/dev/hidraw*` character device; userspace opens it via `hidapi` and reads/writes raw bytes.

---

## Dual file descriptors

The device is opened twice — once for reading, once for writing — producing two independent file descriptors on the same `hidraw` node. This prevents the read side's blocking `read_timeout` from serialising writes, which would cap per-key RGB frame rates to the read timeout period.

Both fds are opened in non-blocking mode. All I/O is dispatched via `tokio::task::spawn_blocking`.

---

## Report size modes

| Mode | Behavior |
|------|----------|
| `None` (raw passthrough) | Data written exactly as provided; no byte prepended, no padding. Used by HID++ and ASUS Aura USB. |
| `Some(N)` (fixed-size) | Linux: prepend `0x00`, pad/truncate to `N` bytes total (`N+1` bytes written). Windows: pad/truncate to `N+1` bytes. |

---

## Device discovery

`HidTransport::discover()` enumerates connected HID devices and matches them against registered `HidDeviceDescriptor` objects (registered at compile time via the `inventory` crate). Each descriptor declares VID/PID(s), an optional interface filter, an optional `preferred_collection`, and a factory closure.

A single physical device may appear as multiple enumeration entries (one per HID interface, and on Windows one per top-level collection). `pick_hid_devices` resolves this to exactly one entry per physical unit by matching the `preferred_collection` if declared, or taking the first descriptor-matched entry otherwise.

---

## Hotplug

After startup, a background task re-enumerates HID devices every 2 seconds. New entries trigger `add_hid_device`; missing entries trigger removal and `close()`.

### Wireless ↔ wired sibling lifecycle

Some devices can connect both wirelessly (via Lightspeed receiver) and wired (direct USB). Both instances coexist in the device list under different IDs but share the same `hardware_serial()`. When the wired device appears, the wireless instance is marked offline; when it is removed, the wireless instance comes back online. The wireless device is never removed from the device list, only its online state changes.

---

## Limitations

- udev rules are required on Linux; without them `/dev/hidraw*` nodes are root-owned (see `udev/60-halod.rules`).
- `hidrawN` node numbers are dynamic; the serial-based deduplication handles this transparently.
