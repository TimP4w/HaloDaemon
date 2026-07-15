# HID Transport

USB Human Interface Device (HID) transport via the `hidapi` library. Used by the majority of supported devices.

**Platform:** Linux, Windows

---

## Overview

USB HID is reused by peripheral manufacturers to carry vendor-specific binary protocols without requiring a custom kernel driver. On Linux the kernel exposes each HID interface as a `/dev/hidraw*` character device; userspace opens it via `hidapi` and reads/writes raw bytes.

---

## Host readers and bounded input

The device is opened twice — once for reading, once for writing — producing two independent file descriptors on the same `hidraw` node. A continuous host reader owns each input descriptor and feeds a bounded 256-report queue. This prevents input waits from serialising writes and prevents a slow Lua callback from growing memory without bound.

Request/response reads consume the queue from the owning serialized Lua worker. Unsolicited reports wake that same worker, which drains them in arrival order through `on_event`. Lua never polls the HID descriptor.

Plugins may also declare a companion top-level collection by usage page and
usage. The host only opens and exposes that collection; Lua protocol code
explicitly chooses primary or companion operations. The HID transport does not
route or interpret vendor report IDs.

### Merged input queue and dispatch (`read_any` / `defer_event`)

Both collections' reader threads feed one host-owned queue, so a reply can be
matched wherever its report ID lands it (a short request whose reply is a long
report arrives on the companion collection — see
[HID++](../protocols/hidpp.md)). Two endpoint-agnostic primitives let a Lua
protocol implement request/response multiplexing without guessing a collection:

- `read_any(size)` — pop the next inbound report from the merged queue,
  regardless of which collection delivered it.
- `defer_event(bytes)` — hand a report that was read but does not belong to the
  in-flight request back to the event path (`on_event`), preserving arrival
  order, instead of dropping it.

The Lua protocol owns all dispatch semantics — what counts as a reply, an error,
or an unsolicited notification. It writes once (routing the frame to the
collection that accepts its report ID), then loops `read_any`, returning the
matching reply and `defer_event`-ing everything else. This mirrors the native
messenger's `dispatch_packet` and keeps the daemon free of any vendor-specific
knowledge, so the same primitives serve any plugin's multiplexing scheme.

---

## Report size modes

| Mode | Behavior |
|------|----------|
| `None` (raw passthrough) | Data written exactly as provided; no byte prepended, no padding. Used by HID++ and ASUS Aura USB. |
| `Some(N)` (fixed-size) | Linux: prepend `0x00`, pad up to `N` bytes total (`N+1` bytes written); longer payloads are written as-is (no truncation). Windows: pad up to `N+1` bytes; longer payloads written as-is. |

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
