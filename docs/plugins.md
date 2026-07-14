# Plugins

Plugins extend HaloDaemon without compiling device support into the daemon. A
package is an inert `plugin.yaml` catalog plus a Lua entry script. HaloDaemon
parses the catalog, repository index, and assets without executing Lua; Lua
only starts after the user enables the package.

The companion repository contains the authoritative package examples and the
[manifest reference](../../HaloDaemon-plugins/docs/manifest-reference.md).

## Package contract

```text
my_plugin/
├── plugin.yaml
├── main.lua
└── assets/
```

`id` must equal the directory name. The catalog declares identity, platforms,
permissions, capability vocabulary, hardware `match` entries, scoped
transports, configuration, secrets, and effect metadata. Repository
compatibility and package digests belong to the repository-root
`repository.yaml`.

There is no package `compatibility` section and no legacy flat device fields.
Static runtime capability sections, polling declarations, command templates,
and argument schemas are removed. Lua reports per-device descriptors and values
from `initialize` instead.

```yaml
id: example_ring
name: Example Ring
version: 1.0.0
platforms: [linux, windows]
permissions: [hid]
capabilities: [rgb]
devices:
  - vendor: Example
    model: Ring 12
    type: led_strip
    match:
      hid: { vid: 0x1234, pid: 0x5678 }
transports:
  hid: { report_size: 64, timeout_ms: 1000 }
```

Supported nested matches are `hid`, `smbus`, `hwmon`, `command`, `amd_smn`,
and `lpcio`. Concrete identifiers must be unique; generic support is always an
explicit `any: true` declaration. Unsupported-platform packages remain visible
but inert and never suppress a native driver.

## Authority and activation

Plugins are disabled until explicitly enabled. Every enable action presents the
normalized authority: permissions, transport scopes, and command executable
names. The GUI submits that exact snapshot and the daemon compares it to the
current catalog before enabling atomically.

Disabling is immediate. Each later enable requires the modal again. The daemon
stores the accepted authority only to decide whether a repository update has
expanded scope; it does not use a content hash as consent. Integration
activation is a separate explicit switch.

## Repositories

Repositories retain Git objects separately from immutable materialized
revisions. Update validates every package and its index before switching the
active revision, so a running package is never changed in place. Repository
pages show provenance, validation status, and package diffs; update and repair
are repository-wide actions.

Official packages have a verified detached repository-index signature.
Third-party repositories are unsigned but usable. Development repositories are
loaded directly via `--dev-plugin-repo`; imported standalone packages are local
unsigned packages. Invalid official content stays visible for repair but never
loads or shadows native discovery.

## Runtime and containment

`discover(host)` creates physical roots. Each root owns one serialized Lua
worker and its transport handles. `initialize`, capability calls, `children`,
`on_event`, and `close` run through that worker; receiver children use opaque
keys and route through their root rather than creating a multiplexer.

Transports are scoped by catalog declaration:

- HID uses matched endpoints and bounded event queues.
- SMBus is address-scoped and metered.
- Commands use direct process spawning only for declared executable names,
  with bounded argv, timeout, and output.
- hwmon, AMD SMN, and LPCIO expose typed, limited operations rather than raw
  filesystem or broker access.

The daemon applies memory, instruction, wall-clock, queue, payload, allocation,
and hardware-write protections. Health is tracked per plugin and optionally per
device; an equivalent failure notifies once until a successful operation clears
the episode.

## Validation

Validate a package and its optional hardware-free fixture:

```powershell
cargo run -p halod --features plugin-test -- plugin-test ..\path\to\package
```

Before submitting package changes, validate all catalogs without Lua execution,
refresh the deterministic package digest in `repository.yaml`, and run the
daemon workspace formatter, clippy, and relevant tests.
