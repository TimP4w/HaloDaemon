# Linux packaging

Native packages for the four supported Linux families. The install layout is the
same everywhere (mirrors the Nix package): `halod` + `halod-gui` to `/usr/bin`, the
udev rules to `/usr/lib/udev/rules.d/`, the desktop entry and icon to
`/usr/share/`, and an optional (disabled) systemd **user** unit to
`/usr/lib/systemd/user/halod.service`.

| Distro | Format | Definition | Built in CI? |
|--------|--------|------------|--------------|
| Ubuntu / Debian | `.deb` | `[package.metadata.deb]` in `src/daemon/Cargo.toml` | yes (release) |
| Fedora | `.rpm` | `[package.metadata.generate-rpm]` in `src/daemon/Cargo.toml` | yes (release) |
| Arch / CachyOS | `.pkg.tar.zst` | `packaging/arch/PKGBUILD` (+ `halod.install`) | no, build locally |

Both the `.deb` and `.rpm` ship **both** binaries from the one `halod` crate and
detect their runtime library dependencies automatically (`dpkg-shlibdeps` /
rpm `find-requires`). The Arch package is source-built and is intentionally not
produced in CI.

The Nix package, GNOME-extension derivation, and NixOS module live in
`packaging/nix/` (`package.nix`, `gnome-extension.nix`, `module.nix`); the
repo-root `flake.nix` wires them into `packages`/`nixosModules` and keeps only
the dev shell inline. Build with `nix build .#halod`.

## Build locally

All commands run from the repo root.

### Ubuntu / Debian (`.deb`)

```bash
cargo install cargo-deb --locked
# builds the release binaries, then packages them:
cargo deb --manifest-path src/daemon/Cargo.toml
# → src/target/debian/halod_<version>_amd64.deb
sudo apt install ./src/target/debian/halod_*.deb
```

To package binaries you have already built (no rebuild): add `--no-build`.

### Fedora (`.rpm`)

```bash
cargo install cargo-generate-rpm --locked
sudo dnf install -y rpm-build            # for the find-requires dep scanner
cargo build --release -p halod -p halod-gui --manifest-path src/Cargo.toml
cargo generate-rpm -p src/daemon
# → src/target/generate-rpm/halod-<version>-1.x86_64.rpm
sudo dnf install ./src/target/generate-rpm/halod-*.rpm
```

Build dependencies (dnf): `hidapi-devel libusb1-devel pipewire-devel
systemd-devel wayland-devel libxkbcommon-devel dbus-devel clang
pkgconf-pkg-config`, plus `cargo install cargo-about`.

### Arch / CachyOS (`.pkg.tar.zst`)

```bash
cd packaging/arch
makepkg -si          # builds from the tagged source and installs
```

`makepkg` reads `pkgver` from the `PKGBUILD`; bump it to the release you want, or
edit `source=` to point at a branch/commit. `namcap PKGBUILD *.pkg.tar.zst` lints
the result.

## After installing (all distros)

- Fan control via hwmon PWM and plugin-scoped SMBus access need membership of
  the `halod` group (created by the package): `sudo usermod -aG halod $USER`.
- `i2c-tools` is useful for SMBus / DRAM + GPU RGB diagnostics.
- Log out and back in for group changes to apply.
- The GUI spawns the daemon on demand. To run it as a session service instead:
  `systemctl --user enable --now halod.service`.
