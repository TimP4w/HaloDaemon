# Serial Transport

A serial/COM byte stream for integration plugins that talk to a device over a
UART, directly or through a USB-serial adapter.

**Platform:** Linux and Windows

## Overview

The serial transport opens the port selected through plugin configuration and
applies the line settings the manifest declares (baud, data bits, parity, stop
bits, read timeout). Like TCP it is a byte stream with no framing of its own:
`read(size)` returns exactly the requested number of bytes or fails on timeout.

Plugins define their own protocol framing, usually by reading a fixed-size
header followed by the payload length that header describes.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `write(data)` | Write bytes to the port. |
| `read(size)` | Read exactly the requested number of bytes. |
| `write_then_read(data, size)` | Write bytes and then read an exact-size reply. |
| `write_many(chunks)` | Write several byte sequences in order. |
| `set_dtr(level)` | Assert or clear the DTR line. |
| `set_rts(level)` | Assert or clear the RTS line. |
| `send_break(duration_ms)` | Assert BREAK for a bounded duration, then clear it. |
| `flush_input()` | Discard buffered inbound bytes (a stale-frame reset). |

HID-specific feature-report and companion-collection operations are not
available on serial.

## Events

When the manifest enables events and the plugin declares an `event()` callback,
a reader thread on a cloned handle delivers unsolicited inbound bytes through the
shared event path, exactly like HID input reports. Use events for a device that
streams unsolicited data rather than answering request/reply.

## Connection and scope

The plugin manifest names the configuration field holding the selected port and
declares the line settings, initial DTR/RTS state, whether to flush input on
open, and whether to reconnect automatically after an I/O failure. The GUI offers
a dropdown of the host's available ports; prefer a replug-stable
`/dev/serial/by-id/...` path so the selection survives re-enumeration.

Integration plugins own their protocol-level controller discovery. On a
configured `reconnect`, an unplug/replug of a USB adapter reopens the port with
the same settings before the error surfaces.

## Limitations

- Serial provides no encryption or peer authentication.
- A single port is a single handle: mixing request/reply `read` with the event
  reader on the same port races for inbound bytes. Use one mode per device.
- Reads are exact-length stream reads, not packet reads.
- A plugin cannot open any port outside its approved configuration.
