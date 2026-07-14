# Computer (PC / OS) Device

A synthetic device representing the host PC / OS itself, aggregating PC-generic /
OS-specific features under one device. Today it exposes:

- **Power profile** (`Choice`) — performance / balanced / power saver.
- **Host metrics** (`Sensors`) — CPU load, memory usage, CPU frequency, uptime.
- **Keep awake** (`Boolean`) — inhibit idle/sleep while on.

**Platform:** Linux and Windows 10+

---

## Overview

Unlike the hardware transports, this one moves no bytes over a bus. It wraps the
operating system's own interfaces so the host itself shows up in HaloDaemon as a
device. There is no VID:PID and no udev rule — these OS APIs need no broker access,
which is all the platform mechanisms need.

The device (`ComputerDevice`) and its capabilities live in
[`daemon/src/drivers/vendors/generic/devices/computer/`](../../src/daemon/src/drivers/vendors/generic/devices/computer/mod.rs),
with each feature in its own submodule (`power_profile/`, `metrics/`, `keep_awake/`).

---

## Discovery

A `TransportScanner` runs during normal discovery and registers the device (stable
id `computer`) on Linux/Windows. Host metrics are always available there, so the
device is always present; individual features (like the power profile) hide
themselves when the host has no matching interface.

---

## Power profile

Three canonical profiles are exposed — `performance`, `balanced`, `power-saver`.

- **Linux** — talks to [power-profiles-daemon](https://gitlab.freedesktop.org/upower/power-profiles-daemon)
  over the system D-Bus, reading/writing the `ActiveProfile` property (its profile
  strings match our canonical ids 1:1). Both the current
  (`org.freedesktop.UPower.PowerProfiles`) and legacy (`net.hadess.PowerProfiles`)
  bus names are tried, falling back to the `powerprofilesctl get` / `set` CLI. The
  Choice is only shown when one of these is present.
- **Windows** — uses `powercfg` (Windows 10+): `powercfg /getactivescheme` to read
  and `powercfg /setactive <guid>` to switch, mapping the profiles to the standard
  High performance / Balanced / Power saver plan GUIDs (`SCHEME_MIN` /
  `SCHEME_BALANCED` / `SCHEME_MAX`).

## Host metrics

Polled every 2 s into read-only sensors: CPU load (%), memory (% used, with used/
total GB in the label), CPU frequency (MHz), and uptime (h).

- **Linux** — parses `/proc/{stat,meminfo,cpuinfo,uptime}`.
- **Windows** — queries WMI (`Win32_PerfFormattedData_PerfOS_Processor`,
  `Win32_OperatingSystem`, `Win32_Processor`, `Win32_PerfFormattedData_PerfOS_System`).

## Keep awake

While enabled, the host is prevented from idling/sleeping.

- **Linux** — holds a systemd-logind `Inhibit` (`idle:sleep`, `block`) file
  descriptor; releasing it drops the lock.
- **Windows** — a dedicated thread holds `SetThreadExecutionState(ES_CONTINUOUS |
  ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)` and clears it when toggled off.
