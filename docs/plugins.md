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

Supported nested matches are `hid`, `usb`, `smbus`, `command`,
`amd_smn`, and `lpcio`. Concrete identifiers must be unique; generic support is always an
explicit `any: true` declaration. Unsupported-platform packages remain visible
but inert and never suppress a built-in host device.

## Authority and activation

Plugins are disabled until explicitly enabled. Every enable action presents the
normalized authority: permissions, transport scopes, and command executable
names. The GUI submits that exact snapshot and the daemon compares it to the
current catalog before enabling atomically.

Disabling is immediate. Each later enable requires the modal again. The daemon
also checks the complete accepted authority before every runtime activation, so
a package whose permissions or transport scopes expand remains inert until the
new authority is accepted. A content hash is package integrity metadata, not
consent. Integration activation is a separate explicit switch.

## Repositories

Repositories retain Git objects separately from immutable materialized
revisions. Update validates every package and its index before switching the
active revision, so a running package is never changed in place. Repository
pages show provenance, validation status, and package diffs; update and repair
are repository-wide actions.

Official packages have a verified detached repository-index signature. Every
indexed repository, including a third-party or development repository, must
match each package SHA-256 declared by `repository.yaml`; a mismatch makes the
repository inert. Development repositories are loaded directly via
`--dev-plugin-repo` in daemon builds compiled with the
non-default `dev-plugin-repo` Cargo feature; production builds omit the flag and
its runtime state. Imported standalone packages are local unsigned packages.
Invalid official content stays visible for repair but never loads or shadows
native discovery.

The digest check gives unsigned repositories tamper evidence and prevents an
indexed package from silently differing from its own repository manifest. It
does not authenticate a third-party publisher: the repository owner or an
attacker able to replace both the package and `repository.yaml` can publish a
new matching digest. Halo currently pins signing keys only for the official
repository; third-party repositories do not use trust-on-first-use key pinning.

Run a development build with
`cargo run -p halod --features dev-plugin-repo -- --dev-plugin-repo <DIR>`.

`--dev-plugin-repo` is a process-local official-source replacement, not an
additional checkout. Halo canonicalizes and displays its package paths, keeps
third-party repositories available, and never fetches, checks, updates, or
repairs the managed official repository for that daemon process. Restart
without the flag to return to the installed official revision.

An invalid development repository does not prevent the daemon from starting.
Packages with malformed manifests or mismatched index digests are listed as
disabled with a load error and are never executed; fixing the repository and
reloading plugins makes them eligible again.

## Runtime and containment

### Lua callback contract (plugin API 1)

`repository.yaml` must declare `compatibility.plugin_api: 1`. The matching
machine-readable callback and table-shape catalog lives in
`drivers/plugins/contract.rs`; repository validation and that catalog use the
same `PLUGIN_API` constant, so changing the ABI requires changing its version.

Every device callback receives `dev` first. Its stable fields are `transport`
and `match`; `status`, `zones`, and `audio` are present only on the paths that
provide them. `dev.match` contains `transport` and the applicable `bus`, `addr`,
`vid`, `pid`, `index`, `key`, and `name` fields plus declared transport extras.

The lifecycle callbacks are `initialize(dev)`, `close(dev)`,
`close_child(dev)`, and SMBus `pre_scan(dev)`. Host polling invokes
`read_status(dev)`; there is no Lua callback named `poll`. HID input invokes
`event(dev, event)` and may first invoke `event_source(event)`. Integration
roots use `enumerate_controllers(dev)`. Capability callbacks and effect
patterns (`render_<id>` and `led_colors_<id>`) are listed with their exact
argument and return shapes in the in-code catalog. Structured callback results
are decoded using the Rust serde types named there; unknown or malformed fields
therefore follow those types' serde rules.

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
built-in host device is the computer special case. Linux hwmon behavior is
supplied by the official integration package; the daemon retains only its
scoped sysfs transport and discovery boundary.

## Validation

Validate a package and its optional hardware-free fixture:

```powershell
cargo run -p halod --features plugin-test -- plugin-test ..\path\to\package
```

Before submitting package changes, validate all catalogs without Lua execution,
refresh the deterministic package digest in `repository.yaml`, and run the
daemon workspace formatter, clippy, and relevant tests.

`halod-plugin-signing` is the canonical implementation used by both the daemon
and release automation. `validate <repo>` verifies all indexed SHA-256 values,
`index <repo> --check` rejects a stale generated index, and
`index <repo> --version <version>` discovers top-level packages and atomically
rewrites their sorted hashes. `sign` signs the exact index bytes using the
base64 Ed25519 seed named by `--key-env`; `verify` checks the signature,
compatibility, manifests, and package contents.
