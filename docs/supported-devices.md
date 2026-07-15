# Supported Devices

HaloDaemon ships with support to the following devices by default. More devices are however provided by the [official plugin](https://github.com/TimP4w/HaloDaemon-plugins) repository and are not listed here.

🐧 = Linux, 🪟 = Windows.

## Fans & Controllers

| Vendor | Model | VID:PID | Protocol | Transport | Platform |
|--------|-------|---------|----------|-----------|----------|
| (All) | Motherboard fan headers | — | sysfs | [hwmon](transports/hwmon.md) | 🐧 |
| (All) | Motherboard fan headers (NCT677x) | — | [Nuvoton plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/nuvoton_lpcio) | LPCIO plugin transport | 🪟 |

## Mice

| Vendor | Model | VID:PID | Protocol | Transport | Platform |
|--------|-------|---------|----------|-----------|----------|
| Logitech | G502 X Plus (wired | wireless) | 046d:c095 | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |
| Logitech | G502 Hero (wired) | 046d:c08b | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |

## Keyboards

| Vendor | Model | VID:PID | Protocol | Transport | Platform |
|--------|-------|---------|----------|-----------|----------|
| Logitech | G PRO X TKL (wired | wireless) | 046d:c352 | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |

> Wireless G502 X Plus and G PRO X TKL connect through the Logitech Lightspeed Receiver (`046d:c547`), which proxies HID++ to the paired device.

## Headsets

| Vendor | Model | VID:PID | Protocol | Transport | Platform |
|--------|-------|---------|----------|-----------|----------|
| Logitech | PRO X Wireless Gaming Headset | 046d:0aba | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |
| Logitech | PRO X 2 LIGHTSPEED | 046d:0af7 | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |
| Logitech | G733 LIGHTSPEED | 046d:0ab5 \| 0afe | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |
| Logitech | G535 LIGHTSPEED | 046d:0ac4 | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |
| Logitech | G935 | 046d:0a87 | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |
| Logitech | G533 | 046d:0a66 | [Logitech plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech) | [HID](transports/hid.md) | 🐧🪟 |

## Speakers

| Vendor | Model | VID:PID | Protocol | Transport | Platform |
|--------|-------|---------|----------|-----------|----------|
| Logitech | G560 Gaming Speaker | 046d:0a78 | [G560 plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/logitech_g560) | [HID](transports/hid.md) | 🐧🪟 |

## Sensors

| Source | VID:PID | Transport | Platform |
|--------|---------|-----------|----------|
| CPU / motherboard temperatures | — | [hwmon](transports/hwmon.md) | 🐧 |
| Motherboard temperatures (NCT677x) | — | [Nuvoton plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/nuvoton_lpcio) | 🪟 |
| AMD Ryzen CPU temperatures (Zen 17h/19h/1Ah) | — | [AMD SMN plugin](https://github.com/TimP4w/HaloDaemon-plugins/tree/main/amd_smn) | 🪟 |
| GPU temperatures (NVIDIA) | — | NvAPI (🪟) / `nvidia-smi` (🐧) | 🐧🪟 |

## Computer (PC / OS)

| Source | VID:PID | Transport | Platform |
|--------|---------|-----------|----------|
| Power profile (performance / balanced / power saver) | — | [computer](transports/computer.md) | 🐧🪟 |
| Host metrics (CPU load, memory, frequency, uptime) | — | [computer](transports/computer.md) | 🐧🪟 |
| Keep awake (inhibit idle/sleep) | — | [computer](transports/computer.md) | 🐧🪟 |
