# Architecture

This document is the map of how HaloDaemon is put together: the layers, how they
connect, and where a new feature or device plugs in. Read it before adding a
device, a command, or an engine. Companion docs go deeper on specific areas:
[engines.md](engines.md) (engine internals), [development.md](development.md)
(toolchain, onboarding walkthrough, udev rules), and the per-transport pages
under [transports/](transports/). Plugin wire protocols are documented per
package in the [plugins repository](https://github.com/TimP4w/HaloDaemon-plugins).

## The crates

The workspace (`src/Cargo.toml`) has six members:

| Crate | Directory | Role |
|-------|-----------|------|
| `halod-shared` | [src/shared/](../src/shared/) | Shared types **and logic** other crates depend on: IPC commands, the `DeviceCapability` enum, and frame/zone definitions, plus real logic: curve evaluation, frame codecs, geometry, zone transforms, and LCD/effect helpers. |
| `halod-hwaccess` | [src/hwaccess/](../src/hwaccess/) | Low-level hardware-access primitives shared by the daemon and the broker. |
| `halod-broker` | [src/broker/](../src/broker/) | **Windows-only** privileged helper: runs as LocalSystem and performs the hardware access the per-user daemon can't. |
| `halod` | [src/daemon/](../src/daemon/) | The **daemon**: device I/O, discovery, engine loops, config persistence, IPC server. Runs **per-user** (not elevated); on Windows privileged hardware access is delegated to the `halod-broker` process. |
| `halod-gui` | [src/ui/](../src/ui/) | **GUI** (eframe/egui on a glow/OpenGL renderer) with a system tray. Talks to the daemon over IPC; holds no device logic. |
| `halod-plugin-signing` | [src/plugin-signing/](../src/plugin-signing/) | Plugin-repository index, hashing, signature, and bundle logic shared by the daemon and the release tool (`index`/`validate` CLI). |

The cardinal rule (`AGENTS.md` → *Keep layers separate*): protocol (wire
encode/decode) ↔ device (`Device` trait + capabilities) ↔ transport (byte
movement). Never leak transport bytes into a device, vendor wire formats into a
usecase, or device logic into the GUI.

## Driver layering: vendor → device → protocol → transport

The daemon is split into `application/` (IPC, usecases, state, bus),
`domain/` (devices, capabilities, engines, plugins), and `infrastructure/`
(drivers, transports, platform services). Built-in hardware code lives under
[infrastructure/drivers/](../src/daemon/src/infrastructure/drivers/), organized in four stacked
concerns. A single physical device is the composition of all four.

```
vendor/   organizational namespace (built-ins live under infrastructure/drivers/vendors/generic/)
  device      implements Device, declares capabilities()        ← what the rest of the daemon sees
    protocol    encodes/decodes the vendor wire format           ← "how do I phrase a 'set color' for this chip"
      transport   moves raw bytes (HID, SMBus, USB, …)           ← "how do those bytes reach the wire"
```

### Transport: moving bytes

A transport implements the `Transport` trait
([transports/](../src/daemon/src/infrastructure/drivers/transports/)): at its core
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
([infrastructure/drivers/](../src/daemon/src/infrastructure/drivers/)), costed in
bytes/sec, delaying (never dropping) writes that exceed the limit. The
ceiling is opt-in, not a default: a plugin declares `max_bytes_per_sec` in its
manifest transport scope; without one nothing is throttled. Live
throughput is still measured and surfaced to the GUI regardless of whether a
limit is set. Chain accessories (e.g. an NZXT F-series fan on a Kraken) don't
hold their own transport at all, so their writes are already covered by
their parent hub's limiter for free; the GUI resolves this by walking the
hub's chain links rather than
requiring the daemon to duplicate the number per accessory. SMBus devices
sharing one `PluginScanEntry` (e.g. multiple DRAM sticks) share one bus-level
limiter, since they already serialize through the same bus mutex; a
different plugin's `PluginScanEntry` on the same *physical* bus number still
opens an independent `SmBusDevice`/limiter today; cross-vendor unification on
one physical bus is a known, unaddressed gap. USB-control endpoints are also
metered and receive the plugin/device write-rate limit.

### Protocol: speaking the vendor's wire format

A protocol module sits on top of a transport and turns intent ("set zone 2 to
red", "read fan RPM") into the exact byte sequences the chip expects, and parses
replies back. Protocols hold a transport (often `Mutex<Option<T>>` so it can be
opened/closed) and expose typed methods. See the
[Logitech HID++ plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech)
for an example. When you port a wire format from third-party code, add the
SPDX attribution header (`AGENTS.md` → *Licensing and attribution*) and document the
format beside its package in the [official plugin repository](https://github.com/TimP4w/HaloDaemon-plugins).

### Device: the unit the daemon understands

A device implements the `Device` trait
([domain/device/](../src/daemon/src/domain/device/)). It owns one or more
protocol instances and answers two central questions:

- **Identity & lifecycle** - `id()` (stable across runs, unique per physical
  device), `name()`/`vendor()`/`model()`, `initialize()` (open connections,
  returns whether connected) and `close()`.
- **`capabilities() -> Vec<CapabilityRef>`** - the list of things this device can
  do. This is the *only* surface the rest of the daemon talks to.

Capabilities are the daemon's abstraction over heterogeneous hardware. Each
variant of `CapabilityRef` (`Cooling`, `Lighting`, `Sensor`, `Range`, `Choice`,
`Boolean`, `Action`, `Battery`, `Connection`, `Equalizer`, `Dpi`,
`OnboardProfiles`, `Lcd`, `KeyRemap`, `KeyboardLayout`, `Controller`, `Pairing`)
has a matching trait and a generated accessor (`as_lighting()`, `as_cooling()`, …).
**Usecases and engines never see concrete device types: they call
`device.as_lighting()` and talk to the trait object.** Adding a capability
variant is a deliberate compile-time event: the `capability_dispatch!` macro in
[domain/device/](../src/daemon/src/domain/device/) forces every new variant to be classified `persisting` (its state is saved to
config) or `wire_only` (pushed to the GUI but not persisted); there is no silent
default.

`Device` also covers serialization to the wire (`serialize()` builds a
`WireDevice` from the capability list), config state (`save_state`/`load_state`),
and optional hooks (`debug_info_extra`). **Controllers** host
child devices (wireless receivers, fan hubs) via `discover_children()`; chainable
ARGB hubs expose a chain host so the canvas engine can compose per-zone frames
(see [domain/device/](../src/daemon/src/domain/device/)).

### Discovery: how a device gets constructed

Discovery is driven by **transport scanners**, listed in the `SCANNERS` table in
[registry/observers/discovery.rs](../src/daemon/src/domain/registry/observers/discovery.rs).
`discover_devices()` walks that table in order (skipping any entry whose
`platform` doesn't match the host) and runs each bus scan; a scanner registered
in a module elsewhere in the tree exposes a `pub(crate) const SCANNER` that the
table names, so the full set is visible at the loop that runs it.

Each scan builds a `DiscoveryHandle` (carrying VID/PID, SMBus addr + bus kind,
chain accessory id, …) and hands it to `discover_handle()`. **Peripheral
construction itself is owned exclusively by the runtime plugin registry**:
`discover_handle()` calls `registry.make_device()`, which matches the handle
against loaded plugin manifests. Plugin SMBus packages contribute a
`PluginScanEntry` ([plugin/scan.rs](../src/daemon/src/domain/plugin/scan.rs)) so
the SMBus scanner knows which addresses to probe (with an optional `pre_scan`).

Whichever transport backend a matched plugin gets is resolved through the
`DESCRIPTORS` table in
[plugin/engine/backends/mod.rs](../src/daemon/src/domain/plugin/engine/backends/mod.rs):
one `PluginTransportDescriptor` per bus, defined next to the transport it wraps
and listed there, looked up by the manifest's `match.transport` string.

`discover_handle()` then registers the constructed device through
the registry usecases ([application/usecases/registry/](../src/daemon/src/application/usecases/registry/)), which store
it in `AppState`, restore its saved config, and run `discover_children()`.

### Plugins: devices without recompiling

Discovery roots and transport backends are fixed at **compile time** in those
tables; the devices they find are not. **Device plugins**
([domain/plugin/](../src/daemon/src/domain/plugin/)) are the **runtime**
registry: it loads packages (`plugin.yaml` + entry script) from registered
plugin sources, and `make_device()` matches a `DiscoveryHandle` against those
manifests via `match_handle()`. A single generic
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

**Sources.** Release builds embed a signed official plugin bundle
(`HALOD_OFFICIAL_PLUGIN_BUNDLE`, staged by `daemon/build.rs`). At startup the
daemon seeds the non-removable **official repo** record and extracts and
verifies that bundle, so official plugins are available offline. When the user
allows downloads, the latest official release is fetched in the background;
failure leaves the embedded revision active. Other sources are GitHub release
sources or imported release archives. Every source installs as a complete,
verified, immutable revision that is selected atomically; update checks never
modify installed files, and a failed update keeps the previous revision.
Official plugins use the same enable-confirmation gate as all other sources.
A plugin id is owned by whichever source loads it first (official repo, then
other sources in config order), so a community source can never shadow an
existing id; a collision is rejected and surfaced as a load warning.

## IPC and usecases: the daemon's public API

The daemon and GUI are separate processes talking over a Unix domain socket
(`$XDG_RUNTIME_DIR/halod.sock`) or a Windows named pipe (`\\.\pipe\halod`), using
binary-framed JSON.

**Request/response path:** the router in [application/ipc/](../src/daemon/src/application/ipc/)
receives each message, deserializes it into the typed `DaemonCommand` enum (from
[shared/src/commands.rs](../src/shared/src/commands.rs); the `type` field is
the serde discriminator), and `dispatch()` matches the variant to a usecase under
[application/usecases/](../src/daemon/src/application/usecases/) (one directory
per domain: `lighting/`, `cooling/`, `lcd/`, `registry/`, …) **with
already-parsed typed arguments**. Usecases never re-parse raw JSON. The usecase
layer *is* the daemon's public API surface. Before dispatch, `command_target()`
maps device-scoped commands to the device id whose per-device lock serializes
them (commands to different devices run concurrently), and
`command_writes_device()` marks commands whose failure means a hardware write did
not land.

**High-frequency push path:** state that changes constantly (canvas preview, LCD
frames, sensor readings) is *not* request/response. The router runs
`tokio::broadcast` subscription loops (`engine_subscribe_loop`) that stream frames
to subscribed clients, and state changes are published as retained bus topics
by recording a `Change` (see [application/bus/](../src/daemon/src/application/bus/)).

So **adding a command is a four-part change**:

1. Add a `DaemonCommand` variant in [shared/src/commands.rs](../src/shared/src/commands.rs).
2. In the router, add a `dispatch()` arm and, if the command targets a device,
   list it in `command_target()` (and in `command_writes_device()` if it writes
   hardware).
3. Write/extend the usecase in the owning domain's `application/usecases/` directory (talking to devices through capability accessors).
4. Call it from the GUI.

## Engines: background loops over time

Engines (one per owning domain under
[domain/](../src/daemon/src/domain/): `lighting/engine/`, `cooling/engine/`,
`lcd/engine/`, `input/engine/`, and the active-window observer in
`profiles/observers/`; shared loop infra in
[application/](../src/daemon/src/application/) `run_loop.rs`) are the daemon's
background drivers of device state. Each owns a tick interval (or is event-driven),
subscribes to its engine-config topic on the data bus, mutates devices through
capability accessors, and publishes changes. Each domain's state on `AppState`
holds its engine handle, set once at startup (gated by `engines_ready`); engines
are the reason device state evolves without the user touching anything.

The set (full internals in [engines.md](engines.md)):

- **canvas** - unified RGB effect loop; renders a tiny-skia pixmap per tick and
  samples each placed zone to per-LED RGB. Drives every `Rgb` capability. Its
  effect runtime has an intentional two-domain boundary: daemon-owned built-ins
  only for host services/the always-available designer, and runtime plugins for
  portable effects; see [engines.md](engines.md#effects).
- **fan_curve** - closed-loop temp→PWM with hysteresis, deadband, and a failsafe
  duty when the sensor is missing.
- **lcd** - renders template images / video frames to Kraken-style LCD panels.
- **action_executor**, **focus** (active-window observer), **key_remap** - event-driven engines for
  actions, per-app profile switching, and key remapping.

A usecase reconfigures an engine by updating config and recording a `Change`,
which republishes the engine-config topic, rather than calling the engine
directly.

## State and config

- **`AppState`** ([application/state/](../src/daemon/src/application/state/)) is the shared
  hub: config, the device registry, connected IPC clients, discovery status,
  per-domain state (lighting, cooling, LCD, focus, input) with their engine
  handles, the plugin registry, the data bus, per-device command locks, and
  shutdown signaling. It is passed (as `Arc<AppState>`) into usecases and
  engines.
- **Config** is persisted as a directory of YAML files by concern under
  `~/.config/halod/` (Linux) or `%APPDATA%\halod\` (Windows); see
  [daemon/src/config/mod.rs](../src/daemon/src/config/mod.rs): `config.yaml`
  (global settings + active profile), `devices.yaml` (known devices, chain
  layouts, zone transforms), `app_rules.yaml`, one
  `profiles/<name>.yaml` per profile, and `lcd/<name>.yaml` for saved custom
  LCD templates. Each file is saved via tmp-file + rename, fsync'd on Unix (a
  fully durable cross-platform atomic-replace is still being unified); the
  in-memory `Config` struct stays unified, so usecases read/write it exactly
  as before. A device's persistent capability state flows through
  `save_state`/`load_state` on the `Device` trait, keyed by each capability's
  `state_key()`, and lands in the active profile's `device_states` map.

## The GUI side

The GUI ([ui/src/](../src/ui/src/)) is immediate-mode (eframe/egui on a
glow/OpenGL renderer): each frame is drawn from app state held in
[ui/src/app.rs](../src/ui/src/app.rs) as an `Arc<TopicStore>` (the latest daemon
bus state, cloned when its `watch` channel changes), with GUI-side derivations under
[ui/src/domain/models/](../src/ui/src/domain/models/). Daemon IPC lives in
[ui/src/runtime/ipc.rs](../src/ui/src/runtime/ipc.rs). The device page
is **capability-driven**: each capability tab is registered in
[ui/src/domain/models/](../src/ui/src/domain/models/) (`device_tabs`) and shown only when
the connected device reports that capability, so a new capability shows up in the
UI by registering a tab, not by editing the device page. Two-way-bound widgets
gate daemon updates for ~1.5 s after a user edit (a `LiveGuard`) and debounce
outgoing commands to avoid fighting the user; reuse those rather than
hand-rolling. High-frequency canvas/LCD frames arrive on dedicated channels, not
the state broadcast.

## End-to-end: adding a new device

New device support is added in the
[HaloDaemon plugins repository](https://github.com/TimP4w/HaloDaemon-plugins) as
a Lua package, not as a Rust vendor module in this repo. See
[development.md](development.md#adding-device-support) for the plugin workflow
and the manifest/Lua authoring references.

The built-in Rust driver stack (`infrastructure/drivers/`) is now limited to the generic
`computer`/hwmon-class devices that don't fit a plugin. The vendor → device →
protocol → transport layering above is how those built-ins are structured, and a
plugin needs a **new Rust change here only** when it requires a capability or a
scoped transport operation the daemon does not yet expose: add that core
primitive first, then consume it from Lua. When you do touch a built-in:

1. **Transport** - reuse an existing one if the bus is already supported; write a
   new transport module only for a genuinely new bus, and document it in
   [transports/](transports/).
2. **Device** - a file under `infrastructure/drivers/vendors/<vendor>/devices/` implementing
   `Device` and declaring `capabilities()`, reached from the discovery root that
   constructs it.
3. **udev rule** (Linux) - add built-in-driver access to
   [udev/60-halod.rules](../udev/60-halod.rules). Plugin HID/USB access is
   instead derived from the manifest by `halod udev-rules`.
4. **Test** - exercise the new frame encode/decode and parsing. `MockDevice` in
   [test_support.rs](../src/daemon/src/test_support.rs) covers capability-level
   tests; prefer property tests where a real invariant exists
   (`AGENTS.md` → *Code conventions*).

## End-to-end: adding a new feature (command)

If the feature is "let the user do X to a device", it's a new capability and/or a
new command:

1. If X is a new *kind* of control, add a `CapabilityRef` variant + trait +
   accessor in [domain/device/](../src/daemon/src/domain/device/) (and classify it
   in `capability_dispatch!`), then implement it on the relevant device(s).
2. Add the `DaemonCommand` variant, the `dispatch()` arm (plus its
   `command_target()`/`command_writes_device()` entries), and the usecase
   (see *IPC and usecases* above).
3. Register a GUI panel for the capability (capability-driven device page) or wire
   the command into an existing view.
4. If X needs to evolve over time (effects, curves), it belongs in an engine, fed
   by its engine-config topic.
5. Test the new behaviour in its owning crate.
