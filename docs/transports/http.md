# HTTP Transport

A scoped HTTP/HTTPS client for integration plugins that talk to a local device
API or a web service, such as a Philips Hue bridge or a weather provider.

**Platform:** Linux and Windows

## Overview

Unlike the stream transports, HTTP is not exposed as `dev.transport` userdata.
A plugin that declares an `http` transport and holds the `network` permission
gets the `halod.http:request{…}` capability global. Each call is a synchronous,
bounded request/response validated against the manifest before a socket opens;
there is no persistent connection the plugin controls.

## Operations for plugins

| Operation | Purpose |
|---|---|
| `halod.http:request{ method, origin, path, headers, body, timeout_ms }` | Perform one bounded request against a declared origin. |

The result table carries `status`, lowercased `headers`, the raw `body` string,
and a parsed `json` field when the body is JSON. Requests that exceed the
declared method set, size limits, timeout, or origin scope raise an error.

## Discovery and scope

The manifest declares an exact `scheme://host[:port]` origin allowlist, no
wildcards. A device whose address isn't known at authoring time uses the literal
`{host}` token, resolved from a typed `host_key` config field and combined with
the typed port named by `port_key`. Redirects are not followed, and
loopback, private, and link-local origins are rejected unless the manifest
explicitly opts in.

TLS trust is either the public web PKI (`default`) or a plugin-shipped DER root
(`custom-ca`) pinned to a config-provided certificate identity. Plugins cannot
disable certificate verification.

## Limitations

- Only request/response is exposed: no streaming, websockets, or server push.
- The plugin cannot reach any origin outside its approved allowlist.
- Service discovery via mDNS/SSDP is a separate integration-setup mechanism, not
  part of this transport.

See the plugin repository's manifest reference (`transports.http`) and Lua API
(`halod.http:request`) for the full authoring contract.
