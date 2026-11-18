# Windows privilege separation

On Windows, HaloDaemon needs elevated access for a small set of low-level
register-bus operations: chipset SMBus (DRAM/GPU RGB), SuperIO port I/O
(motherboard fan control and temperatures), and the AMD Ryzen on-die SMN
thermal registers, all reached through the PawnIO kernel driver. Rather than
run the whole application elevated, that privileged surface is isolated into a
single tiny binary, **`halod-broker.exe`**, and everything else runs at the
user's normal (medium) integrity.

This document is the design and threat model for that split. For the day-to-day
developer workflow (dev-run UAC prompt and installer testing), see
[development.md](development.md#build-and-run).

## Process topology

```
 halod-gui.exe        user, medium integrity   - the UI + tray; launches the worker
      │  (named pipe \\.\pipe\halod, framed JSON)
      ▼
 halod.exe            user, medium integrity   - the daemon/worker: device I/O,
      │                                           engines, config, the Lua plugin host
      │  (named pipe \\.\pipe\halod-broker, framed JSON - proto.rs)
      ▼
 halod-broker.exe     LocalSystem (elevated)   - register-bus RPC only
```

`halod.exe` is **never** elevated and is **not** a service. The GUI launches it
as a plain user process. The first time the daemon touches a register bus it
brings the broker up:

- **Installed:** the on-demand `HalodBroker` LocalSystem service is started via
  the SCM (no UAC prompt), and self-stops once idle. See
  [broker/src/service.rs](../src/broker/src/service.rs).
- **Dev run (no service):** the daemon `ShellExecuteExW("runas")`-launches
  `halod-broker.exe --coordinator-sid=<SID>`, producing **one** UAC prompt.

The broker links only `halod-hwaccess` + `windows`
([broker/Cargo.toml](../src/broker/Cargo.toml)): no Lua runtime, no network
stack, no `halod` code. That narrowness is the point: the elevated binary can
reach register buses and nothing else.

## The RPC surface

The daemon↔broker protocol ([hwaccess/src/proto.rs](../src/hwaccess/src/proto.rs))
is just the register-bus primitives:

- SMBus: open an enumerated controller, then byte/word/block reads and writes
  on the returned **handle**.
- AMD SMN: open a typed handle and read a 32-bit SMN offset.
- LPC I/O: open a typed handle, select a SuperIO slot, find BARs, and perform
  fixed port/config-register byte operations.

There is **no** filesystem, process-spawn, or generic "run this" verb. Frames
are a `u32` length prefix + JSON, capped at 64 KiB. The first frame must be
`Hello { scope }`; the broker replies `Ready` and binds the connection to that
scope for its lifetime. An SMBus scope names one enumerated bus and address
set; every scope carries a request-rate ceiling.

## Trust boundaries

| Principal | Trusted? | Enforced how |
|---|---|---|
| `halod.exe` daemon | **Yes** | - |
| Other code running as the coordinator user | **Same as the daemon** | not distinguishable |
| `halod-broker.exe` | **Yes** | - |
| Lua plugins (run inside the daemon) | **No** | declared address scope plus daemon-layer activation and permission gates |
| Other users on the box | **No** | **the broker pipe and service DACLs** |

1. **A compromised daemon remains trusted.** It can request any scope within
   the protocol. The split limits elevated code; it does not sandbox the daemon.
2. **Same-user code is the daemon.** Any process of the coordinator user can
   inject into `halod.exe` or start the service itself. Windows has no boundary
   between one user's processes in one session, so the broker does not try to
   authenticate the daemon with a secret.
3. **Plugins are untrusted.** SMBus use requires the `smbus` permission and
   exact addresses, checked at activation and again in `pre_scan`. The daemon
   turns them into the connection scope, so the broker rejects anything else.

The boundary the **broker** enforces is the last row: keeping a *different*
user off the elevated pipe.

## Pipe security

### 1. DACL and pipe flags

The pipe DACL is `D:P(A;;GA;;;<coordinator SID>)…(A;;GA;;;SY)`
([hwaccess/src/winsec.rs](../src/hwaccess/src/winsec.rs)), with no `IU`, `AU`,
or world ACE. Instances reject remote clients. When no instance of ours exists,
the next is created with `FILE_FLAG_FIRST_PIPE_INSTANCE`; if the name is taken,
the broker exits rather than serve beside a squatter.

### 2. Coordinator identity

Coordinator SIDs are fixed at broker start, never taken from the client:

- **Installed service:** every account granted `SERVICE_START` in the service's
  own DACL (the installer's ACE, section 4). Aliases are resolved to numeric
  SIDs, so `LA` counts; broad principals (SYSTEM and other NT AUTHORITY
  well-knowns, Everyone, BUILTIN groups, Guest, well-known domain groups) never
  do. With none left the service fails closed. It is started with no arguments.
- **Dev run:** `--coordinator-sid=<SID>` from the UAC-approved command line.

Each client's SID is read from its process token
([broker/src/clientauth.rs](../src/broker/src/clientauth.rs)), without
impersonation, and must be a coordinator. The session is logged, not pinned.

### 3. Image-path pinning (a bar-raiser, not a boundary)

- The installed service requires the client to be `halod.exe` in its own
  directory. Dev runs skip this; a build tree is user-writable.
- The daemon opens the pipe at `SECURITY_IDENTIFICATION` and requires the server
  to be `halod-broker.exe` next to itself before sending `Hello`.

Both directories are admin-only when installed. This stops a renamed copy or a
casual squatter, not same-user code. Either refusal fails the open at once; the
broker sends its reason as an error frame first.

### 4. Service-control grant

The installer grants only the installing user's SID **SERVICE_START + query**
(`(A;;RPLC;;;<SID>)`, [broker/src/service.rs](../src/broker/src/service.rs)),
not `SERVICE_STOP`, and removes legacy `IU` ACEs. The broker self-stops when
idle. This ACE is also the coordinator identity of section 2.

## Broker self-hardening

In service mode ([broker/src/hardening.rs](../src/broker/src/hardening.rs)) the
broker restricts DLL search to System32 and its own directory, disables
extension points, prohibits dynamic code, refuses remote images, and removes
every privilege except `SeChangeNotifyPrivilege` (PawnIO access is a DACL
check). No signature policy: `PawnIOLib.dll` is not Microsoft-signed.

Only the installed service writes `halod-broker.log` (admin-only directory); a
dev-run broker logs to its console. Accepts, refusals, and failures are logged,
not individual register operations.

## Resource bounds

([broker/src/server.rs](../src/broker/src/server.rs))

- Connections and pipe instances are capped (`MAX_CLIENTS`); worker threads are
  joined, not detached.
- A client must send `Hello` within five seconds.
- Per-connection handles are capped (`MAX_HANDLES_PER_KIND`); handle ids never
  wrap.
- Operations are rate-limited per connection, clamped to the protocol ceiling.
- Idle clients are disconnected after `CLIENT_IDLE_TIMEOUT`; with no client for
  a grace period the service stops.
