<!--
SPDX-License-Identifier: GPL-3.0-or-later
-->

# Windows privilege separation (register-bus broker)

On Linux `halod` runs as the logged-in user; nothing is privileged, and SMBus
goes through `/dev/i2c-*` gated by the `i2c` group (udev rules), not elevation.
On Windows a subset of transports need Administrator rights — PawnIO (chipset
SMBus and SuperIO fan control), AMD SMN, and the Windows SMBus register
backends. Before this change **everything** ran elevated because it shared one
process with those transports: HID, the TCP/network plugin transport, the Lua
plugin sandbox, engines and the GUI IPC included.

This split confines elevation to one small, separate binary — `halod-broker.exe`
— that does exactly one thing (serve register-bus RPC) and **cannot run Lua**
(it does not link `mlua`, the network types, or `halod`). Everything that can
execute plugin/Lua/network code runs at the user's normal integrity level.

## Threat model

- **Before:** a Lua sandbox escape or a malicious plugin inside the daemon had
  full Administrator rights.
- **After:** the same escape runs at medium integrity. It can still *ask* the
  broker to perform register-bus operations (the daemon and a compromised plugin
  are indistinguishable at the RPC layer — same process), so this is **not** a
  sandbox around register access. It is a reduction of blast radius for
  everything that isn't register access (filesystem, other processes, network,
  credential theft, persistence), plus a smaller, auditable elevated surface with
  a **logged** RPC boundary. It is not a security boundary against a fully
  compromised worker.

## Process topology

```
halod-gui.exe            User frontend, medium integrity. Autostarts (its own
   │                      HKCU Run toggle) — typically living in the tray.
   │  A background IPC thread keeps the daemon up: it spawns a sibling halod.exe
   │  whenever it can't connect (independent of any window), so a tray-only GUI
   │  still runs and restarts the daemon.
   ▼
halod.exe                Plain user token, MEDIUM integrity, interactive session.
   │  HID, TCP/network, Lua plugins, engines, GUI IPC, screen capture.
   │  NEVER elevated, NEVER a service, no supervisor.
   │  On first register-bus access it brings the broker up:
   │    installed → StartService(HalodBroker) via the SCM (granted right, no UAC)
   │    dev run   → ShellExecuteExW("runas") UAC spawn of halod-broker.exe
   ▼ (named pipe \\.\pipe\halod-broker, DACL = Interactive users + SYSTEM)
halod-broker.exe --service   On-demand LocalSystem service. ONE job: register RPC.
                             Serves SMBus/PawnIO, logs every op, self-stops when
                             its last client disconnects. Links halod-hwaccess +
                             windows only — no mlua/network/halod.
```

There is **no supervisor** and **no session bridge**. Because the broker is
LocalSystem it does register access directly, so the user's *linked/elevated*
token is never needed. The worker exists only at medium integrity, launched by
the (also medium-integrity) GUI. `halod.exe` has no `--service`/`--worker`
roles at all — the only knob is `--headless` (opt out of idle-shutdown).

## Crates

- **`halod-hwaccess`** — the raw privileged primitives, shared by the daemon and
  the broker: the `SmBusSyncOps` trait + platform SMBus backends
  (Linux i2c-dev / Windows chipset+NvAPI / fallback), the `PawnioModule` /
  `PawnioOps` bridge, the `winsec` pipe-DACL helper, the `BROKER_SERVICE_NAME`
  constant, and the `proto` wire protocol. Cross-platform; consumed in-process by
  `halod` on Linux and by `halod-broker` on Windows.
- **`halod-broker`** — Windows-only binary; the on-demand LocalSystem service.
- **`halod`** — the daemon/worker. Obtains register-bus ops through
  `drivers/transports/register_ops.rs`, which resolves to either the direct
  in-process `halod-hwaccess` impl (Linux, or `HALOD_NO_BROKER=1`) or the broker
  RPC client (Windows).

## The seam: `register_ops`

| Situation | Backend |
|-----------|---------|
| Linux (any build) | Direct in-process (`/dev/i2c-*` permissions, no elevation) |
| Windows, `HALOD_NO_BROKER=1` | Direct in-process (monolithic/dev escape hatch) |
| Windows | Broker RPC client; brings the broker up via the SCM (installed) or one UAC prompt (dev) |

The trait boundaries are unchanged (`SmBusSyncOps` for a bus, `PawnioOps` for a
PawnIO module), so device drivers, discovery probes and the SMBus batch runner
are agnostic to which backend they got. Enumeration (WMI, NvAPI) is *not*
privileged and stays in-process in the worker; only opening a bus and the
register reads/writes go through the broker.

## The broker RPC

- **Transport:** a named pipe (`\\.\pipe\halod-broker`) created with a protected
  DACL granting only the well-known **Interactive** group (`S-1-5-4`) + SYSTEM
  (`winsec::interactive_dacl_sddl`). Interactive-logon tokens carry `S-1-5-4`, so
  session-0 services and network logons are excluded — a static rule that works
  before any user has logged in and needs no per-session SID query. (This is not
  a boundary against the worker; it keeps out session-0/remote callers.)
- **Framing:** a `u32` little-endian length prefix + JSON (`proto::write_frame` /
  `read_frame`), with a `MAX_FRAME_LEN` guard.
- **Surface (`proto::Request`):** `Enumerate` / `EnumerateGpu`, `OpenBus` →
  handle, the `SmBusSyncOps` methods addressed by bus handle, and
  `PawnioOpen` → handle / `PawnioExec` by handle. Nothing else — no filesystem,
  no process spawn, no generic "run this".
- **Handles are per-connection:** each accepted connection is served on its own
  thread with its own bus/module handle tables, dropped (closed) when it ends.
- **On-demand lifecycle:** the service reports Running, serves, and stops on an
  SCM STOP or once idle (no live client) for a grace period — so the elevated
  helper doesn't linger after the worker exits (all connections drop).

### Why PawnIO is handle-based, not name-addressed

The daemon opens **one `LpcIoBus` per detected SuperIO chip**, each depending on
its own PawnIO handle's internal `select_slot` / `find_bars` state. A
name-addressed `pawnio_execute(module, …)` op would collapse every chip onto one
shared broker-side module keyed by `"LpcIO.bin"` and collide that state on
multi-chip boards. So the broker opens a **distinct `PawnioModule` per
`PawnioOpen`**, addressed by handle — mirroring the SMBus `OpenBus` model.

## Verification checklist (on a Windows box)

1. `cargo build --workspace` (includes `halod-hwaccess` + `halod-broker`).
2. Install: the `HalodBroker` service is **demand-start**, LocalSystem, and *not
   running*. Open the GUI → a medium-integrity `halod.exe` appears (Process
   Explorer). Touch an RGB/fan device → `halod-broker.exe` starts (elevated, no
   UAC); register ops work; its op log lands in `halod-broker.log`.
3. HID + OpenRGB/network plugins work in the worker, unaffected.
4. Quit the GUI/tray → the worker exits (idle-shutdown); the broker self-stops
   when the last connection drops.
5. Kill `halod-broker.exe` mid-session → register devices fail gracefully
   (logged), HID/network keep working; the next access restarts it via the SCM.
6. Dev run (`cargo run -p halod`, service not installed) → first register-bus
   access triggers exactly one UAC prompt for `halod-broker.exe`; declining
   leaves HID/network working with a clear log message.
7. Installer: both executables present and correctly signed.

The broker appends its op log to `halod-broker.log` next to the executable.
