# Installing on Windows

## Installer

Download `halod-setup-x64.exe` from the [releases page](https://github.com/TimP4w/HaloDaemon/releases) and run it. The installer:

- registers the on-demand privileged register-bus broker as a Windows service,
- installs the `halod-gui` UI,
- bundles `ffmpeg` (LCD video) and the PawnIO kernel driver blobs.

The UI can be launched in the background (without showing a window) via `halod-gui --background` for autostart entries.

No separate runtime dependencies need to be installed; the required libraries and `ffmpeg` ship inside the installer.

## PawnIO (required for DRAM RGB and motherboard fan control)

Chipset SMBus access (ASUS/ENE DRAM RGB, Corsair DRAM RGB) and SuperIO fan control (NCT677x temperature sensors and PWM headers) require the [PawnIO](https://pawnio.eu/) signed kernel driver. PawnIO provides safe, signed port I/O from user space without needing full kernel patches. The driver modules are bundled with the installer.

Installed builds start the on-demand `halod-broker` LocalSystem service when register-bus access is first needed; `halod.exe` itself is never elevated. Development builds without the service launch only `halod-broker.exe` through UAC. Declining that development prompt is non-fatal, but chipset SMBus, AMD SMN temperatures, and SuperIO devices are unavailable.

Devices that talk over plain USB HID (AIOs, mice, keyboards, headsets, ASUS Aura USB, monitors) work without PawnIO.

## Avoid conflicting software

HaloDaemon talks directly to hardware over HID, SMBus/I2C, and SuperIO port I/O. If a vendor RGB/monitoring app is running at the same time, the two will fight over device access: expect flicker, effects snapping back, dropped writes, or a device that won't be claimed. SMBus and SuperIO in particular must not be driven by two programs at once.

Fully **quit and disable autostart** (or uninstall) any of the following that manage your hardware before running HaloDaemon:

- **NZXT CAM** - Kraken AIOs, Control Hub.
- **Corsair iCUE** - DRAM RGB, keyboards; also holds the SMBus.
- **ASUS Armoury Crate / Aura Sync / AI Suite** - Aura USB controllers, DRAM, SuperIO fan/sensor access.
- **Logitech G HUB / Logitech Options** - Logitech mice, keyboards, headsets, speakers.
- **Razer Synapse** - Razer mice/keyboards.
- **SteelSeries GG (Engine)** - Arctis headsets.
- **SignalRGB, MSI Center, Gigabyte RGB Fusion, Zotac Firestorm** - general RGB/SMBus tools that grab the same buses.
- **HWiNFO / other SuperIO monitoring** - if it's polling the NCT677x chip it can collide with fan control; close it or disable its SuperIO access.

Disable these from their own settings (turn off "start with Windows") and via **Task Manager → Startup**, then reboot so nothing re-claims the buses before HaloDaemon starts.

If a device won't appear, make sure no vendor service is still running in the background (Task Manager → Services / Details) and that the broker service can start (or, in a development run, that you accepted its UAC prompt).
