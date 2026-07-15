# HID++ Protocol

Logitech's proprietary protocol layered on USB HID. Two **distinct generations** coexist over one shared wire frame:

- **[HID++ 1.0](hidpp1.md)** — register-based: read/write a 9-bit register address. Used by the Unifying/Lightspeed receiver (pairing, device list, connect/disconnect notifications) and the G560 speaker's vendor commands.
- **[HID++ 2.0](hidpp2.md)** — feature-enumerated: a device exposes a table of *features* (16-bit codes) whose runtime *indices* are discovered via the ROOT feature before any call. Used by modern mice, keyboards and headsets.

This page documents only the **shared wire substrate** both generations ride on (frame format, request/response multiplexing, the listener). The generation-specific register / feature tables live in their own pages above. The code mirrors this split: `protocols/hidpp/mod.rs` holds the shared messenger, `protocols/hidpp/v1/` the registers, `protocols/hidpp/v2/` the features.

**Credits:** reverse-engineered with reference to the [Solaar](https://github.com/pwr-Solaar/Solaar) project (GPL-2.0-or-later), files `base.py`, `hidpp10.py`, `hidpp20.py` by Daniel Pavel and contributors.

---

## Packet layout

Two report sizes, both built by `build_packet`:

| Report ID | Name | Length | Param region |
|-----------|------|--------|--------------|
| `0x10` (`HIDPP_SHORT`) | short | 7 bytes (`SHORT_LEN`) | bytes `4..7` (3 bytes) |
| `0x11` (`HIDPP_LONG`) | long | 20 bytes (`LONG_LEN`) | bytes `4..20` (16 bytes) |

Header byte layout (both generations):

```
Byte 0: Report ID      0x10 short / 0x11 long
Byte 1: Device number  0xFF = receiver itself or wired direct; 1–6 = paired/relayed device
Byte 2: Sub-ID         1.0: register-access sub-ID (0x80/0x81/0x82/0x83)
                       2.0: feature index (from ROOT lookup)
Byte 3: Address byte   1.0: register address low byte
                       2.0: function nibble (high) | software-ID nibble (low)
Byte 4..N: params, zero-padded to frame length (build_packet truncates to fit)
```

**Device-number byte.** `RECEIVER_DEVNUM = 0xFF` addresses the receiver itself for HID++ 1.0 register access. Relayed/wireless devices use `1..=6`; a directly wired device is also addressed at `0xFF` (it is its own transport root, not slot 1).

The meaning of bytes 2–3 differs per generation — see [HID++ 1.0](hidpp1.md) (sub-ID encodes direction + register bit 9) and [HID++ 2.0](hidpp2.md) (feature index + function|software-id).

---

## Transport: short/long collections

On Windows the HID++ interface splits into a short-report node (usage 1) and a long-report node (usage 2) on usage page `0xFF00`; on Linux a single hidraw node carries both with usage 0. `collection::select_hidpp_paths` resolves both paths and `HidppMessenger::start_listener` spawns a second reader task for the long handle when present. Both reader tasks route through the same `dispatch_packet`.

**Replies land on the collection matching the *reply's* report ID, not the request's.** A short (`0x10`) request whose reply is a long (`0x11`) report — the norm for HID++ 2.0 feature calls and for the `0x2B5` receiver register — arrives on the long node even though the request went out short. The host must therefore read *both* collections and match by `(devnum, sub_id)`; reading only the collection a request was written to loses the reply. The Lua plugin runtime gets the same guarantee from the merged `read_any` input queue (see [HID transport](../transports/hid.md)).

`hidpp::open_wired(vid, pid, interface, path, serial, report)` encapsulates this: it resolves the collection paths, opens a dual-handle transport (or, for composite devices that declare no short report, a single long-only handle), and returns a ready `HidppMessenger`. Both wired devices and the receiver use it; the device layer never touches collection resolution or routing.

**Force-long devices.** The LIGHTSPEED headsets expose an interface that declares *no* short report — the 7-byte short form returns `EPIPE`. `open_wired` with `DirectReport::LongOnly` builds the messenger with `with_long_requests()`, routing *every* request (ROOT included) onto a long (`0x11`) report.

---

## Request/response multiplexing

`HidppMessenger` owns the raw transport and serialises one in-flight request at a time (`request_lock`). Both the write and the wait for a reply are bounded at 2 s — a stalled control-transfer write on a headset must not park the whole sequential discovery loop forever.

- **Reply matching** — `dispatch_packet` matches a reply on `(devnum, sub_id/feature_index)`, or accepts an error sub-ID for the same devnum. Unmatched packets become `HidppNotification { devnum, sub_id, address, data }` broadcasts — **except** an orphaned HID++ 2.0 feature response (feature-index sub-ID `< 0x40` with a nonzero software-ID nibble, i.e. `address & 0x0F == HIDPP_SW_ID`), which is dropped. Genuine 2.0 events carry swid 0; a nonzero swid means the packet is a late reply whose caller already timed out, and rebroadcasting it as a notification would re-trigger the reconcile read that solicited it — a self-sustaining storm. HID++ 1.0 sub-IDs (`≥ 0x40`) put a device/register byte in byte 3, not a swid, so they always broadcast.
- **Error sub-IDs** — `0x8F` (wired) or `0xFF` (Lightspeed wireless) in byte 2 marks an error reply: `[devnum, 0x8F/0xFF, feature_idx, func_byte, err_code, …]`. The error is surfaced to the matching request.
- **Reader self-termination** — a run of `MAX_CONSECUTIVE_READ_ERRORS` (20) read errors, or an explicit "disconnected"/"poll error", stops the listener (`classify_read_error`); this is just ahead of the 2 s hotplug monitor, so a vanished device is noticed promptly.

---

## CRC-16/CCITT-FALSE

Shared by the HID++ 2.0 onboard-profile flash format (`crc16`):

| Parameter | Value |
|-----------|-------|
| Polynomial | `0x1021` |
| Initial value | `0xFFFF` |
| Input/output reflection | none |
| Final XOR | none (`0x0000`) |
| Check (`"123456789"`) | `0x29B1` |

Used by [HID++ 2.0](hidpp2.md) ONBOARD_PROFILES sector writes; documented here because `crc16` lives in the shared `mod.rs`.
