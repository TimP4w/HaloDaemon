# Development Guide

## Requirements

### Preferred: Nix

```bash
nix develop
```

This provides the full toolchain: Rust, GCC, pkg-config, clang/libclang, D-Bus, HIDAPI, I2C tools, libusb1, PipeWire, PulseAudio, Wayland libs (the GUI), and `ffmpeg`.

### Manual (Linux)

Install: `cargo`, `rustc`, `hidapi`, `libusb1`, `pkg-config`, `clang`, `dbus`, `pipewire`, `udev`, `wayland`, `libxkbcommon`, `libGL` (the GUI needs the Wayland/OpenGL stack).

`ffmpeg` is an **optional runtime dependency**: it is only needed for the LCD **video** playback mode (decoding a local file into frames). Without it, every other feature works and the GUI greys out the "Play Video" button. Install it from your package manager (`apt install ffmpeg` / `dnf install ffmpeg` / `pacman -S ffmpeg`) and ensure it is on `PATH`.

### Windows (MSYS2 UCRT64)

Open the **MSYS2 UCRT64** shell and install:

```bash
pacman -S --needed \
  mingw-w64-ucrt-x86_64-pkgconf \
  mingw-w64-ucrt-x86_64-gcc \
  mingw-w64-ucrt-x86_64-rust \
  git
```

Use `ucrt-x86_64` packages (not the older `x86_64` MSVCRT ones). Do not mix MSVC-toolchain Rust with MSYS2's MinGW build environment.

For LCD video mode (optional), also install `mingw-w64-ucrt-x86_64-ffmpeg`, or place any `ffmpeg.exe` on `PATH`; the daemon resolves it via `PATH` the same way on Linux and Windows.

---

## Build & Run

```bash
# Build everything
nix develop --command bash -c "cd src && cargo build"

# Run the daemon
nix develop --command bash -c "cd src && cargo run -p halod"

# Run the UI
nix develop --command bash -c "cd src && cargo run -p halod-gui"

# Run daemon unit tests
nix develop --command bash -c "cd src && cargo test -p halod"
```

On Windows (MSYS2 UCRT64 shell):

```bash
cargo build -p halod-gui
cargo run -p halod-gui
```

### Windows privilege separation

On Windows the privileged register-bus transports (chipset SMBus / PawnIO, AMD
SMN) run in a separate elevated process, `halod-broker.exe`; everything else —
including `halod.exe` itself — runs at the user's normal (medium) integrity.
`halod.exe` is **never** elevated and is **not** a service: the GUI launches it
as a plain user process, and it brings the broker up on first register-bus
access. When the `HalodBroker` service is installed it starts on demand via the
SCM (no UAC); the broker self-stops once idle. `halod-broker.exe` links only
`halod-hwaccess` + `windows` — it cannot run Lua.

For a **dev run** (`cargo run -p halod`, no service installed) the daemon
launches the broker itself: the **first** time it touches a DRAM/GPU/SuperIO
register you get **one UAC prompt for `halod-broker.exe`**. Accept it and
register-bus devices work; decline it and they're unavailable (a clear log line
explains why) while HID and network/plugin devices keep working. Windows
register-bus access always goes through the broker; the daemon has no direct,
in-process privileged-hardware path.

See [Windows privilege separation](windows-privilege-separation.md) for the full
design, threat model and process topology.

---

## Building the Windows installer

The release installer (`halod-setup-x64.exe`) bundles all three binaries, the
PawnIO blobs, and a GPL ffmpeg build for LCD video mode. CI produces it in the
`windows-installer` job of [create-release.yml](../.github/workflows/create-release.yml);
locally, [`packaging/windows/build-installer.ps1`](../packaging/windows/build-installer.ps1)
runs the same three stages end to end (build → stage → compile).

### One-time prerequisites

On top of the MSYS2 UCRT64 toolchain above, install ffmpeg + the dependency
walker used by staging, and the Inno Setup compiler:

```powershell
# ffmpeg.exe (bundled for LCD video) + ntldd (collects ffmpeg's DLL deps)
C:\msys64\usr\bin\bash.exe -lc "pacman -S --needed --noconfirm mingw-w64-ucrt-x86_64-ffmpeg mingw-w64-ucrt-x86_64-ntldd"

# Inno Setup 6 — the installer compiler (ISCC.exe)
winget install --id JRSoftware.InnoSetup
```

`winget` may drop Inno Setup in a per-user path (`%LOCALAPPDATA%\Programs\Inno Setup 6\`)
rather than `C:\Program Files (x86)\` — the build script probes both plus `PATH`,
so you don't need to hardcode it.

### Build it

From the repo root, in PowerShell:

```powershell
# Full run: build release binaries, stage, compile the installer
.\packaging\windows\build-installer.ps1

# Stamp a real version (as CI does with the release tag)
.\packaging\windows\build-installer.ps1 -AppVersion 1.2.3

# Install the ffmpeg/ntldd/Inno Setup prerequisites first, then build
.\packaging\windows\build-installer.ps1 -InstallDeps

# Reuse the existing src\target\release binaries (skip the cargo build)
.\packaging\windows\build-installer.ps1 -SkipBuild
```

The result lands at `packaging\windows\Output\halod-setup-x64.exe`. The three
stages can also be run by hand:

1. `cargo build --release -p halod -p halod-gui -p halod-broker` (from `src/`).
2. `.\packaging\windows\stage-release.ps1` — copies the exes, ffmpeg + its DLLs,
   and the PawnIO blobs into `packaging\windows\staging\`.
3. `ISCC.exe /DAppVersion=<version> packaging\windows\halod.iss` — compiles the
   staged tree into the installer.

> A local build without `cargo-about` on `PATH` logs a warning and omits the
> Rust-crate license page; CI sets `HALOD_REQUIRE_LICENSES=1` to make that fatal
> for release artifacts. The `PrivilegesRequired=admin` + HKCU compiler warning
> is expected — the installer only cleans up the tray's HKCU "Start on boot"
> value on uninstall (see the `[Registry]` note in `halod.iss`).

### Testing the installer

Install into a throwaway VM or Windows Sandbox, not your daily machine — it
registers a LocalSystem service and writes under `%ProgramFiles%`. After running
the installer, sanity-check:

- Files land in `%ProgramFiles%\HaloDaemon` (three exes, `ffmpeg.exe` + `libav*` DLLs,
  the four `.bin` blobs, license texts).
- `sc.exe query HalodBroker` shows the broker service registered as `DEMAND_START`
  and stopped.
- Launching `halod-gui.exe` spawns the user-level `halod.exe` (`Get-Process halod, halod-gui`).
- First register-bus device access raises **one** UAC prompt for
  `halod-broker.exe`; HID and network/plugin devices work without it.
- Re-running the installer over an existing install upgrades cleanly (the
  `PrepareToInstall` step stops the service/processes first), and uninstalling
  removes the service and program files.

---

## Project layout

Crates under `src/`:

| Crate | Purpose |
|-------|---------|
| `halod` | Background device I/O, engine loops, config persistence |
| `halod-gui` | GUI with system tray |
| `halod-shared` | Shared wire types (IPC messages) |
| `halod-hwaccess` | Raw privileged register-bus primitives (SMBus, PawnIO) + the broker RPC protocol; shared by `halod` and `halod-broker` |
| `halod-broker` | Windows-only elevated register-bus broker (see [Windows privilege separation](#windows-privilege-separation)) |

The daemon and UI communicate over a local IPC channel, a Unix domain socket (`$XDG_RUNTIME_DIR/halod.sock`) on Linux, a named pipe (`\\.\pipe\halod`) on Windows — using binary-framed JSON messages.

Configuration lives in `~/.config/halod/config.yaml` (Linux) or `%APPDATA%\halod\config.yaml` (Windows).

---

## Adding a new device

### 1. Create the device file

`src/daemon/src/drivers/vendors/<vendor>/devices/<model>.rs`

Implement the `Device` trait. Declare capabilities in `fn capabilities()` — the list of `Capability` variants the device supports (e.g. `Capability::Rgb`, `Capability::Fan`, `Capability::Battery`). Each capability variant has a matching accessor (`as_rgb()`, `as_fan()`, etc.) that returns a trait object.

See an official plugin package (for example [`logitech_g560`](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech_g560)) for a complete device-protocol example.

### 2. Add a protocol module (if new)

`src/daemon/src/drivers/vendors/<vendor>/protocols/<proto>.rs`

Implement the message types, frame builders, and response parsers for the wire protocol. Wire it to the appropriate transport (HID, SMBus, USB control). See [protocols](protocols/) for existing examples.

### 3. Build and register a descriptor

In the vendor's `mod.rs`, construct a `HidDeviceDescriptor` (or `SmbusDeviceDescriptor`, etc.) with the VID, PID(s), and a factory closure that builds the device. Add it to the descriptor list returned by the vendor's `descriptors()` function.

`HidTransport::discover()` iterates all registered descriptors and calls the factory when a matching USB device is found.

For devices that host child devices (e.g. a wireless receiver or a fan hub), implement the `Controller` trait and override `discover_children(app)` — it is called after `initialize()` and should return the child `Device` objects.

For chainable ARGB hubs, implement `ChainCapability` and `ChainAdapter` so the canvas engine can compose per-zone LED frames. Locked auto-detected links (NZXT) use a fixed chain; user-added generic links (ASUS Aura USB) use the `ChainHost` CRUD interface.

### 4. Add udev rules (Linux)

`udev/60-halod.rules`

One rule per VID:PID for HID devices. Use `uaccess` alone — it grants the
device to the active local-session user via ACL, so no group/mode is needed
(and adding `GROUP`/`MODE` would widen access beyond the seated user):
```
KERNEL=="hidraw*", ATTRS{idVendor}=="<vid>", ATTRS{idProduct}=="<pid>", TAG+="uaccess"
```

For USB control-transfer devices use `SUBSYSTEM=="usb"` instead. Keep rules grouped by vendor with a comment above each group. All PIDs in lowercase hex. Verify with:
```bash
sudo udevadm verify udev/60-halod.rules
```

### 5. Add a docs page

Add `docs/protocol.md` inside the plugin package (with local split pages for a large protocol) and reference the device in the official plugin repository's supported-devices table.

---

## Adding a new protocol

1. Create `src/daemon/src/drivers/vendors/<vendor>/protocols/<name>.rs`.
2. Implement frame builders (requests) and response parsers as plain functions or a small struct. Keep protocol logic separate from device state.
3. Wire to the transport: take a `HidTransport`, `SmbusTransport`, or `UsbControlTransport` reference in the device struct and call it from the `Device` trait implementation.
4. Add `docs/protocol.md` to the owning package in the [official plugin repository](https://github.com/TimP4w/HaloDaemon-plugins), covering the frame format, key commands, credits/references, and known limitations. Large packages may use that page as a local index to split pages in the same `docs/` directory. The doc is the source of truth — every concrete value must trace to the implementation.
5. Keep protocol documentation package-local; do not add a centralized protocol page to the daemon repository.

### Adding a new transport

Every transport gets write-rate throughput, a limiter, and enforcement for free by wrapping its raw I/O handle in `Metered<T>` ([daemon/src/drivers/rate_limit.rs](../src/daemon/src/drivers/rate_limit.rs)) instead of holding the handle directly:

1. Give the transport struct an `io: Metered<YourRawIo>` field; its `open(...)` takes a trailing `limit: Option<WriteRateLimit>` and builds it with `Metered::new(raw_io, limit)`.
2. Route every write through `io.write_access(len).await` (async transports) or `io.write_access_blocking(len)` (sync transports, e.g. USB bulk/control — only from a thread that's allowed to block) to get the gated `&YourRawIo` back; route every read through the unmetered `io.read_access()`. For batch transports whose byte count is only known after the operation runs (e.g. SMBus), use `io.write_tallied(...)` instead.
3. Implement the transport trait's `rate_status()`/`set_write_rate_limit()` by delegating to `io.status()`/`io.set_limit()` — these are required methods, not optional ones, so a transport that skips this step won't compile.

Because the raw handle only exists behind the gate, there's no separate step to "remember" metering — a write path that bypasses it simply has no handle to write through.

## Device registration flow

```
HidTransport::discover()
  → iterates HidDeviceDescriptor list
    → for each connected hidraw device, match VID:PID
      → call factory closure → Device
        → device.initialize()
          → if Controller: device.discover_children(app)
```

SMBus and USB control devices follow the same pattern with their own descriptor types and discovery loops.

---

## Testing

Write unit tests in `#[cfg(test)] mod tests` at the bottom of the relevant file. Use `#[tokio::test]` for async tests.

Write tests for:
- **Pure logic** — response parsers, frame builders, model mappings (no hardware required)
- **Serialization** — `serialize()` outputs for new device types
- **Descriptor registration** — inventory smoke tests when adding a new device descriptor
- **Bug fixes** — add a test that reproduces the bug before fixing it

For tests that need a device or hub trait object, create a minimal `MockDevice`/`MockHub` struct inside the test module, or reuse the shared `MockDevice` in `test_support.rs`. See `ipc/serializer.rs` for an example.

Do not write tests that require real hardware, open sockets, or touch the filesystem.
