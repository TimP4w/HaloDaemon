# LpcIO Transport

Windows LPC I/O port access via PawnIO's `LpcIO.bin` kernel module. Used by the SuperIO fan driver to communicate with Nuvoton NCT677x and ITE SuperIO chips.

**Platform:** Windows only

---

## Overview

Nuvoton NCT677x and ITE SuperIO chips provide motherboard temperature sensors and PWM fan headers. On Windows these chips are accessed through LPC I/O port reads/writes, which require a kernel-mode driver. HaloDaemon uses [PawnIO](https://pawnio.eu/), loaded only by the elevated `halod-broker` process; the daemon remains non-elevated.

---

## Operations

| Method | PawnIO function | Purpose |
|--------|-----------------|---------|
| `select_slot(slot)` | `ioctl_select_slot` | Select SuperIO slot 0 or 1 |
| `find_bars()` | `ioctl_find_bars` | Register the selected chip's runtime I/O window |
| `read_port(port)` | `ioctl_pio_inb` | Read byte from I/O port |
| `write_port(port, val)` | `ioctl_pio_outb` | Write byte to I/O port |
| `superio_inb(register)` | `ioctl_superio_inb` | Read a SuperIO configuration register |
| `superio_outb(register, val)` | `ioctl_superio_outb` | Write a SuperIO configuration register |

These mappings exist only in the broker. The daemon RPC contains typed LPC
requests and never supplies a PawnIO module name, function name, or argument vector.

---

## Supported chips

| Chip | Notes |
|------|-------|
| Nuvoton NCT6775 | Found on many AMD and Intel consumer motherboards |
| Nuvoton NCT6776 | |
| Nuvoton NCT6796 | |
| Nuvoton NCT6798 | |
| Nuvoton NCT6799 | |
| ITE IT8686 | |
| ITE IT8720 | |
| ITE IT8728 | |

---

## Module loading

The typed `OpenLpcIo` request makes the broker locate `PawnIOLib.dll` from an explicit list of absolute paths
(never the bare DLL search path / `%PATH%` / CWD, which would be a hijack into an
elevated process), in order:
- `C:\Program Files\PawnIO\PawnIOLib.dll`
- `%ProgramFiles%\PawnIO\PawnIOLib.dll`
- `%ProgramW6432%\PawnIO\PawnIOLib.dll`

The broker then loads the fixed `LpcIO.bin` module only from beside its executable (these blobs
are executed by the kernel driver, so user-writable locations like the CWD are
deliberately excluded), in order:
- Executable's directory
- `pwnio/` next to the executable

If `LpcIO.bin` is not found, SuperIO fan control is silently unavailable.

---

## Concurrency

A single `LpcIoBus` is created at discovery time and shared across all sensor and fan devices for the same chip via `Arc<Mutex<LpcIoBus>>`. The mutex serialises all port access because SuperIO register reads are stateful (index→data port writes, bank-select writes) and cannot interleave.

---

## Limitations

- Windows only — excluded from Linux builds at the compiler level.
- Requires PawnIO installed; only the on-demand `halod-broker` helper is elevated.
