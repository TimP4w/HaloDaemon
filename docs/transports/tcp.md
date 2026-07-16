# TCP Transport

A TCP byte stream for integration plugins that communicate with a network
service, such as an OpenRGB SDK server.

**Platform:** Linux and Windows

## Overview

The TCP transport connects to a host and port supplied through plugin
configuration. It provides the same basic stream operations as HID, but TCP has
no report boundaries: reads return the exact requested number of bytes or fail
on timeout or disconnect.

Plugins define their own protocol framing, usually by reading a fixed-size
header followed by the payload length described by that header.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `write(data)` | Write bytes to the connection. |
| `read(size)` | Read exactly the requested number of bytes. |
| `read_nonblocking(size)` | Read available bytes without waiting. |
| `write_then_read(data, size)` | Write bytes and then read an exact-size reply. |
| `write_many(chunks)` | Write several byte sequences in order. |

HID-specific feature-report, companion-collection, and event-deferral operations
are not available on TCP.

## Connection and scope

The plugin manifest identifies the configuration fields containing the host and
port and sets a connection timeout. Private, loopback, and link-local addresses
are rejected unless the manifest explicitly allows private targets.

Integration plugins own their protocol-level controller discovery. HaloDaemon
monitors the configured connection and retries after startup or connection
failure. Changing the integration configuration reconnects it with the new
scope.

## Limitations

- TCP provides no encryption or peer authentication. Use it only with a trusted
  service or a protocol that supplies its own security.
- Service discovery is not provided; the user configures the address.
- A plugin cannot connect to arbitrary targets outside its approved
  configuration.
- Reads are exact-length stream reads, not packet reads.
