# Installing on Linux

Pick the section for your distro. All installs ship the `halod` daemon, the `halod-gui` UI, and the base udev rules; installed plugins contribute their own rules dynamically. After installing, read [Runtime dependencies](#runtime-dependencies), [Groups & permissions](#groups--permissions), and [Avoid conflicting software](#avoid-conflicting-software).

## NixOS

The flake exposes a NixOS module:

```nix
# flake.nix
inputs.halod.url = "github:TimP4w/HaloDaemon";

# configuration.nix
imports = [ inputs.halod.nixosModules.default ];

programs.halod = {
  enable = true;

  # SMBus / DRAM + GPU RGB (ASUS/ENE, Corsair DRAM). Loads i2c-dev and
  # exposes the device nodes; pick the chipset driver for your board.
  i2c.enable = true;
  i2c.platform = "amd";          # "amd" → i2c-piix4 | "intel" → i2c-i801

  # Motherboard temperature sensors + PWM fan control on Nuvoton NCT677x
  # SuperIO chips (most AMD/Intel consumer boards). Loads nct6775.
  enableNuvotonFanControl = true;

  # GNOME Shell focus-watcher extension (foreground-app detection on Wayland).
  enableGnomeExtension = true;
};

# Grants access to plugin-scoped SMBus nodes and hwmon PWM controls.
users.users.<you>.extraGroups = [ "halod" ];
```

This installs the binaries and base udev rules and runs `halod` as a per-user service. Every option except `enable` defaults to `false`, so enable only what your hardware needs:

| Option | Effect |
|--------|--------|
| `i2c.enable` | Turns on `hardware.i2c.enable` (loads `i2c-dev`) for SMBus DRAM/GPU RGB; generated rules grant matching nodes to `halod` |
| `i2c.platform` | Chipset SMBus driver: `"amd"` → `i2c-piix4`, `"intel"` → `i2c-i801` (`null` = load neither) |
| `enableNuvotonFanControl` | Loads `nct6775` for NCT677x SuperIO temperature sensors and PWM fan headers (no-op if the chip is absent) |
| `enableGnomeExtension` | Installs the GNOME Shell extension system-wide (each user still runs `gnome-extensions enable halod@halod`) |

To try without installing: `nix run github:TimP4w/HaloDaemon`.

## Ubuntu / Debian

Download `halod_<version>_amd64.deb` from the [releases page](https://github.com/TimP4w/HaloDaemon/releases) and install it:
```bash
sudo apt install ./halod_*.deb
```
The package installs both binaries, the base udev rules, and a desktop entry, pulls in the runtime libraries automatically, and creates the `halod` group. `ffmpeg` (LCD video) is a recommended dependency; `nvidia-utils` and `i2c-tools` are suggested. Then join the groups you need (see [Groups & permissions](#groups--permissions)).

## Fedora

Download `halod-<version>-1.x86_64.rpm` from the [releases page](https://github.com/TimP4w/HaloDaemon/releases) and install it:
```bash
sudo dnf install ./halod-*.rpm
```
Runtime libraries resolve automatically and `ffmpeg` is pulled in as a weak dependency. The package creates the `halod` group via `systemd-sysusers`.

## Arch / CachyOS

Build and install with `makepkg` (CachyOS uses the same package):
```bash
cd packaging/arch
makepkg -si
```
See [`packaging/README.md`](../../packaging/README.md) for details and optional dependencies.

## Other distros

Download `halod-linux-x64.tar.gz` from the [releases page](https://github.com/TimP4w/HaloDaemon/releases), or build from source — see the [development guide](../development.md).

The tarball install does **not** set up the udev rules, groups, or kernel modules automatically — do that manually below.

### Runtime dependencies

| Package | Notes |
|---------|-------|
| hidapi | USB HID communication |
| libusb1 | Raw USB transfers |
| pipewire / pulseaudio | Audio capture (audio-reactive effects, screen capture) |
| libudev | Device discovery |
| wayland / libxkbcommon / libGL | GUI windowing + rendering |
| dbus | Foreground app detection; MPRIS now-playing metadata |
| ffmpeg | LCD video playback (optional) |
| nvidia-smi | GPU temperatures (optional; shipped with the NVIDIA proprietary driver) |
| i2c-tools | SMBus bus probing / debugging (optional) |

### udev rules

Required — without these the daemon needs root:
```bash
halod udev-rules | sudo tee /etc/udev/rules.d/60-halod.rules >/dev/null
sudo udevadm control --reload-rules
sudo udevadm trigger
```

Official release packages generate this file from the exact signed plugin
bundle embedded in that release. Run the same command again after installing,
removing, or updating plugins. The Plugins screen reports when the effective
installed file is stale and can copy these manual installation commands.

## Groups & permissions

USB peripherals (HID, raw USB, uinput key remapper) are granted to the user of the active local session via the `uaccess` tag, so no group membership is needed for them. The `halod` group covers hwmon and plugin-scoped SMBus access; the `.deb`/`.rpm`/Arch packages create it for you. On a tarball install, create it and add yourself (then log out and back in):
```bash
sudo groupadd -f halod
sudo usermod -aG halod $USER
```
For PWM fan control the udev rules scope every hwmon device's `pwm1`–`pwm7`
and corresponding `_enable` files to the `halod` group on device add. This is
independent of the kernel driver's chip name.
After adding the user to `halod`, log out and back in, then repair permissions
for already-present devices with:

```bash
sudo udevadm control --reload-rules
sudo udevadm trigger --action=change --subsystem-match=hwmon
```

Install and enable the official **Linux Hardware Monitoring** integration, then
approve its hwmon permission. Sensors and fan headers are no longer built into
the daemon and remain absent until the integration is enabled.

### Motherboard fans & sensors (NCT677x)

If your board uses a Nuvoton NCT677x SuperIO chip, load the kernel module:
```bash
sudo modprobe nct6775
```
Add it to `/etc/modules-load.d/` to persist across reboots.

### SMBus DRAM / GPU RGB

Chipset SMBus access needs the `i2c-dev` module plus your platform's bus driver (`i2c-piix4` on AMD, `i2c-i801` on Intel). Generated plugin rules grant only matching adapters to the `halod` group.

### Screen capture (canvas screen sampler)

Ensure `xdg-desktop-portal` is running:
```bash
systemctl --user enable --now xdg-desktop-portal
```

## Avoid conflicting software

HaloDaemon talks directly to hardware over HID, SMBus/I2C, and SuperIO port I/O. If another RGB/peripheral tool is running at the same time, the two will fight over device access — you'll see flicker, dropped writes, effects reverting, or a device failing to be claimed.

Stop and disable any other daemon that manages the same hardware before running HaloDaemon.
You can disable single devices on HaloDaemon if you prefer to use another program to control them.

If a device won't appear, check that nothing else holds it (`sudo lsof` on the hidraw/i2c node) and that the kernel module and udev rules above are in place.
