# Installing on Linux

Pick the section for your distro. All installs ship the `halod` daemon, the `halod-gui` UI, and the udev rules. After installing, read [Runtime dependencies](#runtime-dependencies), [Groups & permissions](#groups--permissions), and [Avoid conflicting software](#avoid-conflicting-software).

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
  # creates the `i2c` group; pick the chipset driver for your board.
  i2c.enable = true;
  i2c.platform = "amd";          # "amd" → i2c-piix4 | "intel" → i2c-i801

  # Motherboard temperature sensors + PWM fan control on Nuvoton NCT677x
  # SuperIO chips (most AMD/Intel consumer boards). Loads nct6775.
  enableNuvotonFanControl = true;

  # GNOME Shell focus-watcher extension (foreground-app detection on Wayland).
  enableGnomeExtension = true;
};

# Required for i2c.enable — grants your user SMBus access.
# Add "halod" too if you use hwmon PWM fan control — the pwm files are
# group-scoped (mode 0664) rather than world-writable.
users.users.<you>.extraGroups = [ "i2c" "halod" ];
```

This installs the binaries and udev rules and runs `halod` as a per-user service. Every option except `enable` defaults to `false`, so enable only what your hardware needs:

| Option | Effect |
|--------|--------|
| `i2c.enable` | Turns on `hardware.i2c.enable` (loads `i2c-dev`, creates the `i2c` group) for SMBus DRAM/GPU RGB |
| `i2c.platform` | Chipset SMBus driver: `"amd"` → `i2c-piix4`, `"intel"` → `i2c-i801` (`null` = load neither) |
| `enableNuvotonFanControl` | Loads `nct6775` for NCT677x SuperIO temperature sensors and PWM fan headers (no-op if the chip is absent) |
| `enableGnomeExtension` | Installs the GNOME Shell extension system-wide (each user still runs `gnome-extensions enable halod@halod`) |

To try without installing: `nix run github:TimP4w/HaloDaemon`.

## Ubuntu / Debian

Download `halod_<version>_amd64.deb` from the [releases page](https://github.com/TimP4w/HaloDaemon/releases) and install it:
```bash
sudo apt install ./halod_*.deb
```
The package installs both binaries, the udev rules, and a desktop entry, pulls in the runtime libraries automatically, and creates the `halod` group. `ffmpeg` (LCD video) is a recommended dependency; `nvidia-utils` and `i2c-tools` are suggested. Then join the groups you need (see [Groups & permissions](#groups--permissions)).

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
sudo cp udev/60-halod.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
sudo udevadm trigger
```

## Groups & permissions

USB peripherals (HID, raw USB, uinput key remapper) are granted to the user of the active local session via the `uaccess` tag, so no group membership is needed for them. Two groups cover the rest — the `.deb`/`.rpm`/Arch packages create `halod` for you; on a tarball install add yourself to whichever you need (log out and back in afterwards):
```bash
sudo usermod -aG halod $USER   # motherboard PWM fan control via hwmon
sudo usermod -aG i2c $USER     # SMBus/DRAM + GPU RGB (ASUS/ENE, Corsair DRAM)
```
For motherboard PWM fan control the udev rules scope the hwmon `pwm` files to the `halod` group (mode 0664) on device add.
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

Chipset SMBus access needs the `i2c-dev` module plus your platform's bus driver (`i2c-piix4` on AMD, `i2c-i801` on Intel). Load them and join the `i2c` group above.

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
