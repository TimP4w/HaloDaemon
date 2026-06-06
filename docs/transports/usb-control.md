# USB Control Transport

USB vendor control transfer transport via `rusb` (libusb bindings).

**Source:** `src/daemon/src/drivers/transports/usb_control.rs`

**Platform:** Linux, Windows

---

## Overview

USB control transfers are the standard request/response mechanism defined by the USB specification. They use endpoint zero and carry a structured 8-byte setup packet followed by an optional data stage. 

---

## Implementation

Opening a device:
1. Create a `rusb::Context`.
2. Open the device via `open_device_with_vid_pid(vid, pid)`.
3. On Linux, detach the kernel driver from the target interface if one is active.
4. Claim the interface.

The `write_control` method issues a single control transfer:

| Field | Meaning |
|-------|---------|
| `bm_request_type` | Direction, type, and recipient bitmap |
| `b_request` | Request code |
| `w_value` | Request-specific value |
| `w_index` | Request-specific index (often the interface number) |
| `data` | Optional data payload |


---
