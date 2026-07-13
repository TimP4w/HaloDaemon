# Windows privilege separation

On Windows, HaloDaemon needs elevated access for a small set of low-level
register-bus operations — chipset SMBus (DRAM/GPU RGB), SuperIO port I/O
(motherboard fan control and temperatures), and the AMD Ryzen on-die SMN
thermal registers — all reached through the PawnIO kernel driver. Rather than
run the whole application elevated, that privileged surface is isolated into a
single tiny binary, **`halod-broker.exe`**, and everything else runs at the
user's normal (medium) integrity.

This document is the design and threat model for that split. For the day-to-day
developer workflow (dev-run UAC prompt, `HALOD_NO_BROKER`, installer testing)
see [development.md](development.md#windows-privilege-separation).

## Process topology

```
 halod-gui.exe        user, medium integrity   — the UI + tray; launches the worker
      │  (named pipe \\.\pipe\halod, framed JSON)
      ▼
 halod.exe            user, medium integrity   — the daemon/worker: device I/O,
      │                                           engines, config, the Lua plugin host
      │  (named pipe \\.\pipe\halod-broker, framed JSON — proto.rs)
      ▼
 halod-broker.exe     LocalSystem (elevated)   — register-bus RPC only
```

`halod.exe` is **never** elevated and is **not** a service. The GUI launches it
as a plain user process. The first time the daemon touches a register bus it
brings the broker up:

- **Installed:** the on-demand `HalodBroker` LocalSystem service is started via
  the SCM (no UAC prompt), and self-stops once idle. See
  [broker/src/service.rs](../src/broker/src/service.rs).
- **Dev run (no service):** the daemon `ShellExecuteExW("runas")`-launches
  `halod-broker.exe`, producing **one** UAC prompt.

The broker links only `halod-hwaccess` + `windows`
([broker/Cargo.toml](../src/broker/Cargo.toml)) — no Lua runtime, no network
stack, no `halod` code. That narrowness is the point: the elevated binary can
reach register buses and nothing else.

## The RPC surface

The daemon↔broker protocol ([hwaccess/src/proto.rs](../src/hwaccess/src/proto.rs))
is deliberately just the register-bus primitives:

- SMBus: enumerate controllers, open a bus, and the byte/word/block
  read/write ops — each addressed by a broker-side **handle** returned at open.
- PawnIO: open a module into a handle, then execute a named function against
  that handle.

There is **no** filesystem op, process-spawn, or generic "run this" verb. Frames
are a `u32` length prefix + JSON, capped at `MAX_FRAME_LEN` (64 KiB) so a hostile
length prefix cannot force a huge allocation.

## Trust boundaries

| Principal | Trusted? | Enforced how |
|---|---|---|
| `halod.exe` daemon | **Yes** | — |
| `halod-broker.exe` | **Yes** | — |
| Lua plugins (run inside the daemon) | **No** | daemon-layer `smbus` permission gate |
| Other interactive users on the box | **No** | **the broker pipe (this document)** |

Two consequences follow, and they are the crux of the model:

1. **The daemon and the broker trust each other.** They are indistinguishable at
   the RPC layer — a request that reached the pipe came from a process running as
   the coordinating user, which is exactly what the daemon is. The split is about
   *surface area and auditability* (a small, logged, elevated binary), **not** a
   boundary against a fully compromised daemon. So the broker does not, and
   cannot usefully, re-authorize individual operations: any allowlist the daemon
   *sent* would be worthless (a compromised daemon sends a permissive one), and
   the legitimate set of buses/addresses lives in the daemon's device+plugin
   layer, which the broker intentionally does not link.

2. **Plugins are the untrusted code, and they are constrained at the daemon, not
   the broker.** A plugin that wants SMBus access must hold the `smbus`
   permission, and its manifest declares the buses/addresses it uses. Tightening
   that further (broker-enforced per-address capabilities) was considered and
   **deliberately not done** — it would duplicate the device layer into the
   elevated process, still could not cover plugin-declared addresses, and buys
   nothing given (1). If the plugin permission model ever needs to become a hard
   boundary, that work belongs in the daemon's plugin host.

The boundary the **broker** actually enforces is the last row: keeping a
*different* interactive user off the elevated pipe.

## Pipe security

The broker's named pipe is protected two ways, in depth.

### 1. DACL

The pipe is created with the protected DACL `D:P(A;;GA;;;IU)(A;;GA;;;SY)`
([hwaccess/src/winsec.rs](../src/hwaccess/src/winsec.rs)) — `GENERIC_ALL` to the
well-known **Interactive** group (`IU`) and LocalSystem (`SY`), and nobody else.
`S-1-5-4` (Interactive) appears only in an interactive-logon token, never in a
session-0 service token or a network/batch logon, so this alone keeps out
session-0 and remote/network callers. It does **not**, by itself, distinguish
one interactive user from another.

### 2. Coordinator binding (per-connection authentication)

Because `IU` grants *every* interactive user, on a multi-session box
(fast-user-switching, RDP) a second logged-in user could otherwise connect to
the elevated pipe and issue raw register writes. To close that, the broker
authenticates every connection
([broker/src/clientauth.rs](../src/broker/src/clientauth.rs)):

1. On accept, it `ImpersonateNamedPipeClient`s the caller, reads the token's
   **user SID** and **logon-session id**, then `RevertToSelf`.
2. The **first** identity seen becomes "the coordinator" and is bound.
3. Every later connection must match that SID **and** session, or it is refused
   (its stream is dropped, disconnecting it) and logged.
4. When the last connection drops (the daemon has exited and the broker is
   idle), the binding is released, so a broker started fresh later can be claimed
   by whichever user's daemon next brings it up.

The impersonate→read→revert FFI is kept small; the admission decision itself is a
pure function so its coordinator/cap invariants are unit-tested without a live
pipe.

### 3. Service-control grant

So the non-elevated daemon can start the on-demand service without a UAC prompt,
the installer grants interactive users **SERVICE_START + query only**
(`(A;;RPLC;;;IU)`, [broker/src/service.rs](../src/broker/src/service.rs)) — not
`SERVICE_STOP`. The broker self-stops when idle, so no unprivileged caller needs
to stop it, and withholding STOP prevents one interactive user from stopping
another's in-use broker. Even a user permitted to *start* the service still
cannot *connect* unless they are the bound coordinator.

## Resource bounds

A LocalSystem service must not let a peer exhaust it
([broker/src/server.rs](../src/broker/src/server.rs)):

- **Connections** are capped (`MAX_CLIENTS`); excess accepts are refused.
- **Per-connection handles** (open buses / PawnIO modules) are capped
  (`MAX_HANDLES_PER_KIND`), so one connection cannot grow its handle maps without
  bound.
- **Handle-id allocation** is checked (`checked_add`), never wrapping.
- **Idle self-stop:** with no live connection for a grace period the service
  stops, so the elevated helper never lingers after its worker is gone.

Per-request rate limiting and per-connection idle timeouts are intentionally
**not** applied: the daemon holds its bus handles open for the whole session and
streams RGB at a high rate by design, and it is trusted,
so those would throttle or sever legitimate traffic while adding nothing against
the one untrusted principal the pipe already refuses.
