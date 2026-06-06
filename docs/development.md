# Development Guide

## Requirements

### Preferred: Nix

```bash
nix develop
```

This provides the full toolchain: Rust, GCC, pkg-config, clang/libclang, D-Bus, GTK4, Libadwaita, HIDAPI, I2C tools, libusb1, PipeWire, PulseAudio, Wayland libs.

### Manual (Linux)

Install: `cargo`, `rustc`, `gtk4`, `libadwaita`, `hidapi`, `libusb1`, `pkg-config`, `clang`, `dbus`, `pipewire`, `udev`, `wayland`.

### Windows (MSYS2 UCRT64)

Open the **MSYS2 UCRT64** shell and install:

```bash
pacman -S --needed \
  mingw-w64-ucrt-x86_64-gtk4 \
  mingw-w64-ucrt-x86_64-libadwaita \
  mingw-w64-ucrt-x86_64-gettext \
  mingw-w64-ucrt-x86_64-libxml2 \
  mingw-w64-ucrt-x86_64-librsvg \
  mingw-w64-ucrt-x86_64-pkgconf \
  mingw-w64-ucrt-x86_64-gcc \
  mingw-w64-ucrt-x86_64-rust \
  git
```

Use `ucrt-x86_64` packages (not the older `x86_64` MSVCRT ones). Do not mix MSVC-toolchain Rust with MSYS2's MinGW-built GTK.

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

---

## Project layout

Three crates under `src/`:

| Crate | Purpose |
|-------|---------|
| `halod` | Background device I/O, engine loops, config persistence |
| `halod-gui` | GTK4 GUI with system tray |
| `halod-protocol` | Shared wire types (IPC messages) |

The daemon and UI communicate over a local IPC channel, a Unix domain socket (`$XDG_RUNTIME_DIR/halod.sock`) on Linux, a named pipe (`\\.\pipe\halod`) on Windows — using binary-framed JSON messages.

Configuration lives in `~/.config/halod/config.yaml` (Linux) or `%APPDATA%\halod\config.yaml` (Windows).

---

## Adding a new device

### 1. Create the device file

`src/daemon/src/drivers/vendors/<vendor>/devices/<model>.rs`

Implement the `Device` trait. Declare capabilities in `fn capabilities()` — the list of `Capability` variants the device supports (e.g. `Capability::Rgb`, `Capability::Fan`, `Capability::Battery`). Each capability variant has a matching accessor (`as_rgb()`, `as_fan()`, etc.) that returns a trait object.

See any existing device (e.g. `nzxt/devices/kraken.rs`) for a complete example.

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

One rule per VID:PID for HID devices:
```
KERNEL=="hidraw*", ATTRS{idVendor}=="<vid>", ATTRS{idProduct}=="<pid>", TAG+="uaccess", GROUP="input", MODE="0660"
```

For USB control-transfer devices use `SUBSYSTEM=="usb"` instead. Keep rules grouped by vendor with a comment above each group. All PIDs in lowercase hex. Verify with:
```bash
sudo udevadm verify udev/60-halod.rules
```

### 5. Add a docs page

Add a page under `docs/protocols/` (if a new protocol) and/or reference the device in the supported-devices table in `README.md`.

---

## Adding a new protocol

1. Create `src/daemon/src/drivers/vendors/<vendor>/protocols/<name>.rs`.
2. Implement frame builders (requests) and response parsers as plain functions or a small struct. Keep protocol logic separate from device state.
3. Wire to the transport: take a `HidTransport`, `SmbusTransport`, or `UsbControlTransport` reference in the device struct and call it from the `Device` trait implementation.
4. Add a page to `docs/protocols/` covering the frame format, key commands, credits/references, and known limitations.

---

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

For tests that need a device or hub trait object, create a minimal `MockDevice`/`MockHub` struct inside the test module. See `f_fan.rs` (`src/daemon/src/drivers/vendors/nzxt/devices/f_fan.rs`) or `ipc/serializer.rs` for examples.

Do not write tests that require real hardware, open sockets, or touch the filesystem.
