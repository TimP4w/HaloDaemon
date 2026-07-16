# Development guide

This repository contains the HaloDaemon core: the daemon, GUI, shared types,
hardware-access brokers, plugin runtime, and release tooling.

> [!IMPORTANT]
> New devices should be implemented as **Lua plugins**, not as Rust drivers in
> this repository. Device plugins live in the
> [HaloDaemon plugins repository](https://github.com/TimP4w/HaloDaemon-plugins)
> and can be developed, tested, and released independently from the daemon.

## Development environment

### Nix (recommended)

From the repository root:

```bash
nix develop
cd src
```

The development shell provides Rust, GCC, pkg-config, clang/libclang, D-Bus,
HIDAPI, libusb, I2C tools, PipeWire, PulseAudio, and the Wayland/OpenGL
dependencies required by the GUI.

### Linux without Nix

Install a Rust toolchain plus the development packages for HIDAPI, libusb,
pkg-config, clang, D-Bus, PipeWire, udev, Wayland, libxkbcommon, and OpenGL.
Package names vary by distribution.

`ffmpeg` is optional and is only used for LCD video playback. All other
features work without it.

### Windows

Use an **MSYS2 UCRT64** shell and install the UCRT64 Rust toolchain:

```bash
pacman -S --needed \
  mingw-w64-ucrt-x86_64-pkgconf \
  mingw-w64-ucrt-x86_64-gcc \
  mingw-w64-ucrt-x86_64-rust \
  git
```

Do not mix MSVC Rust with the MSYS2 MinGW environment. LCD video playback also
requires `mingw-w64-ucrt-x86_64-ffmpeg` or another `ffmpeg.exe` on `PATH`.

## Build and run

Run Cargo commands from `src/`:

```bash
# Build the workspace
cargo build --workspace

# Run the daemon
cargo run -p halod

# Run the GUI
cargo run -p halod-gui

# Run all tests
cargo test --workspace
```

On Windows, the GUI starts the daemon as a normal user process. Privileged
SMBus, AMD SMN, and Super I/O operations are delegated to `halod-broker.exe`.
A development run may show one UAC prompt when that access is first needed.
See [Windows privilege separation](windows-privilege-separation.md) for the
process model and security boundary.

## Project layout

The Rust workspace is under `src/`:

| Crate | Purpose |
|---|---|
| `halod` | Daemon, plugin runtime, discovery, engines, configuration, and IPC server |
| `halod-gui` | Desktop GUI and system tray |
| `halod-shared` | Shared IPC, device, capability, engine, and rendering types |
| `halod-hwaccess` | Low-level privileged hardware-access primitives and broker protocol |
| `halod-broker` | Windows-only privileged hardware-access helper |
| `halod-plugin-signing` | Plugin repository validation, indexing, and signing tools |

The daemon and GUI communicate over a Unix socket on Linux and a named pipe on
Windows. For a deeper overview, see [Architecture](architecture.md).

## Adding device support

Add device support in the
[HaloDaemon plugins repository](https://github.com/TimP4w/HaloDaemon-plugins),
using Lua. Do not add a new Rust vendor module, `Device` implementation, or
compiled device descriptor to the daemon.

A typical plugin package contains:

```text
my_device/
├── plugin.yaml       device matches, permissions, capabilities, and transports
├── main.lua          discovery, initialization, status, and capability callbacks
├── test.lua          optional hardware-free regression tests
├── docs/
│   └── protocol.md   wire protocol, references, and known limitations
└── assets/           optional images
```

The usual workflow is:

1. Add a package manifest declaring the supported devices, platforms,
   permissions, capabilities, and narrowly scoped transports.
2. Implement the device protocol and capability callbacks in `main.lua`.
3. Add `test.lua` coverage for encoding, parsing, and transport traffic where
   possible.
4. Document the protocol alongside the plugin and update the plugin repository
   index.
5. If a Linux HID or USB device needs an ACL, add its VID:PID to
   the plugin's HID/USB/SMBus declarations. Use `halod udev-rules` to print the
   assembled baseline and installed-plugin rules.

The authoritative authoring references are:

- [Plugin overview](plugins.md)
- [Manifest reference](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/manifest-reference.md)
- [Lua API and test harness](https://github.com/TimP4w/HaloDaemon-plugins/blob/main/docs/lua-api.md)
- [Official plugins and protocol examples](https://github.com/TimP4w/HaloDaemon-plugins#plugin-catalog)

Rust changes are appropriate only when a plugin needs a capability or a safe,
scoped transport operation that the daemon does not yet expose. Add that core
primitive here first, then consume it from the Lua plugin.

## Changing the core

Core contributions include:

- adding or changing a device capability shared by plugins, engines, IPC, and
  the GUI;
- implementing a new transport or extending a transport's scoped Lua API;
- changing plugin loading, validation, sandboxing, or repository management;
- working on engines, configuration, discovery, IPC, or the GUI;
- changing the Windows broker or platform-specific hardware-access layer.

Keep hardware wire protocols and vendor-specific behavior in Lua plugins. Core
transports should move bytes or expose narrow typed operations without knowing
about a particular vendor or model.

## Testing and checks

Before submitting a Rust change, run from `src/`:

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Add unit tests for new logic and regressions. Tests should use mocks or recorded
traffic rather than requiring physical hardware. Plugin packages have their own
Lua test harness, documented in the plugin repository.

## Packaging

Linux packaging definitions and local build commands are documented in
[`packaging/README.md`](../packaging/README.md). The Windows installer is built
with [`packaging/windows/build-installer.ps1`](../packaging/windows/build-installer.ps1),
which builds the release binaries, stages their dependencies, and invokes Inno
Setup.
