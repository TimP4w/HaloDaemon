# Architecture

This document is the map of how HaloDaemon is put together: the layers, how they
connect, and where a new feature or device plugs in. Read it before adding a
device, a command, or an engine. Companion docs go deeper on specific areas:
[engines.md](engines.md) (engine internals), [development.md](development.md)
(toolchain, onboarding walkthrough, udev rules), and the per-protocol/transport
pages under [protocols/](protocols/) and [transports/](transports/).

## The five crates

The workspace (`src/Cargo.toml`) has five members:

| Crate | Directory | Role |
|-------|-----------|------|
| `halod-shared` | [src/shared/](../src/shared/) | Shared types **and logic** other crates depend on: IPC commands, the `DeviceCapability` enum, and frame/zone definitions, plus real logic — curve evaluation, frame codecs, geometry, zone transforms, and LCD/effect helpers. |
| `halod-hwaccess` | [src/hwaccess/](../src/hwaccess/) | Low-level hardware-access primitives shared by the daemon and the broker. |
| `halod-broker` | [src/broker/](../src/broker/) | **Windows-only** privileged helper: runs as LocalSystem and performs the hardware access the per-user daemon can't. |
| `halod` | [src/daemon/](../src/daemon/) | The **daemon**: device I/O, discovery, engine loops, config persistence, IPC server. Runs **per-user** (not elevated); on Windows privileged hardware access is delegated to the `halod-broker` process. |
| `halod-gui` | [src/ui/](../src/ui/) | **GUI** (eframe/egui on a glow/OpenGL renderer) with a system tray. Talks to the daemon over IPC; holds no device logic. |

The cardinal rule (`CLAUDE.md` → *Keep the layers separate*): protocol (wire
encode/decode) ↔ device (`Device` trait + capabilities) ↔ transport (byte
movement). Never leak transport bytes into a device, vendor wire formats into a
usecase, or device logic into the GUI.

## Driver layering: vendor → device → protocol → transport

Everything that touches hardware lives under
[daemon/src/drivers/](../src/daemon/src/drivers/), organized in four stacked
concerns. A single physical device is the composition of all four.

```
vendor/   organizational namespace (nzxt, corsair, logitech, asus, …)
  device      implements Device, declares capabilities()        ← what the rest of the daemon sees
    protocol    encodes/decodes the vendor wire format           ← "how do I phrase a 'set color' for this chip"
      transport   moves raw bytes (HID, SMBus, USB, …)           ← "how do those bytes reach the wire"
```

### Transport — moving bytes

A transport implements the `Transport` trait
([transports/mod.rs](../src/daemon/src/drivers/transports/mod.rs)): at its core
just `write(&[u8])` and `read(size)`, with default-implemented conveniences
(`write_then_read`, `write_many`) that transports may override with
hardware-backed versions. HID feature reports, companion collections, nonblocking
reads, and event delivery live on the `HidTransport` sub-trait and are reached
through an explicit capability downcast. Available transports: `hid`, `smbus`,
endpoint-oriented `usb`, `hwmon` (Linux), `lpcio`/`pawnio` (Windows SuperIO),
and `mock` (for tests). Hwmon and LPCIO expose narrow typed operations rather
than raw paths or broker handles; transports still know nothing about colors,
fans, or vendors.

For that same reason, a per-device write-rate ceiling is enforced at this
boundary rather than in a usecase or engine: `HidTransport` (and `SmBusDevice`
for SMBus) gate every write through a `WriteRateLimiter`
([drivers/rate_limit.rs](../src/daemon/src/drivers/rate_limit.rs)), costed in
bytes/sec, delaying (never dropping) writes that exceed the limit. The
ceiling is opt-in, not a default: `Device::write_rate_limit()` returns `None`
unless a device explicitly declares one, following the same pattern as
`debug_transport()` — a device overrides the default only if it needs
something different, and today none do, so nothing is throttled. Live
throughput is still measured and surfaced to the GUI regardless of whether a
limit is set. Chain accessories (e.g. an NZXT F-series fan on a Kraken) don't
hold their own transport at all, so their writes are already covered by
their parent hub's limiter for free — the GUI resolves this by walking the
hub's chain links (`ui/src/domain/models/device.rs::find_hub_write_rate`) rather than
requiring the daemon to duplicate the number per accessory. SMBus devices
sharing one `SmBusScanEntry` (e.g. multiple DRAM sticks) share one bus-level
limiter, since they already serialize through the same bus mutex; a
different vendor's `SmBusScanEntry` on the same *physical* bus number still
opens an independent `SmBusDevice`/limiter today—cross-vendor unification on
one physical bus is a known, unaddressed gap. USB-control endpoints are also
metered and receive the plugin/device write-rate limit.

### Protocol — speaking the vendor's wire format

A protocol module sits on top of a transport and turns intent ("set zone 2 to
red", "read fan RPM") into the exact byte sequences the chip expects, and parses
replies back. Protocols hold a transport (often `Mutex<Option<T>>` so it can be
opened/closed) and expose typed methods. See the
[Logitech HID++ plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech)
for an example. When you port a wire format from third-party code, add the
SPDX attribution header (`CLAUDE.md` → *Licensing & attribution*) and document the
format beside its package in the [official plugin repository](https://github.com/TimP4w/HaloDaemon-plugins).

### Device — the unit the daemon understands

A device implements the `Device` trait
([drivers/mod.rs](../src/daemon/src/drivers/mod.rs)). It owns one or more
protocol instances and answers two central questions:

- **Identity & lifecycle** — `id()` (stable across runs, unique per physical
  device), `name()`/`vendor()`/`model()`, `initialize()` (open connections,
  returns whether connected) and `close()`.
- **`capabilities() -> Vec<CapabilityRef>`** — the list of things this device can
  do. This is the *only* surface the rest of the daemon talks to.

Capabilities are the daemon's abstraction over heterogeneous hardware. Each
variant of `CapabilityRef` (`Fan`, `Rgb`, `Sensor`, `Range`, `Choice`, `Boolean`,
`Action`, `Battery`, `Equalizer`, `Dpi`, `OnboardProfiles`, `Lcd`, `KeyRemap`,
`Chain`, `Controller`, `TransportSwitchable`) has a matching trait and a generated
accessor (`as_rgb()`, `as_fan()`, …). **Usecases and engines never see concrete
device types — they call `device.as_rgb()` and talk to the trait object.** Adding
a capability variant is a deliberate compile-time event: the
`capability_dispatch!` macro in [drivers/mod.rs](../src/daemon/src/drivers/mod.rs)
forces every new variant to be classified `persisting` (its state is saved to
config) or `wire_only` (pushed to the GUI but not persisted) — there is no silent
default.

`Device` also covers serialization to the wire (`serialize()` builds a
`WireDevice` from the capability list), config state (`save_state`/`load_state`),
and optional hooks (`after_register`, `debug_info_extra`). **Controllers** host
child devices (wireless receivers, fan hubs) via `discover_children()`; chainable
ARGB hubs implement `ChainCapability`/`ChainAdapter` so the canvas engine can
compose per-zone frames — see [chain.rs](../src/daemon/src/drivers/chain.rs).

### Discovery — how a device gets constructed

There is **no central registry**. Each device module registers itself next to its
own code via `inventory::submit!`, and discovery walks those submissions
([registry/discovery.rs](../src/daemon/src/registry/discovery.rs)):

- **`TransportScanner`** — submitted by each transport. `discover_devices()` loops
  over every registered scanner (optionally platform-gated) and runs its bus scan.
- **`DeviceDescriptor`** `{ matches, make }` — submitted by each device module.
  Bus scanners build a `DiscoveryHandle` (carrying VID/PID, SMBus addr + bus kind,
  chain accessory id, Logitech slot, …) and `make_device()` returns the first
  descriptor whose `matches(handle)` is true, calling its `make(handle)` to
  construct the `Arc<dyn Device>`.
- **`SmBusScanEntry`** — SMBus devices also submit one of these so the scanner
  knows which addresses to probe on which bus (with an optional `pre_scan`).

A device file's tail therefore looks like:

```rust
inventory::submit!(DeviceDescriptor {
    matches: |h| matches!(h, DiscoveryHandle::UsbNonHid { vid: VID, pid: PID }),
    make: |_h| Ok(Arc::new(MyDevice::new()) as Arc<dyn crate::drivers::Device>),
});
```

`discover_handle()` then registers the constructed device through
[registry::usecases::registration](../src/daemon/src/registry/usecases/registration.rs), which stores
it in `AppState`, restores its saved config, runs `discover_children()`, and calls
`after_register()`.

### Plugins — devices without recompiling

Built-in host devices register at **compile time** via `inventory`. **Device plugins**
([drivers/plugins/](../src/daemon/src/drivers/plugins/)) add a parallel **runtime**
registry: `load_all_with_repos()` reads directory packages (`plugin.yaml` + entry
script) from the local plugins directory and every registered git-repo source, and
`make_device()` consults `plugins::match_handle()` *before* built-in descriptors —
so a plugin **shadows** a built-in host device for the same hardware. A single generic
`LuaDevice` implements the `Device` + capability traits and forwards each call into a
per-physical-root Lua worker thread (which owns the VM + transport). Dynamic children
use persistent routed `dev` tables on that same serialized worker. Plugins expose only
existing capability *kinds*; the capability taxonomy and engines stay native and
type-safe. See [docs/plugins.md](plugins.md) for the authoring guide.

Integration plugins have host-instantiated roots rather than hardware matches.
TCP roots connect from plugin config; the Linux hwmon root receives a scoped
local collection and enumerates stable sensor/fan children. In both cases the
host owns child relationships, reconciliation, and transport safety cleanup.

**Trust boundary.** Untrusted plugin Lua runs **in-process** in the daemon today.
Runtime VMs strip filesystem/process/native-loading globals and enforce heap,
instruction, and per-call time limits, but this is not process isolation. The
roadmap moves Lua into an unprivileged, killable plugin-host process, with
privileged hardware access behind a separate broker (as already used for
Windows), so a misbehaving plugin can be terminated without taking the daemon
down and never touches privileged hardware directly.

There are no plugins compiled into the daemon binary. At startup the daemon seeds
a non-removable **official plugin repo** record and clones it over the network
(`registry::ensure_official_repo`) — a clone failure (e.g. no network on first
launch) is logged and never fails boot, so the daemon simply has no official
plugins until a later successful clone. Official plugins use the same
enable-confirmation gate as all other sources. A plugin id
is owned by whichever source loads it first (official repo, then local
`plugins/`, then other repos in config order — see `load_all_with_repos`), so a
community repo can never shadow an existing plugin id; a collision is rejected
and surfaced via `take_plugin_load_warnings` rather than silently dropped.

Repo updates are atomic and repository-scoped. Git object storage is separate
from immutable executable revisions: Halo fetches, verifies the root index and
official signature, walks first-parent history to the newest compatible commit,
materializes and validates that proposed revision, then atomically selects it.
Update checks never modify executable files, and a failed update leaves the
previously selected revision running.

## IPC and usecases — the daemon's public API

The daemon and GUI are separate processes talking over a Unix domain socket
(`$XDG_RUNTIME_DIR/halod.sock`) or a Windows named pipe (`\\.\pipe\halod`), using
binary-framed JSON.

**Request/response path:** [ipc/router.rs](../src/daemon/src/ipc/router.rs)
receives each message, deserializes it into the typed `DaemonCommand` enum (from
[shared/src/commands.rs](../src/shared/src/commands.rs); the `type` field is
the serde discriminator), and `dispatch()` matches the variant to a usecase in
the owning domain module's `usecases/` (e.g. `daemon/src/lighting/usecases/`,
`daemon/src/cooling/usecases/`, `daemon/src/registry/usecases/`) **with
already-parsed typed arguments**. Usecases never re-parse raw JSON. The usecase
layer *is* the daemon's public API surface — one usecase module per concern
(`rgb`, `fan_curve`, `dpi`, `lcd`, `profiles`, `app_rules`, `settings`, …), living
next to the engine/state it drives rather than in one shared directory.

**High-frequency push path:** state that changes constantly — canvas preview, LCD
frames, sensor readings — is *not* request/response. The router runs
`tokio::broadcast` subscription loops (`engine_subscribe_loop`) that stream frames
to subscribed clients, and broad device-state changes go out via
`broadcast_state()`.

So **adding a command is a four-part change**:

1. Add a `DaemonCommand` variant in [shared/src/commands.rs](../src/shared/src/commands.rs).
2. Add a `dispatch()` arm in [router.rs](../src/daemon/src/ipc/router.rs).
3. Write/extend the usecase in the owning domain's `usecases/` that does the work (talking to devices through capability accessors).
4. Call it from the GUI.

## Engines — background loops over time

Engines (one per owning domain — `lighting/rgb_engine/`, `cooling/fan_curve.rs`,
`lcd/engine/`, `input/`, `profiles/focus_watcher/`; shared loop infra in
[daemon/src/run_loop.rs](../src/daemon/src/run_loop.rs)) are the daemon's
background drivers of device state. Each owns a tick interval (or is event-driven),
reads config from a `watch` channel, mutates devices through capability accessors,
and broadcasts changes. They are held in `AppState.engines` (a `OnceLock<Engines>`)
and are the reason device state evolves without the user touching anything.

The set (full internals in [engines.md](engines.md)):

- **canvas** — unified RGB effect loop; renders a tiny-skia pixmap per tick and
  samples each placed zone to per-LED RGB. Drives every `Rgb` capability. Its
  effect runtime has an intentional two-domain boundary: daemon-owned built-ins
  only for host services/the always-available designer, and runtime plugins for
  portable effects; see [engines.md](engines.md#effects).
- **fan_curve** — closed-loop temp→PWM with hysteresis, deadband, and a failsafe
  duty when the sensor is missing.
- **lcd** — renders template images / video frames to Kraken-style LCD panels.
- **action_executor**, **focus_watcher**, **key_remap** — event-driven engines for
  actions, per-app profile switching, and key remapping.

Engines receive runtime config via `watch` channels (`*_cfg_tx` on `Engines`), so
a usecase reconfigures an engine by sending on its channel rather than calling it
directly.

## State and config

- **`AppState`** ([daemon/src/state/mod.rs](../src/daemon/src/state/mod.rs)) is the shared
  hub: the device registry (`Mutex<Vec<Arc<dyn Device>>>`), connected IPC clients,
  discovery status, engine handles, fan-curve statuses, button-event broadcast,
  and shutdown signaling. It is passed (as `Arc<AppState>`) into usecases and
  engines.
- **Config** is persisted as a directory of YAML files by concern under
  `~/.config/halod/` (Linux) or `%APPDATA%\halod\` (Windows) — see
  [daemon/src/config/mod.rs](../src/daemon/src/config/mod.rs): `config.yaml`
  (global settings + active profile), `devices.yaml` (known devices, chain
  layouts, zone transforms, sensor visibility), `app_rules.yaml`, one
  `profiles/<name>.yaml` per profile, and `lcd/<name>.yaml` for saved custom
  LCD templates. Each file is saved via tmp-file + rename, fsync'd on Unix (a
  fully durable cross-platform atomic-replace is still being unified); the
  in-memory `Config` struct stays unified, so usecases read/write it exactly
  as before. A device's persistent capability state flows through
  `save_state`/`load_state` on the `Device` trait, keyed by each capability's
  `state_key()`, and lands in the active profile's `device_states` map.

## The GUI side

The GUI ([ui/src/](../src/ui/src/)) is immediate-mode (eframe/egui on a
glow/OpenGL renderer) — each frame is drawn from app state held in
[ui/src/app.rs](../src/ui/src/app.rs) as an `Arc<AppState>` (the latest daemon
state, cloned when its `watch` channel changes), with GUI-side derivations under
[ui/src/domain/models/](../src/ui/src/domain/models/). Daemon IPC lives in
[ui/src/runtime/ipc.rs](../src/ui/src/runtime/ipc.rs). The device page
is **capability-driven**: each capability tab is registered in
[ui/src/ui/screens/device/mod.rs](../src/ui/src/ui/screens/device/mod.rs) and shown only when
the connected device reports that capability, so a new capability shows up in the
UI by registering a tab — not by editing the device page. Two-way-bound widgets
gate daemon updates for ~1.5 s after a user edit (a `LiveGuard`) and debounce
outgoing commands to avoid fighting the user; reuse those rather than
hand-rolling. High-frequency canvas/LCD frames arrive on dedicated channels, not
the state broadcast.

## End-to-end: adding a new device

Putting the layers together, a new device is rarely *just* a code change. The full
checklist (see the `add-device` skill and `CLAUDE.md` → *Adding a device*):

1. **Transport** — reuse an existing one if the bus is already supported; write a
   new transport module only for a genuinely new bus.
2. **Protocol** — a module under `vendors/<vendor>/protocols/` encoding the wire
   format (with SPDX attribution if ported). Document it in [protocols/](protocols/).
3. **Device** — a file under `vendors/<vendor>/devices/` implementing `Device`,
   declaring `capabilities()`, plus the `inventory::submit!(DeviceDescriptor …)`
   (and `SmBusScanEntry` for SMBus parts).
4. **Supported-devices table** — add the row (Vendor, Model, VID:PID, Protocol link,
   Transport link, Platform) in [docs/supported-devices.md](supported-devices.md); this is the
   user-facing source of truth (linked from [README.md](../README.md)).
5. **udev rule** (Linux) — add built-in-driver access to
   [udev/60-halod.rules](../udev/60-halod.rules). Plugin HID/USB access is
   instead derived from the manifest by `halod udev-rules`.
6. **Docs** — new protocol/transport pages, and any kernel-module/PawnIO setup
   notes in [development.md](development.md).
7. **Test** — exercise the new frame encode/decode and parsing. `MockDevice` in
   [test_support.rs](../src/daemon/src/test_support.rs) covers capability-level
   tests; prefer property tests where a real invariant exists
   (`CLAUDE.md` → *Code conventions*).

## End-to-end: adding a new feature (command)

If the feature is "let the user do X to a device", it's a new capability and/or a
new command:

1. If X is a new *kind* of control, add a `CapabilityRef` variant + trait +
   accessor in [drivers/mod.rs](../src/daemon/src/drivers/mod.rs) (and classify it
   in `capability_dispatch!`), then implement it on the relevant device(s).
2. Add the `DaemonCommand` variant, the `dispatch()` arm, and the usecase
   (see *IPC and usecases* above).
3. Register a GUI panel for the capability (capability-driven device page) or wire
   the command into an existing view.
4. If X needs to evolve over time (effects, curves), it belongs in an engine, fed
   by a `watch` config channel.
5. Test the new behaviour in its owning crate.
