# TCP Transport

A plain TCP stream transport for plugins that talk to a network service (e.g.
an OpenRGB SDK server) instead of a hardware bus.

**Platform:** Linux, Windows

---

## Overview

Implemented in [`src/daemon/src/drivers/transports/tcp.rs`](../../src/daemon/src/drivers/transports/tcp.rs)
as `TcpTransport`, backed by `tokio::net::TcpStream` and metered through the
same `Metered<T>` write-rate gate every other transport uses. It's the only
transport a [config-instantiated integration plugin](../plugins.md) can use,
via the `tcp` `PluginTransportDescriptor`
([`drivers/plugins/backends/tcp.rs`](../../src/daemon/src/drivers/plugins/backends/tcp.rs)).

## Read-exact semantics

HID's `Transport::read(size)` is report-based: a device sends whatever it
sends, and `size` is advisory. A raw TCP byte stream has no such framing, so
`TcpTransport::read(size)` instead **always returns exactly `size` bytes, or
errors** (on timeout or a closed connection) — it is `read_exact`, never a
short read. Every consumer built on this transport (e.g. the OpenRGB protocol
client) reads a fixed-size header, then exactly `data_length` bytes of
payload, and relies on this guarantee.

## Connecting

Two constructors, both bounded by a caller-supplied timeout (default 5s if
`0`):

- `TcpTransport::connect(host, port, timeout_ms)` — async, for callers already
  on an async task.
- `TcpTransport::connect_blocking(host, port, timeout_ms)` — uses
  `std::net::TcpStream::connect_timeout` then hands the socket to
  `tokio::net::TcpStream::from_std` to register it with the *current* async
  runtime. This exists because `PluginTransportDescriptor::open` is a plain
  synchronous `fn` pointer (shared with the `hid`/`smbus` backends) — the `tcp`
  backend's `open` calls this rather than requiring an async signature just
  for itself. **Callers on a shared async runtime should run this inside
  `tokio::task::spawn_blocking`** (a real network connect can block for the
  full timeout), which is exactly what the integration-plugin discovery
  scanner ([`drivers/plugins/integration_scan.rs`](../../src/daemon/src/drivers/plugins/integration_scan.rs))
  does, so a slow/unreachable server only stalls that one scanner pass.

## No hotplug / reconnect

Unlike HID, there is no background monitor watching for a dropped TCP
connection. If the remote server restarts or becomes unreachable, in-flight
reads/writes fail and are logged; reconnecting requires the user to re-run
discovery (or disable/re-enable the plugin). Automatic reconnect-with-backoff
is a deliberate non-goal for now, not an oversight.

## Limitations

- No TLS — plugins using this transport are expected to talk to a local or
  trusted-network service (e.g. OpenRGB's SDK server has no auth or
  encryption of its own either).
- The plugin declares its own host/port via manifest `config` fields (see
  [plugins.md](../plugins.md)); there's no discovery/enumeration of network
  services — the user types an address.
