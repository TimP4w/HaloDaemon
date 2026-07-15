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

Supported nested matches are `hid`, `usb`, `smbus`, `hwmon`, `command`,
`amd_smn`, and `lpcio`. Concrete identifiers must be unique; generic support is always an
explicit `any: true` declaration. Unsupported-platform packages remain visible
but inert and never suppress a built-in host device.

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
loaded directly via `--dev-plugin-repo` in daemon builds compiled with the
non-default `dev-plugin-repo` Cargo feature; production builds omit the flag and
its runtime state. Imported standalone packages are local unsigned packages.
Invalid official content stays visible for repair but never loads or shadows
native discovery.

Run a development build with
`cargo run -p halod --features dev-plugin-repo -- --dev-plugin-repo <DIR>`.

`--dev-plugin-repo` is a process-local official-source replacement, not an
additional checkout. Halo canonicalizes and displays its package paths, keeps
third-party repositories available, and never fetches, checks, updates, or
repairs the managed official repository for that daemon process. Restart
without the flag to return to the installed official revision.

## Runtime and containment

`discover(host)` creates physical roots. Each root owns one serialized Lua
worker and its transport handles. `initialize`, capability calls, `children`,
`on_event`, and `close` run through that worker; receiver children use opaque
keys and route through their root rather than creating a multiplexer.

Device packages opt into this with `dynamic_children: true` and an
`enumerate_controllers()` callback. Each returned record supplies a unique,
stable `id`; `key` is opaque and is injected into every routed child table as
`dev.match.key`. This is used for one NVIDIA child per UUID and one Logitech
child per receiver slot. Children share the root transport but do not recurse
into further dynamic discovery.

`initialize` supplies device-specific RGB zones/effects, controls, DPI bounds
and steps, LCD policy, and fan-channel identity. Those values are never static
package catalog fields.

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

## Built-in boundary

Device-vendor implementations are plugin-owned. AMD SMN, Nuvoton LPCIO,
NVIDIA SMI, and Logitech HID++ are supplied by the official plugin repository;
the daemon retains only transport brokers and discovery roots for them. The only
built-in host devices are the Linux hwmon path and the computer special case.

## Validation

Validate a package and its optional hardware-free fixture:

```powershell
cargo run -p halod --features plugin-test -- plugin-test ..\path\to\package
```

Before submitting package changes, validate all catalogs without Lua execution,
refresh the deterministic package digest in `repository.yaml`, and run the
daemon workspace formatter, clippy, and relevant tests.
