# Windows privilege separation

On Windows, HaloDaemon needs elevated access for a small set of low-level
register-bus operations — chipset SMBus (DRAM/GPU RGB), SuperIO port I/O
(motherboard fan control and temperatures), and the AMD Ryzen on-die SMN
thermal registers — all reached through the PawnIO kernel driver. Rather than
run the whole application elevated, that privileged surface is isolated into a
single tiny binary, **`halod-broker.exe`**, and everything else runs at the
user's normal (medium) integrity.

This document is the design and threat model for that split. For the day-to-day
developer workflow (dev-run UAC prompt and installer testing), see
[development.md](development.md#windows-privilege-separation).

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

- SMBus: open an enumerated controller and use the byte/word/block
  read/write ops — each addressed by a broker-side **handle** returned at open.
- PawnIO: open a module into a handle, then execute a named function against
  that handle.

There is **no** filesystem op, process-spawn, or generic "run this" verb. Frames
are a `u32` length prefix + JSON, capped at `MAX_FRAME_LEN` (64 KiB) so a hostile
length prefix cannot force a huge allocation. Before any operation, a connection
must exchange the per-process bootstrap secret for a short-lived,
connection-bound capability. An SMBus capability names one exact enumerated bus
and address set. A PawnIO capability names one hard-allowlisted module and exact
function set. Both carry request-rate and total-operation ceilings.

## Trust boundaries

| Principal | Trusted? | Enforced how |
|---|---|---|
| `halod.exe` daemon | **Yes** | — |
| `halod-broker.exe` | **Yes** | — |
| Lua plugins (run inside the daemon) | **No** | daemon-layer `smbus` permission gate |
| Other interactive users on the box | **No** | **the broker pipe (this document)** |

Two consequences follow, and they are the crux of the model:

1. **A fully compromised daemon remains trusted.** It owns the bootstrap secret
   and can request new capabilities within the broker's hard protocol surface.
   The split limits elevated code and prevents unrelated processes/users from
   reaching it; it is not a sandbox for the daemon itself.

2. **Plugins are untrusted.** A plugin must hold the daemon-layer `smbus`
   permission and declare its addresses. The daemon turns the scan job's exact
   addresses and pre-scan scope into the broker capability. The broker therefore
   rejects an accidental or confused-deputy request outside that scope, while a
   daemon compromise can still request a different capability as described in
   (1).

The boundary the **broker** actually enforces is the last row: keeping a
*different* interactive user off the elevated pipe.

## Pipe security

The broker's named pipe is protected two ways, in depth.

### 1. Exact-principal DACL

The daemon resolves its concrete user SID before starting the broker. The pipe
uses the protected DACL `D:P(A;;GA;;;<coordinator SID>)(A;;GA;;;SY)`
([hwaccess/src/winsec.rs](../src/hwaccess/src/winsec.rs)). There is no `IU`,
`AU`, or world ACE, so another interactive account cannot open the pipe at all.

### 2. Coordinator identity and capability authentication

The daemon supplies its SID, Windows session id, and a cryptographically random
32-byte bootstrap secret as transient service-start arguments (or as arguments
to the UAC foreground broker). The broker authenticates every connection
([broker/src/clientauth.rs](../src/broker/src/clientauth.rs),
[broker/src/server.rs](../src/broker/src/server.rs)):

1. On accept, it `ImpersonateNamedPipeClient`s the caller, reads the token's
   **user SID** and **logon-session id**, then `RevertToSelf`.
2. The identity must match the startup SID **and** session; a first-client race
   can never claim an unbound broker.
3. The first RPC frame must carry the startup secret and an exact operation
   scope. The secret comparison is constant-time. Authentication returns a
   random, short-lived capability bound to that connection.
4. Every later operation must be inside the scope and its rate/operation limits,
   or it is refused. Renewal requires the current capability id.
5. A wrong identity disconnects. A wrong secret, expired capability, or
   out-of-scope operation is rejected without executing privileged work.

The impersonate→read→revert FFI is kept small; the admission decision itself is a
pure function so its coordinator/cap invariants are unit-tested without a live
pipe.

### 3. Service-control grant

So the non-elevated daemon can start the on-demand service without a UAC prompt,
the installer grants only the installing user's concrete SID
**SERVICE_START + query only** (`(A;;RPLC;;;<SID>)`,
[broker/src/service.rs](../src/broker/src/service.rs)) — not `SERVICE_STOP`.
Installation and upgrades remove every legacy `IU` ACE before adding the exact
principal. The broker self-stops when idle, so no unprivileged caller needs to
stop it.

## Resource bounds

A LocalSystem service must not let a peer exhaust it
([broker/src/server.rs](../src/broker/src/server.rs)):

- **Connections** are capped (`MAX_CLIENTS`); excess accepts are refused.
- **Named-pipe instances** use the same finite cap; no unlimited-instance pipe
  is created.
- **Worker threads** are retained and joined after completion rather than
  detached and forgotten.
- **Per-connection handles** (open buses / PawnIO modules) are capped
  (`MAX_HANDLES_PER_KIND`), so one connection cannot grow its handle maps without
  bound.
- **Handle-id allocation** is checked (`checked_add`), never wrapping.
- **Operations** are limited per second and per capability; scope values are
  clamped to hard protocol ceilings.
- **Idle clients** are disconnected after `CLIENT_IDLE_TIMEOUT`, releasing all
  connection-owned bus/module handles.
- **Idle self-stop:** with no live connection for a grace period the service
  stops, so the elevated helper never lingers after its worker is gone.
