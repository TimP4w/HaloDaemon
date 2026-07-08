# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Before committing

Always run, from `src/`, and resolve any issues before committing:

- `cargo fmt --all`
- `cargo clippy --all-targets -- -D warnings` (matches the CI gate; see [Lint policy](#lint-policy))

The workspace manifest lives at `src/Cargo.toml`. Members are the directories `shared`, `daemon`, `ui` (crates `halod-shared`, `halod`, `halod-gui`).

## Code conventions

- **Minimize code.** Prefer simplicity and maintainability — the smallest change that does the job. Reuse existing helpers, traits, and patterns instead of adding parallel ones; check for an existing accessor/usecase/transport before writing a new one.
- **Comments are sparse.** Avoid lengthy comments. If a behaviour needs a long explanation, encode it in a test that demonstrates it rather than prose.
- **Test all new code — the GUI included.** Every new behaviour gets a test in its owning crate; don't land logic that isn't exercised. Prefer property tests (`proptest`, already a dev-dependency in `shared` and `halod`) over example-based ones where a meaningful invariant exists — pick invariants that actually pin down correctness (round-trip encode/decode equals identity, output stays within device bounds, monotonic curves stay monotonic) rather than restating the implementation. See the `proptest!` blocks in [shared/src/frames.rs](src/shared/src/frames.rs), [shared/src/zone_transform.rs](src/shared/src/zone_transform.rs), and [daemon/src/cooling/fan_curve.rs](src/daemon/src/cooling/fan_curve.rs).
  - **`halod-gui` is not exempt.** It can't be driven through a live egui frame in tests, but the logic *around* the immediate-mode painting can and must be: state reducers (seeding/debounce/selection), geometry math (widget rects, clamps, resize/scale), and value mapping (params ↔ wire types, formatting). Factor that logic into free functions taking plain data — not `&mut egui::Ui` — so it's unit-testable, then test it. Build a minimal `TabCtx` over `AppState::default()` for daemon-facing state logic; see the `seed_if_needed`/`send_def`/`spawn_widget` tests in [ui/src/device/lcd_editor.rs](src/ui/src/device/lcd_editor.rs). When the daemon and GUI must agree on a constant or formula (e.g. an LCD widget's size factors), pin it with a test on each side.
- **Keep the layers separate.** Maintain the clean split: protocol (wire encode/decode) ↔ device (`Device` trait + capabilities) ↔ transport (byte movement). Don't leak transport bytes into a device, vendor wire formats into a usecase, or device logic into the GUI.
- **No device assumptions in shared code.** Engines, usecases, and any layer above a concrete driver must NEVER assume device-specific behaviour (e.g. "the panel latches the last frame so we can skip the re-stream", "this controller ignores duplicate writes", timing/keepalive quirks). This is a NO-GO: baking one device's quirk into generic code silently breaks every other device that doesn't share it. Such a property is something the *device declares* through its descriptor/capability, and shared code branches on that declared flag — with the safe/conservative behaviour as the default. If shared code must behave differently for a device, add the flag to the capability descriptor and default it to the safe value.
- **Attribute references.** When porting or adapting third-party code, add a REUSE-style SPDX header to the file and register the license — see [Licensing & attribution](#licensing--attribution).

## Build & test

All commands run from `src/`. The toolchain is provided by Nix (`nix develop`); prefix commands accordingly when not already in the dev shell, e.g. `nix develop --command bash -c "cd src && cargo build"`.

- Build everything: `cargo build`
- Run the daemon: `cargo run -p halod`
- Run the GUI: `cargo run -p halod-gui`
- Test everything: `cargo test --workspace`
- Test one crate: `cargo test -p halod` (or `-p halod-shared`)
- Run a single test: `cargo test -p halod <test_name>` (or `<module>::<test_name>`)
- Check test coverage gaps: `cargo mutants` (config in [src/.cargo/mutants.toml](src/.cargo/mutants.toml); transports/vendors/ui are excluded as hardware-only). A surviving mutant means logic no test caught — close the gap with a test, don't just rerun.

`docs/development.md` has the full Linux/Windows (MSYS2 UCRT64) setup, the device-onboarding walkthrough, and udev rule conventions — read it before adding a device or touching platform glue.

A CodeGraph MCP server (`codegraph`) is wired up in [src/.mcp.json](src/.mcp.json): it indexes the workspace into a queryable code graph for semantic search and cross-reference navigation. Optional and started on demand; install the `codegraph` binary if you want it.

## Architecture

[docs/architecture.md](docs/architecture.md) is the full map — layer boundaries (vendor → device → protocol → transport), discovery, IPC/usecases, engines, state/config, and step-by-step recipes for adding a device or a command. Read it before implementing a new feature or device; the summary below is the orientation.

Three crates under `src/`:

- **`halod-shared`** — shared wire types only (no logic). Both other crates depend on it. IPC messages, `Capability` enum, frame/command definitions.
- **`halod`** — the daemon: device I/O, engine loops, config persistence. Runs elevated.
- **`halod-gui`** — eframe/egui GUI with a system tray; talks to the daemon, holds no device logic.

### Daemon ↔ GUI IPC

The two processes communicate over a Unix domain socket (`$XDG_RUNTIME_DIR/halod.sock`) on Linux or a named pipe (`\\.\pipe\halod`) on Windows, using binary-framed JSON. [daemon/src/ipc/router.rs](src/daemon/src/ipc/router.rs) deserializes each message into the typed `DaemonCommand` enum (from [shared/src/commands.rs](src/shared/src/commands.rs); the `type` field is the serde discriminator) and `dispatch()` matches the variant to a usecase in the owning domain module's `usecases/` (e.g. `daemon/src/lighting/usecases/`, `daemon/src/registry/usecases/`) with already-parsed typed arguments — usecases never re-parse raw JSON. High-frequency state (canvas preview, sensor readings) is pushed via `tokio::broadcast` subscription loops rather than request/response. When adding a command, add a `DaemonCommand` variant, a `dispatch()` arm, and the matching usecase — the usecase layer is the daemon's public API surface.

### Driver layering

Devices are organized as **vendor → device → protocol → transport**, all under [daemon/src/drivers/](src/daemon/src/drivers/):

- A **device** implements the `Device` trait and declares its `capabilities()` (Rgb, Fan, Battery, Lcd, Dpi, …). Each capability has a matching accessor (`as_rgb()`, `as_fan()`) returning a trait object — usecases and engines talk to devices only through these capability accessors, never concrete types.
- A **transport** (HID, SMBus, USB control, PawnIO/LpcIO) moves bytes. Each device file registers itself with `inventory::submit!(DeviceDescriptor { matches, make })` — `discover()` walks `inventory::iter` and runs the first descriptor whose `matches` accepts the `DiscoveryHandle` (VID/PID, SMBus addr, chain accessory id, …). There is no central registry; the descriptor lives next to the device. SMBus devices also submit a `SmBusScanEntry` so the bus gets probed.
- A **protocol** module encodes/decodes the vendor wire format on top of a transport.
- **Controllers** host child devices (wireless receivers, fan hubs) via `discover_children()`. Chainable ARGB hubs implement `ChainCapability`/`ChainAdapter` so the canvas engine can compose per-zone LED frames; see [daemon/src/drivers/chain.rs](src/daemon/src/drivers/chain.rs).

**Adding a device — don't stop at the code.** Beyond the device file + descriptor (and a new protocol/transport module if the wire format is new), every device also requires:

1. **Supported-devices table** — add a row under the right category in [docs/supported-devices.md](docs/supported-devices.md) (Vendor, Model, VID:PID, Protocol link, Transport link, Platform). This is the user-facing source of truth for supported hardware (linked from [README.md](README.md)).
2. **udev rule** (Linux) — add the VID:PID to [udev/60-halod.rules](udev/60-halod.rules), grouped by vendor, or the device won't be reachable without root.
3. **Docs** — if you added a new protocol or transport, write `docs/protocols/<name>.md` / `docs/transports/<name>.md` and link it from the README table; note any vendor-specific setup (kernel module, PawnIO, etc.) in `docs/development.md`.
4. **Test** — exercise the new frame encode/decode and any parsing with a unit test (`MockDevice` in [daemon/src/test_support.rs](src/daemon/src/test_support.rs) covers capability-level tests).

### GUI

The GUI ([ui/src/](src/ui/src/)) uses eframe/egui in immediate-mode style. State is fetched from the daemon via the IPC socket ([ui/src/ipc.rs](src/ui/src/ipc.rs)) and cached in a local `Model` ([ui/src/model.rs](src/ui/src/model.rs)) that drives the next frame. The device page is capability-driven: each capability tab is registered in [ui/src/device/mod.rs](src/ui/src/device/mod.rs) and shown only when the connected device reports that capability. High-frequency canvas/LCD frames arrive on dedicated async channels kept separate from the state poll loop. The system tray is handled per-platform in [ui/src/tray/](src/ui/src/tray/).

### Engines

Engines live inside their owning domain module, not a shared `engines/` tree: **canvas**/**direct** (unified RGB effect loop sampling a tiny-skia pixmap per zone) in [daemon/src/lighting/rgb_engine/](src/daemon/src/lighting/rgb_engine/), **fan_curve** (closed-loop temp→PWM with hysteresis and failsafe) in [daemon/src/cooling/fan_curve.rs](src/daemon/src/cooling/fan_curve.rs), **lcd** (template image rendering) in [daemon/src/lcd/engine/](src/daemon/src/lcd/engine/), plus `action_executor`/`key_remap` in [daemon/src/input/](src/daemon/src/input/) and `focus_watcher` in [daemon/src/profiles/focus_watcher/](src/daemon/src/profiles/focus_watcher/) — documented in `docs/engines.md`. [daemon/src/engines.rs](src/daemon/src/engines.rs) holds only the shared `engine_run_loop`/`EngineRunConfig` infra every engine's watch-loop is built on. `AppState` ([daemon/src/state/mod.rs](src/daemon/src/state/mod.rs)) composes each domain's state struct and holds the shared device registry; engines receive runtime config via `watch` channels.

### Config

Persisted as a directory of YAML files under `~/.config/halod/` (Linux) or `%APPDATA%\halod\` (Windows), split by concern — see [daemon/src/config/mod.rs](src/daemon/src/config/mod.rs):

- `config.yaml` — `active_profile` + `GlobalConfig` (engine toggles, log level, close-to-tray)
- `devices.yaml` — known devices, chain layouts, zone transforms, sensor visibility
- `app_rules.yaml` — app-focus → profile rules
- `profiles/<name>.yaml` — one file per profile (device capability state, canvas overrides, RGB Lighting targets)
- `lcd/<name>.yaml` — saved custom LCD templates ([daemon/src/lcd/usecases/templates.rs](src/daemon/src/lcd/usecases/templates.rs))
- `media/lcd_images/` — uploaded LCD image library

Every file is written atomically (tmp + rename) on each save; profile files are named from a sanitized slug of the profile name and pruned on rename/delete.

## Licensing & attribution

The workspace is `GPL-3.0-or-later`. The repo follows the [REUSE](https://reuse.software/) convention — every file's license is declared, and full texts live in the top-level `LICENSES/` directory.

When you port or adapt third-party code:

1. Add an SPDX header to the top of the file, e.g.

   ```rust
   // SPDX-License-Identifier: GPL-3.0-or-later
   // SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
   ```

   Use the upstream's actual license/copyright and a link to the source. See [drivers/vendors/nzxt/protocols/nzxt_kraken.rs](src/daemon/src/drivers/vendors/nzxt/protocols/nzxt_kraken.rs) for the pattern.
2. If the upstream license text isn't already in `LICENSES/`, add the matching `<SPDX-id>.txt` file there.

Rust crate dependency licenses are tracked separately by cargo-about ([src/about.toml](src/about.toml) + [src/about_licenses.hbs](src/about_licenses.hbs)); regenerate with the command at the top of `about.toml` when dependencies change.

[docs/licenses.md](docs/licenses.md) is the full map of how licensing/attribution is discovered and shipped across every layer — REUSE/SPDX, cargo-about, bundled fonts, PawnIO blobs, FFmpeg, and the Windows installer. Read it before touching `REUSE.toml`, `about.toml`, or the installer.

## Lint policy

`dead_code` is allowed workspace-wide (vendor/protocol scaffolding is intentionally kept), as are `type_complexity` and `too_many_arguments` (closure-heavy UI and driver builders). CI gates on `cargo clippy -- -D warnings`, so don't reintroduce these as hard errors. See the `[workspace.lints]` section in `src/Cargo.toml` for rationale.
