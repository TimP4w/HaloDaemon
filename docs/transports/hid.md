# HID Transport

USB Human Interface Device (HID) access for plugins that communicate with
peripherals using HID reports.

**Platform:** Linux and Windows

## Overview

Many keyboards, mice, headsets, coolers, and RGB controllers use vendor-defined
HID reports. HaloDaemon discovers devices from the matches declared in a plugin
manifest and gives the plugin a scoped byte-stream interface to the matched
device.

The host handles device opening, report sizing, timeouts, hotplug, and input
delivery. It does not interpret vendor protocols; framing and reply matching
remain the plugin's responsibility.

Plugins may also declare a companion HID collection when a device splits input
and output across multiple collections.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `write(data)` | Write one report to the primary collection. |
| `read(size)` | Wait for and return an input report. |
| `read_nonblocking(size)` | Read an available report without waiting. |
| `write_then_read(data, size)` | Write a report and wait for a reply. |
| `write_many(packets)` | Write several reports in order. |
| `feature_exchange(data, size)` | Perform a HID feature-report exchange. |
| `read_any(size)` | Read from either the primary or companion collection. |
| `defer_event(data)` | Return an unrelated report to the plugin event path. |
| `has_companion()` | Check whether the declared companion collection is open. |

Companion variants of the read, write, write-then-read, and batch-write
operations are available when the manifest declares a companion collection.
Unsolicited input is delivered to the plugin's event callback.

## Discovery and scope

A device plugin declares HID vendor and product identifiers and may further
restrict matches by interface, usage page, or usage. HaloDaemon exposes only
the matched device and any explicitly declared companion collection.

Report size, timeout, companion selection, and optional write-rate limits are
also controlled by the manifest.

## Access requirements

Linux installations need an appropriate udev rule so the active user can open
the device. Windows normally requires no additional device permissions.

## Limitations

- Plugins receive raw reports and must implement vendor-specific framing.
- Companion operations are unavailable unless the collection was declared and
  successfully opened.
- HID permissions do not grant access to arbitrary USB interfaces or control
  transfers; those use the USB transport.
