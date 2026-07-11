<div align="center">

<img src="./assets/icon.svg" width="80">

# HaloDaemon

A Linux/Windows peripheral control daemon, inspired by SignalRGB.

<img src="./docs/images/home.png" width="640" alt="HaloDaemon home screen">

</div>

Started as a project to learn about HID, in order to remove adware like Aura Sync, G Hub and NZXT Cam, with a first PoC in python, then re-implemented in Rust.

It support my own devices, and some others may be added in the future, mostly following what my friends own.

> [!WARNING]
> **Disclaimer - use at your own risk.** This software communicates directly with low-level hardware interfaces (HID, SMBus/I2C, SuperIO port I/O, etc.). Sending incorrect data to peripherals or your motherboard can cause malfunction, data loss, or **irreparable damage to your peripherals or PC**. It is provided "as is", without warranty of any kind. You assume all responsibility for any damage that results from its use.


## LLM Notice

Claude code was heavily used. The GUI is exclusively done using claude code, the daemon's initial architecture and code was written manually, then iterated over with claude code.

## Features

<table>
<tr>
<td width="50%" valign="top">

**Fan curves** — temperature-based PWM control with hysteresis and failsafe; preset curves (Balanced, Silent, Performance, Full Speed, 50%).

</td>
<td width="50%"><img src="./docs/images/fan.png" alt="Fan curve editor"></td>
</tr>
<tr>
<td width="50%" valign="top">

**RGB canvas engine** — unified loop across all placed zones; effects: static color, breathing, rainbow, screen sampler (mirrors monitor content). See [engines](docs/engines.md).

</td>
<td width="50%"><img src="./docs/images/canvas.png" alt="RGB canvas engine"></td>
</tr>
<tr>
<td width="50%" valign="top">

**Chainable ARGB** — daisy-chain generic ARGB accessories on supported hubs; user-defined zones placed on the canvas.

</td>
<td width="50%"><img src="./docs/images/chain.png" alt="Chainable ARGB layout"></td>
</tr>
<tr>
<td width="50%" valign="top">

**LCD display** — template-based image rendering on LCD panel (frame counter, sensor readouts).

</td>
<td width="50%"><img src="./docs/images/lcd.png" alt="LCD template editor"></td>
</tr>
<tr>
<td width="50%" valign="top">

**Per-led RGB** — full per-led lighting.

</td>
<td width="50%"><img src="./docs/images/rgb.png" alt="Per-LED RGB editor"></td>
</tr>
<tr>
<td width="50%" valign="top">

**Audio-reactive effects & now playing** — RGB effects driven by system audio (beat, level, spectrum, waveform) and an LCD widget showing the current track (MPRIS/GSMTC). See [engines](docs/engines.md).

</td>
<td width="50%"><img src="./docs/images/lighting.png" alt="Audio-reactive lighting"></td>
</tr>
<tr>
<td width="50%" valign="top">

**DPI profiles & onboard profiles** — read/write onboard profile storage; DPI step configuration.

</td>
<td width="50%"><img src="./docs/images/dpi.png" alt="DPI profile editor"></td>
</tr>
<tr>
<td width="50%" valign="top">

**Profiles** — profiles and auto-switch based on focused window.

</td>
<td width="50%"><img src="./docs/images/profiles.png" alt="Profiles list" width="49%"><img src="./docs/images/profile_switch.png" alt="Profile auto-switch" width="49%"></td>
</tr>
<tr>
<td width="50%" valign="top">

**Battery, Control, Eq, and more** — control device quirks.

</td>
<td width="50%"><img src="./docs/images/eq.png" alt="Equalizer control"></td>
</tr>
</table>

- **ChatMix** — for SteelSeries Arctis Nova
- **Key remap** — divert buttons to custom actions (key chord, mouse button, media key, DPI cycle, macro, command, …)

---

## Supported Devices

NZXT Kraken AIOs and Control Hub, Logitech/Razer/Corsair peripherals, SteelSeries Arctis headsets, Philips Evnia monitors, ASUS Aura & SMBus DRAM/GPU RGB, motherboard fans and sensors, and more.

See the full list with VID:PIDs, protocols, and platform support: **[Supported devices](docs/supported-devices.md)**.

---

## Install

- **Linux** (NixOS, Ubuntu/Debian, Fedora, Arch/CachyOS, other distros) — see **[Installing on Linux](docs/install/linux.md)**
- **Windows** — see **[Installing on Windows](docs/install/windows.md)**

Both guides cover the runtime dependencies, permissions/PawnIO setup, and the vendor software (NZXT CAM, iCUE, G HUB, …) you should disable to avoid conflicts.

---

## Further reading

- [Development guide](docs/development.md) — build, add devices, add protocols
- [Device plugins](docs/plugins.md) — add a device in Lua without recompiling
- [Engines](docs/engines.md) — canvas, fan curve, LCD, key remap
- [Protocols](docs/protocols/) — HID++, NZXT, ENE SMBus, DDC/CI, ASUS Aura USB, Corsair, SteelSeries, NCT677x SuperIO, NVIDIA GPU sensors, OpenRGB SDK
- [Transports](docs/transports/) — HID, SMBus, hwmon, LpcIO, USB control, TCP

---

## Acknowledgments

HaloDaemon would not exist without the reverse-engineering work and documentation produced by these open-source projects (see [docs/licenses.md](docs/licenses.md) for how licensing and attribution are tracked across the app):

| Project | License | Used for |
|---------|---------|----------|
| [Solaar](https://github.com/pwr-Solaar/Solaar) | GPL-2.0-or-later | Logitech HID++ protocol (feature pages, onboard profiles, RGB effects) |
| [OpenRGB](https://gitlab.com/CalcProgrammer1/OpenRGB) | GPL-2.0-or-later | ENE SMBus, ASUS Aura USB, Corsair DRAM, Corsair NXP peripheral, Zotac GPU protocols |
| [liquidctl](https://github.com/liquidctl/liquidctl) | GPL-3.0 | NZXT Kraken protocol |
| [Linux kernel nzxt-smart2](https://github.com/torvalds/linux/blob/master/drivers/hwmon/nzxt-smart2.c) | GPL-2.0-or-later | NZXT Control Hub protocol |
| [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) | MPL-2.0 | NCT677x SuperIO register map, AMD Ryzen (Zen) SMN thermal decode |
| [OpenRazer](https://github.com/openrazer/openrazer) | GPL-2.0-or-later | Razer protocol (matrix RGB, DPI) |
| [linux-arctis-manager](https://github.com/elegos/Linux-Arctis-Manager) | GPL-3.0 | SteelSeries Arctis protocol |
| [evnia](https://github.com/tomasf/evnia) | MIT | Philips Evnia Ambiglow protocol |
| [g560-led](https://github.com/mijoe/g560-led) | MIT | Logitech G560 protocol |
| [PawnIO modules](https://github.com/namazso/PawnIO_modules) | LGPL-2.1-or-later | Bundled `LpcIO.bin`, `SmbusI801.bin`, `SmbusPIIX4.bin`, `AMDFamily17.bin` for Windows SuperIO / chipset SMBus / AMD SMN access (© 2023 namazso) |

## License

```
HaloDaemon
Copyright (C)  2026 TimP4w and contributors

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
GNU General Public License for more details.

You should have received a copy of the GNU General Public License
along with this program.  If not, see <http://www.gnu.org/licenses/>.
```
