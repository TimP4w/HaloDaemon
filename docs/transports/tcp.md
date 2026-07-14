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

The production plugin backend resolves the configured hostname once, rejects
non-routable results unless `transports.tcp.allow_private: true`, and connects
to that exact vetted `SocketAddr` through `connect_addr_blocking`. This avoids a
second DNS lookup and DNS-rebinding gap. The connect and subsequent reads/writes
use the manifest timeout (default 5 seconds; validated as `1..=60000` ms).

The underlying transport also exposes two test/convenience constructors:

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

## Reconnect and controller monitoring

The transport reports failures; the plugin integration monitor owns recovery.
It checks enabled roots every 5 seconds, reconnects integrations that were
offline at startup or dropped later, and backs persistent failures off at
5/5/10/20/30 seconds (capped at 30). A healthy pass also re-enumerates and diffs
remote controllers, adding/removing children without a full discovery scan.
Disabling an integration or changing its config performs a scoped teardown and
reconnect immediately.

## Limitations

- No TLS — plugins must implement protocol security themselves or talk to a
  trusted service. Private, loopback, and link-local targets require the
  manifest's explicit `allow_private: true` opt-in.
- The plugin declares its own host/port via manifest `config` fields (see
  [plugins.md](../plugins.md)); there's no discovery/enumeration of network
  services — the user types an address.
