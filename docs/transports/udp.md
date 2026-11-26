# UDP Transport

A scoped connected-UDP client for integration plugins that talk to a LAN device
over datagrams, such as a Nanoleaf panel or a WLED controller.

**Platform:** Linux and Windows

## Overview

Like HTTP, UDP is not exposed as `dev.transport` userdata. A plugin that
declares a `udp` transport gets the
`halod.udp` capability global. The socket is *connected* to the single
destination named in plugin configuration and bound to an ephemeral local port,
so the plugin has no free-roaming socket: every datagram goes to and comes from
that one peer.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `halod.udp:send{ bytes }` | Send one datagram to the configured destination. Returns the number of bytes sent. |
| `halod.udp:receive{ timeout_ms }` | Wait for one inbound datagram, returning its bytes or `nil` on timeout. |

A datagram larger than the declared `max_datagram_bytes` is rejected before it
reaches the socket. `send` and `receive` each fail once their configured
timeout elapses.

## Connection and scope

The manifest names the config fields holding the destination host (`host_key`)
and port (`port_key`), and declares `max_datagram_bytes`, `send_timeout_ms`, and
`recv_timeout_ms`. The destination is resolved once and vetted by the shared
network guard: loopback, private, link-local, CGNAT, multicast, and
metadata addresses are rejected unless the manifest sets `allow_private: true`
for a LAN device.

Because the socket is connected, the plugin cannot send to or receive from any
peer other than the vetted destination.

## Datagram TLS

A manifest may add a `dtls` block to wrap the socket in a DTLS 1.2 pre-shared-key
session. The plugin still calls `halod.udp:send` and `halod.udp:receive` with
plaintext datagrams; the host encrypts and decrypts them.

- The identity and key are read from the config fields the manifest names. The
  key field must be `secure: true` and is decoded as `hex`, `base64`, or `raw`.
  Without both values the plugin has no `halod.udp`.
- The manifest lists the cipher suites it accepts, drawn from the suites the
  host supports.
- The handshake runs on the first `send`, bounded by `send_timeout_ms`, so a
  plugin can prepare the peer over another transport first. `receive` returns
  `nil` until a session exists.
- A failed send or receive ends the session; the next `send` handshakes again.
- With `session_idle_ms`, a `send` after a longer gap opens a new session from a
  new local port instead of reusing one the peer may have dropped.

## Limitations

- Unicast to a single connected destination only, no broadcast or multicast.
- UDP provides no delivery guarantee or ordering; the plugin's protocol owns any
  reliability it needs. Encryption and peer authentication exist only with a
  `dtls` block.
- Service discovery via mDNS, SSDP or a TCP probe is a separate integration-setup
  mechanism, not part of this transport.

See the plugin repository's manifest reference (`transports.udp`) and Lua API
(`halod.udp`) for the full authoring contract.
