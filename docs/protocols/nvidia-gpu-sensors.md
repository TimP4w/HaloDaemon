# NVIDIA GPU Sensors Protocol

Read-only access to NVIDIA GPU temperature sensors. There is no wire bus here: the "protocol" is the reverse-engineered **NvAPI private interface** on Windows and the stable **`nvidia-smi` CSV query** contract on Linux. NVIDIA ships no public sysfs thermal node for the proprietary driver, so both paths are the only machine-readable source.

**Credits:** NvAPI function-ID hashes and the `NV_GPU_THERMAL_SETTINGS_V2` struct layout are reverse-engineered from the NVIDIA NvAPI SDK headers (the same well-known `nvapi_QueryInterface` hashes used by OpenHardwareMonitor / nvapiwrapper). HaloDaemon contributors (GPL-3.0-or-later).

---

## Overview

GPU temperature is **sensor-only** (no RGB, no fan control over this path). One passive `Device` re-reads every GPU's sensors on a fixed cadence; the platform reading source is injected:

| Platform | Source | Mechanism |
|----------|--------|-----------|
| Windows | NvAPI | Load `nvapi64.dll`, resolve private functions by ID via `nvapi_QueryInterface`, call them with C structs |
| Linux | `nvidia-smi` | Spawn the driver's CLI, parse `--format=csv,noheader,nounits` stdout |

Both are host-initiated request/response; the GPU never pushes. All temperatures are integer °C.

---

## 1. Packet layout

There are no on-wire packets. The two "frames" are a C struct (Windows) and a CSV line (Linux).

### Windows — `NV_GPU_THERMAL_SETTINGS_V2`

`get_thermal(handle, target, *settings)` fills this 68-byte struct (asserted by `nv_thermal_settings_size_matches_layout`):

```
offset size field
  0     4   version   u32  — MAKE_NVAPI_VERSION (see §3); caller seeds before the call
  4     4   count     u32  — number of populated sensors (device fills, ≤ 3)
  8    20   sensor[0] NvThermalSensor
 28    20   sensor[1]
 48    20   sensor[2]
total 68
```

`NvThermalSensor` (20 bytes):

```
offset size field
  0     4   controller        u32
  4     4   default_min_temp  i32
  8     4   default_max_temp  i32
 12     4   current_temp      i32  — temperature in °C (the value we read)
 16     4   target            u32  — NV_THERMAL_TARGET (see §3 label map)
```

The full-name buffer (`GetFullName`) is a 64-byte (`NVAPI_SHORT_STRING_MAX`) NUL-terminated ASCII buffer.

### Linux — `nvidia-smi` CSV line

One comma-separated row per GPU, no header, no units:

```
enumerate:  "<uuid>, <name>"                    e.g. "GPU-54968926-…, NVIDIA GeForce RTX 5080"
read temps: "<temperature.gpu>, <temperature.memory>"   e.g. "47, N/A"
```

Fields are split on `,` and trimmed; a field of `N/A` (unsupported sensor) is skipped.

---

## 2. Functions

### Windows (NvAPI)

Every NvAPI function is obtained by `nvapi_QueryInterface(fn_id) → fn pointer`, then `transmute`d to its C signature and called. `nvapi64.dll` exports exactly one symbol, `nvapi_QueryInterface`; everything else is private and addressed by hashed ID.

| Function | ID (§3) | Signature → call | Params | Required sequence / notes |
|----------|---------|------------------|--------|---------------------------|
| `nvapi_QueryInterface` | (DLL export) | `fn(fn_id: u32) -> usize` | function ID | Resolve every other pointer; `0` = unavailable → whole source disabled |
| Initialize | `0x0150E828` | `fn() -> i32` | none | Call once before any other call. Ref-counted in the driver, so it coexists with the SMBus NvAPI module |
| EnumPhysicalGPUs | `0xE5AC921F` | `fn(*mut [usize;64], *mut u32) -> i32` | out: handle array + count | After Initialize. Fills up to `NVAPI_MAX_GPUS` (64) opaque handles |
| GPU_GetFullName | `0xCEEE8E9F` | `fn(handle, *mut u8[64]) -> i32` | GPU handle; out: name buf | Per handle at enumeration; on failure a synthetic `NVIDIA GPU 0x…` name is used |
| GPU_GetThermalSettings | `0xE3640A56` | `fn(handle, target: u32, *mut NvThermalSettings) -> i32` | GPU handle, `target=15` (ALL), pre-versioned struct | The read. Seed `settings.version` first; pass `target=15` to return all sensors |

#### Enumeration sequence (once, lazily, `init_nvapi`)

1. `LoadLibrary("nvapi64.dll")` → get `nvapi_QueryInterface`.
2. Resolve `Initialize`, `EnumPhysicalGPUs`, `GetFullName`, `GetThermalSettings`; bail if any is `0`.
3. `Initialize()` (expect `NVAPI_OK`).
4. `EnumPhysicalGPUs(handles, &count)`.
5. For each non-zero handle, `GetFullName(handle, buf)` → friendly name.
6. Cache `(handle, name)` list in a process `OnceLock`; reused for the lifetime of the process.

#### Read sequence (per poll, `read_temperatures`)

1. `settings = NvThermalSettings::zeroed()` — sets `version`, zeroes the rest.
2. `GetThermalSettings(handle, 15, &settings)`; non-`OK` → error.
3. For `sensor[0..min(count,3)]`, map `target` → label (§3) and take `current_temp` as °C; unknown `target` values are skipped.

### Linux (`nvidia-smi`)

| Function | Command | Params | Notes |
|----------|---------|--------|-------|
| enumerate | `nvidia-smi --query-gpu=uuid,name --format=csv,noheader,nounits` | none | Missing binary / non-zero exit → empty list, not an error |
| read temps | `nvidia-smi -i <uuid> --query-gpu=temperature.gpu,temperature.memory --format=csv,noheader,nounits` | GPU UUID | Addressed by the stable per-GPU UUID from enumerate |

---

## 3. Parameters

**NvAPI function IDs** — stable across driver versions:

| Name | ID |
|------|----|
| Initialize | `0x0150E828` |
| EnumPhysicalGPUs | `0xE5AC921F` |
| GPU_GetFullName | `0xCEEE8E9F` |
| GPU_GetThermalSettings | `0xE3640A56` |

**`version` field — `MAKE_NVAPI_VERSION`**: `sizeof(struct) | (version_id << 16)`. Here `68 | (2 << 16)` = `0x00020044` (V2). The low 16 bits MUST equal the struct size or the driver rejects the call (asserted).

**`target` — `NV_THERMAL_TARGET`**:
- As input to GetThermalSettings: `15` = `NVAPI_THERMAL_TARGET_ALL` (request every sensor).
- As output per sensor → friendly label (`thermal_target_label`):

The enum uses **bit-style values** (1, 2, 4, 8, …) with gaps — not a contiguous run. Values 3/5/6/7 are unused.

| `target` | Label |
|----------|-------|
| 1 | GPU Core |
| 2 | GPU Memory Junction |
| 4 | Power Supply |
| 8 | GPU Board |
| 9 | Visual Computing Board |
| 10 | Visual Computing Inlet |
| 11 | Visual Computing Outlet |
| other | skipped |

VCD targets (9/10/11) require an `NvVisualComputingDeviceHandle`, not the `NvPhysicalGpuHandle` we enumerate, so they are unreachable in practice; consumer GPUs report GPU=1, occasionally Memory=2, and Board=8.

**Limits**: `NVAPI_OK = 0`; max 64 GPUs; max 3 thermal sensors per GPU; 64-byte name string.

**Linux query fields**: always `--format=csv,noheader,nounits`; enumerate selects `uuid,name`; read selects `temperature.gpu,temperature.memory`; output labels are fixed to `["GPU Core", "GPU Memory"]`; `N/A` values (memory temp on most consumer cards) are dropped.

---

## 4. Responses

**Windows.** Every NvAPI call returns `i32`; `NVAPI_OK` (`0`) = success, anything else is an error and the read fails. `GetThermalSettings` writes `count` and the `sensor[]` array in place; `current_temp` is read as integer °C. `GetFullName` writes a NUL-terminated buffer (decoded as UTF-8-lossy up to the first `0`).

**Linux.** Success is `nvidia-smi` exit status `0`; non-zero or spawn failure surfaces as an error (enumerate swallows it to an empty list; read propagates it). The response is stdout text parsed as CSV (see §1).

---

## 5. Polling & notifications

No notifications — neither path pushes. The shared device polls every **1 s** (`POLL_INTERVAL`), calling the platform source's `read_temperatures` for each cached GPU and broadcasting changed sensor values. The GPU handle/UUID list is enumerated once and reused.

---

## Notes

- **NvAPI init is shared.** `NvAPI_Initialize` is reference-counted inside the driver, so this thermal module and the SMBus NvAPI GPU-I2C transport (see [SMBus transport](../transports/smbus.md)) each keep their own resolved pointers without interfering.
- **V2 struct only.** The layout is pinned to `NV_GPU_THERMAL_SETTINGS_V2` (68 bytes); a driver expecting a different version would reject the `version` field.
- **Memory temp is usually absent.** `temperature.memory` / Memory-Junction reports `N/A` on most consumer cards and is silently skipped on both platforms.
- **Linux pays a process spawn** per poll (one `nvidia-smi` per read); Windows is an in-process call.
