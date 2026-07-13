// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
// Reference: OpenRGB ENE SMBus implementation
// https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusInterface/ENESMBusInterface_i2c_smbus.cpp
// SPD reference: https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusController.cpp
//
// Platform backends live in sibling files, each selected by `cfg`:
//   - `linux.rs`          — i2c-dev ioctl interface
//   - `windows/chipset.rs` — PawnIO chipset SMBus
//   - `windows/nvapi.rs`   — NvAPI GPU i2c
//   - `fallback.rs`       — unsupported platforms
// Every backend exposes the same four items consumed below: `SmBusInner`,
// `enumerate_buses`, `enumerate_gpu_buses`, `open_device`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::{
    drivers::Metered,
    registry::discovery::{DiscoveryHandle, SmBusScanEntry, TransportScanner},
    state::AppState,
};
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

// The raw SMBus primitives — the ops trait, `BusInfo`, and the platform
// backends — now live in `halod-hwaccess` (shared with the elevated broker).
// Re-exported so every device driver / discovery / plugin call site that
// imports `transports::smbus::{BusInfo, SmBusSyncOps}` keeps resolving.
use halod_hwaccess::smbus::{enumerate_buses, enumerate_gpu_buses};
pub use halod_hwaccess::smbus::{BusInfo, SmBusSyncOps};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmbusBusKind {
    Chipset,
    Gpu,
}

/// A PCI-identity filter that gates a GPU-bus scan to known cards, so the daemon
/// never pokes an RGB address on a graphics card it doesn't recognise — the GPU
/// I²C segment is shared with the monitor's DDC/EDID lines, and a stray write
/// there can hang the display. `None` fields are wildcards; every set field must
/// equal the bus's corresponding PCI id.
///
/// `Copy`/const-constructible so a native [`SmBusScanEntry`] can declare a
/// `static [PciMatch]`, and `Deserialize` so a Lua plugin manifest can declare
/// the same list — both feed the identical scanner gate ([`gate_bus`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PciMatch {
    #[serde(default)]
    pub vendor: Option<u16>,
    #[serde(default)]
    pub device: Option<u16>,
    #[serde(default)]
    pub sub_vendor: Option<u16>,
    #[serde(default)]
    pub sub_device: Option<u16>,
    /// A verified board: the scanner emits it without any probe transaction (the
    /// curated-whitelist stance). Unset entries are confirmed by a gentle probe
    /// before use.
    #[serde(default)]
    pub confirmed: bool,
}

impl PciMatch {
    /// Do all of this filter's set fields equal `bus`'s PCI ids?
    pub fn accepts(&self, bus: &BusInfo) -> bool {
        self.vendor.is_none_or(|v| v == bus.pci_vendor)
            && self.device.is_none_or(|v| v == bus.pci_device)
            && self.sub_vendor.is_none_or(|v| v == bus.pci_sub_vendor)
            && self.sub_device.is_none_or(|v| v == bus.pci_sub_device)
    }
}

pub struct SmBusDevice {
    pub bus_number: u8,
    /// Rate-limits the whole bus; devices sharing a `SmBusScanEntry` already
    /// share this gate and its bus mutex. Boxed so tests can inject a recording
    /// backend without opening real hardware.
    io: Metered<Mutex<Box<dyn SmBusSyncOps + Send>>>,
}

impl SmBusDevice {
    pub fn open(info: &BusInfo) -> Result<Arc<Self>> {
        let inner = super::register_ops::open_smbus(info)?;
        Ok(Arc::new(Self {
            bus_number: info.bus_number,
            io: Metered::new(Mutex::new(inner), None),
        }))
    }

    /// Wrap an arbitrary synchronous ops backend — a mock in tests.
    #[cfg(test)]
    pub fn from_ops(bus_number: u8, ops: Box<dyn SmBusSyncOps + Send>) -> Arc<Self> {
        Arc::new(Self {
            bus_number,
            io: Metered::new(Mutex::new(ops), None),
        })
    }

    /// Run a batch of synchronous SMBus ops in one `spawn_blocking` call,
    /// tallying written bytes and metering them through the bus's write-rate gate.
    pub async fn run_batch<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut dyn SmBusSyncOps) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        self.io
            .write_tallied(move |inner, bytes| {
                let mut g = inner.lock().map_err(|_| anyhow!("smbus lock poisoned"))?;
                let mut counting = CountingSmBusOps {
                    inner: &mut **g,
                    bytes,
                };
                f(&mut counting)
            })
            .await
    }

    /// Inline twin of [`run_batch`](Self::run_batch): runs the ops on the
    /// **calling thread** under the bus lock (no `spawn_blocking`, no
    /// `Send`/`'static` bound on `f`), so the closure may call back into
    /// non-`Send` state such as a plugin's Lua VM. The caller must be off the
    /// async runtime (a dedicated `std::thread`), since it holds the lock and
    /// may block on the write-rate gate.
    pub fn run_batch_local<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut dyn SmBusSyncOps) -> Result<R>,
    {
        self.io.write_tallied_local(move |inner, bytes| {
            let mut g = inner.lock().map_err(|_| anyhow!("smbus lock poisoned"))?;
            let mut counting = CountingSmBusOps {
                inner: &mut **g,
                bytes,
            };
            f(&mut counting)
        })
    }

    /// Live write-rate limit and throughput for this bus.
    pub fn rate_status(&self) -> Option<WriteRateStatus> {
        Some(self.io.status())
    }

    /// Set (or clear) this bus's write-rate ceiling.
    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }
}

/// Delegates to `inner`, tallying bytes written by successful calls.
struct CountingSmBusOps<'a> {
    inner: &'a mut dyn SmBusSyncOps,
    bytes: &'a AtomicUsize,
}

impl SmBusSyncOps for CountingSmBusOps<'_> {
    fn read_byte(&mut self, addr: u8) -> Result<u8> {
        self.inner.read_byte(addr)
    }

    fn read_byte_data(&mut self, addr: u8, cmd: u8) -> Result<u8> {
        self.inner.read_byte_data(addr, cmd)
    }

    fn write_quick(&mut self, addr: u8) -> Result<bool> {
        let result = self.inner.write_quick(addr);
        if result.is_ok() {
            self.bytes.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
        let result = self.inner.write_byte_data(addr, cmd, val);
        if result.is_ok() {
            self.bytes.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
        let result = self.inner.write_word_data(addr, cmd, val);
        if result.is_ok() {
            self.bytes.fetch_add(2, Ordering::Relaxed);
        }
        result
    }

    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        let len = data.len();
        let result = self.inner.write_block_data(addr, cmd, data);
        if result.is_ok() {
            self.bytes.fetch_add(len, Ordering::Relaxed);
        }
        result
    }

    fn supports_block_write(&self) -> bool {
        self.inner.supports_block_write()
    }
}

/// Abstraction over an SMBus bus, so [`crate::registry::discovery::DiscoveryHandle`] stays free of the concrete type.
pub trait SmBusOps: Send + Sync {
    fn bus_number(&self) -> u8;
    /// Convert this `Arc<Self>` into `Arc<dyn Any + Send + Sync>` so the
    /// standard [`Arc::downcast`] can recover the concrete type.
    fn into_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

impl SmBusOps for SmBusDevice {
    fn bus_number(&self) -> u8 {
        self.bus_number
    }
    fn into_any_arc(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// Downcast `Arc<dyn SmBusOps>` to `Arc<SmBusDevice>`. Panics if the
/// underlying type isn't `SmBusDevice` (never happens: every `Smbus`
/// discovery handle is created by the SMBus transport scanner).
pub fn downcast_smbus_device(bus: Arc<dyn SmBusOps>) -> Arc<SmBusDevice> {
    bus.into_any_arc()
        .downcast::<SmBusDevice>()
        .expect("SmBusOps handle was not a SmBusDevice")
}

pub struct SmBusTransport;

impl SmBusTransport {
    /// Enumerate every chipset and GPU SMBus controller without opening any.
    /// Returns `(chipset_buses, gpu_buses)`; used by the debug usecase.
    pub async fn enumerate_for_debug() -> (Vec<BusInfo>, Vec<BusInfo>) {
        tokio::task::spawn_blocking(|| (enumerate_buses(), enumerate_gpu_buses()))
            .await
            .unwrap_or_else(|e| {
                log::error!("[SmBus] enumerate_for_debug task panicked: {e}");
                Default::default()
            })
    }
}

/// Label for the discovery status line while probing one bus, preferring the
/// adapter name and falling back to the bus number. The UI prefixes this with
/// its translated "Scanning …" wording.
fn bus_scan_label(bus: &BusInfo) -> String {
    if bus.adapter_name.trim().is_empty() {
        format!("SMBus bus {}", bus.bus_number)
    } else {
        format!("SMBus · {}", bus.adapter_name.trim())
    }
}

/// One scan pass over a bus family: what addresses to probe, how, plus optional
/// pre-scan and rate ceiling. Unifies native `SmBusScanEntry`s and the runtime
/// entries plugins contribute, so both drive the identical open/probe flow.
struct ScanJob {
    bus_kind: SmbusBusKind,
    addresses: Vec<u8>,
    write_rate_limit: Option<WriteRateLimit>,
    /// A native pre-scan (fn pointer) or a plugin pre-scan (Lua source + scope).
    pre_scan: PreScan,
    probe: Probe,
    /// PCI-identity gate. Empty ⇒ ungated (chipset default; forbidden on a GPU
    /// job — see [`gate_bus`] and the enforcement in [`discover`]).
    pci_match: Vec<PciMatch>,
}

type NativePreScan = fn(Arc<SmBusDevice>) -> Pin<Box<dyn Future<Output = Result<()>> + Send>>;

enum PreScan {
    None,
    Native(NativePreScan),
    Plugin {
        plugin_id: String,
        source: String,
        scope: Vec<u8>,
    },
}

/// How to gate a declared address into a discovery handle.
#[derive(Clone, Copy)]
enum Probe {
    /// Always emit (native entries; plugins with `probe = "none"`).
    Always,
    /// Emit only if `write_quick` ACKs.
    Quick,
    /// Emit only if `read_byte` succeeds.
    ReadByte,
}

use std::future::Future;
use std::pin::Pin;

fn native_scan_jobs() -> Vec<ScanJob> {
    inventory::iter::<SmBusScanEntry>()
        .map(|entry| ScanJob {
            bus_kind: entry.bus_kind,
            addresses: entry.addresses.to_vec(),
            write_rate_limit: entry.write_rate_limit,
            pre_scan: match entry.pre_scan {
                Some(f) => PreScan::Native(f),
                None => PreScan::None,
            },
            // Native entries pre-select their addresses, so every one is emitted.
            probe: Probe::Always,
            pci_match: entry.pci_match.to_vec(),
        })
        .collect()
}

fn plugin_scan_jobs(registry: &crate::drivers::plugins::Registry) -> Vec<ScanJob> {
    registry
        .plugin_smbus_scan_entries()
        .into_iter()
        .map(|e| ScanJob {
            bus_kind: e.bus_kind,
            addresses: e.addresses.clone(),
            write_rate_limit: e.write_rate_limit,
            probe: match e.probe {
                crate::drivers::plugins::ProbeMode::Quick => Probe::Quick,
                crate::drivers::plugins::ProbeMode::ReadByte => Probe::ReadByte,
                crate::drivers::plugins::ProbeMode::None => Probe::Always,
            },
            pre_scan: if e.pre_scan {
                PreScan::Plugin {
                    plugin_id: e.plugin_id.clone(),
                    source: e.script_source.clone(),
                    scope: e.pre_scan_scope(),
                }
            } else {
                PreScan::None
            },
            pci_match: e.pci_match.clone(),
        })
        .collect()
}

/// Per-bus scan decision from a job's PCI gate, evaluated *before the bus is
/// opened*. `None` ⇒ skip this bus (not a card the gate lists). `Some(probe)` ⇒
/// scan it with this probe mode: a `confirmed` match downgrades to
/// [`Probe::Always`] (emit without a probe transaction), any other match keeps
/// the job's declared probe. An empty gate is "ungated" and returns the job's
/// probe unchanged — permitted for chipset buses; [`discover`] forbids it on a
/// GPU job before ever reaching here.
fn gate_bus(pci_match: &[PciMatch], bus: &BusInfo, job_probe: Probe) -> Option<Probe> {
    if pci_match.is_empty() {
        return Some(job_probe);
    }
    let mut matched: Option<Probe> = None;
    for m in pci_match {
        if m.accepts(bus) {
            if m.confirmed {
                return Some(Probe::Always);
            }
            matched = Some(job_probe);
        }
    }
    matched
}

/// Does `addr` respond on this bus, per the job's probe mode?
fn probe_addr(bus: &SmBusDevice, addr: u8, probe: Probe) -> bool {
    match probe {
        Probe::Always => true,
        Probe::Quick => bus
            .run_batch_local(move |ops| Ok(ops.write_quick(addr).unwrap_or(false)))
            .unwrap_or(false),
        Probe::ReadByte => bus
            .run_batch_local(move |ops| Ok(ops.read_byte(addr).is_ok()))
            .unwrap_or(false),
    }
}

async fn discover(app: Arc<AppState>) -> Result<()> {
    let (chipset_buses, gpu_buses) =
        tokio::task::spawn_blocking(|| (enumerate_buses(), enumerate_gpu_buses())).await?;

    // Failing to open a bus is non-fatal; warn once so the cause isn't silent.
    let mut open_warned = false;

    let mut jobs = native_scan_jobs();
    jobs.extend(plugin_scan_jobs(&app.registry));

    for job in jobs {
        // A GPU job MUST carry a PCI gate: the GPU I²C segment is shared with the
        // display's DDC/EDID lines, so an ungated probe could hang a monitor on a
        // card we don't even support. Plugins are already rejected at parse; this
        // is the backstop for native entries (which can't fail at submit time).
        if job.bus_kind == SmbusBusKind::Gpu && job.pci_match.is_empty() {
            log::warn!(
                "[SmBusTransport] GPU scan entry declares no pci_match; refusing to \
                 scan GPU buses (risk of hanging the display bus). Entry skipped."
            );
            continue;
        }

        let buses: Vec<&BusInfo> = match job.bus_kind {
            SmbusBusKind::Chipset => chipset_buses.iter().filter(|b| !b.is_gpu_bus()).collect(),
            SmbusBusKind::Gpu => gpu_buses.iter().collect(),
        };
        log::debug!(
            "[SmBusTransport] {:?}: {} bus(es) to scan",
            job.bus_kind,
            buses.len()
        );

        for bus_info in buses {
            // Consult the PCI gate before opening the bus: a card the gate doesn't
            // list is never touched. `confirmed` matches downgrade to no probe.
            let Some(effective_probe) = gate_bus(&job.pci_match, bus_info, job.probe) else {
                log::debug!(
                    "[SmBusTransport] bus {} ({:04x}:{:04x} / {:04x}:{:04x}) not in PCI gate; skipping",
                    bus_info.bus_number,
                    bus_info.pci_vendor,
                    bus_info.pci_device,
                    bus_info.pci_sub_vendor,
                    bus_info.pci_sub_device,
                );
                continue;
            };
            crate::registry::discovery::set_discovery_detail(&app, bus_scan_label(bus_info)).await;
            let bus = match SmBusDevice::open(bus_info) {
                Ok(b) => b,
                Err(e) => {
                    if open_warned {
                        log::debug!(
                            "[SmBusTransport] Cannot open bus {}: {}",
                            bus_info.bus_number,
                            e
                        );
                    } else {
                        log::warn!(
                            "[SmBusTransport] cannot open SMBus bus {}: {}, \
                             SMBus RGB devices (e.g. DRAM) on this bus \
                             will be unavailable",
                            bus_info.bus_number,
                            e
                        );
                        open_warned = true;
                    }
                    continue;
                }
            };

            // Apply the entry's declared ceiling before any traffic (pre_scan,
            // probes, later effect streams) touches the freshly opened bus.
            bus.set_write_rate_limit(job.write_rate_limit);

            run_pre_scan(&app.registry, &job.pre_scan, &bus, bus_info.bus_number).await;

            for &addr in &job.addresses {
                if !probe_addr(&bus, addr, effective_probe) {
                    continue;
                }
                crate::registry::discovery::discover_handle(
                    &app,
                    DiscoveryHandle::Smbus {
                        bus: Arc::clone(&bus) as Arc<dyn SmBusOps>,
                        addr,
                        bus_kind: job.bus_kind,
                    },
                )
                .await;
            }
        }
    }

    Ok(())
}

/// Run a job's pre-scan (native fn or plugin Lua) against a freshly opened bus.
async fn run_pre_scan(
    registry: &crate::drivers::plugins::Registry,
    pre_scan: &PreScan,
    bus: &Arc<SmBusDevice>,
    bus_number: u8,
) {
    let result = match pre_scan {
        PreScan::None => return,
        PreScan::Native(f) => f(Arc::clone(bus)).await,
        PreScan::Plugin {
            plugin_id,
            source,
            scope,
        } => {
            let bus = Arc::clone(bus);
            let source = source.clone();
            let scope = scope.clone();
            let granted = registry.granted_for(plugin_id);
            tokio::task::spawn_blocking(move || {
                crate::drivers::plugins::run_pre_scan(
                    &source,
                    bus,
                    scope,
                    &granted,
                    tokio::runtime::Handle::current(),
                )
            })
            .await
            .unwrap_or_else(|e| Err(anyhow!("pre_scan task panicked: {e}")))
        }
    };
    if let Err(e) = result {
        let who = match pre_scan {
            PreScan::Plugin { plugin_id, .. } => plugin_id.as_str(),
            _ => "native",
        };
        log::debug!("[SmBusTransport] pre_scan ({who}) failed on bus {bus_number}: {e}");
    }
}

inventory::submit!(TransportScanner {
    name: "SMBus",
    platform: None,
    scan: |app| Box::pin(async move {
        if let Err(e) = discover(app).await {
            log::error!("SMBus discovery failed: {e}");
        }
    }),
});

#[cfg(test)]
mod tests {
    use super::{
        bus_scan_label, gate_bus, BusInfo, CountingSmBusOps, PciMatch, Probe, SmBusSyncOps,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeOps {
        fail_writes: bool,
    }

    impl SmBusSyncOps for FakeOps {
        fn read_byte(&mut self, _addr: u8) -> anyhow::Result<u8> {
            Ok(0)
        }
        fn read_byte_data(&mut self, _addr: u8, _cmd: u8) -> anyhow::Result<u8> {
            Ok(0)
        }
        fn write_quick(&mut self, _addr: u8) -> anyhow::Result<bool> {
            if self.fail_writes {
                anyhow::bail!("nak");
            }
            Ok(true)
        }
        fn write_byte_data(&mut self, _addr: u8, _cmd: u8, _val: u8) -> anyhow::Result<()> {
            if self.fail_writes {
                anyhow::bail!("nak");
            }
            Ok(())
        }
        fn write_word_data(&mut self, _addr: u8, _cmd: u8, _val: u16) -> anyhow::Result<()> {
            if self.fail_writes {
                anyhow::bail!("nak");
            }
            Ok(())
        }
        fn write_block_data(&mut self, _addr: u8, _cmd: u8, _data: &[u8]) -> anyhow::Result<()> {
            if self.fail_writes {
                anyhow::bail!("nak");
            }
            Ok(())
        }
    }

    #[test]
    fn counting_smbus_ops_tallies_only_successful_writes() {
        let bytes = AtomicUsize::new(0);
        let mut fake = FakeOps { fail_writes: false };
        let mut counting = CountingSmBusOps {
            inner: &mut fake,
            bytes: &bytes,
        };

        counting.write_quick(0x50).unwrap();
        counting.write_byte_data(0x50, 0x01, 0xFF).unwrap();
        counting.write_word_data(0x50, 0x02, 0xBEEF).unwrap();
        counting
            .write_block_data(0x50, 0x03, &[1, 2, 3, 4, 5])
            .unwrap();

        // 1 (quick) + 1 (byte) + 2 (word) + 5 (block)
        assert_eq!(bytes.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn counting_smbus_ops_does_not_tally_failed_writes() {
        let bytes = AtomicUsize::new(0);
        let mut fake = FakeOps { fail_writes: true };
        let mut counting = CountingSmBusOps {
            inner: &mut fake,
            bytes: &bytes,
        };

        let _ = counting.write_quick(0x50);
        let _ = counting.write_byte_data(0x50, 0x01, 0xFF);
        let _ = counting.write_word_data(0x50, 0x02, 0xBEEF);
        let _ = counting.write_block_data(0x50, 0x03, &[1, 2, 3]);

        assert_eq!(bytes.load(Ordering::Relaxed), 0);
    }

    fn bus(bus_number: u8, adapter_name: &str) -> BusInfo {
        BusInfo {
            bus_number,
            adapter_name: adapter_name.to_string(),
            pci_vendor: 0,
            pci_device: 0,
            pci_sub_vendor: 0,
            pci_sub_device: 0,
        }
    }

    #[test]
    fn bus_scan_label_prefers_adapter_name() {
        assert_eq!(bus_scan_label(&bus(3, "i801 SMBus")), "SMBus · i801 SMBus");
    }

    #[test]
    fn bus_scan_label_falls_back_to_bus_number() {
        assert_eq!(bus_scan_label(&bus(5, "   ")), "SMBus bus 5");
        assert_eq!(bus_scan_label(&bus(0, "")), "SMBus bus 0");
    }

    // ── PCI gate ─────────────────────────────────────────────────────────────

    /// An ASUS ROG STRIX RTX 4090's bus IDs.
    fn asus_4090() -> BusInfo {
        BusInfo {
            bus_number: 7,
            adapter_name: "NVIDIA i2c".to_string(),
            pci_vendor: 0x10DE,
            pci_device: 0x2684,
            pci_sub_vendor: 0x1043,
            pci_sub_device: 0x88BF,
        }
    }

    fn m(sub_device: Option<u16>, confirmed: bool) -> PciMatch {
        PciMatch {
            vendor: Some(0x10DE),
            device: Some(0x2684),
            sub_vendor: Some(0x1043),
            sub_device,
            confirmed,
        }
    }

    #[test]
    fn accepts_matches_only_when_all_set_fields_equal() {
        let card = asus_4090();
        // Fully-specified exact match.
        assert!(m(Some(0x88BF), true).accepts(&card));
        // Wildcard sub_device still matches.
        assert!(m(None, true).accepts(&card));
        // Any single differing field rejects.
        assert!(!m(Some(0x8000), true).accepts(&card));
        assert!(!PciMatch {
            vendor: Some(0x1002), // AMD, not this NVIDIA card
            ..m(None, false)
        }
        .accepts(&card));
    }

    #[test]
    fn gate_empty_is_ungated_passthrough() {
        // No gate ⇒ keep the job's probe (chipset behaviour; GPU jobs are
        // rejected before reaching gate_bus).
        assert!(matches!(
            gate_bus(&[], &asus_4090(), Probe::Quick),
            Some(Probe::Quick)
        ));
    }

    #[test]
    fn gate_skips_unlisted_card() {
        // Gate lists a different sub_device only ⇒ this card is not covered.
        let gate = [m(Some(0x0000), false)];
        assert!(gate_bus(&gate, &asus_4090(), Probe::ReadByte).is_none());
    }

    #[test]
    fn gate_confirmed_match_skips_the_probe() {
        let gate = [m(Some(0x88BF), true)];
        assert!(matches!(
            gate_bus(&gate, &asus_4090(), Probe::ReadByte),
            Some(Probe::Always)
        ));
    }

    #[test]
    fn gate_unconfirmed_match_keeps_job_probe() {
        let gate = [m(Some(0x88BF), false)];
        assert!(matches!(
            gate_bus(&gate, &asus_4090(), Probe::ReadByte),
            Some(Probe::ReadByte)
        ));
    }

    #[test]
    fn gate_confirmed_wins_over_unconfirmed_regardless_of_order() {
        let card = asus_4090();
        // unconfirmed (wildcard) before confirmed (exact)
        let a = [m(None, false), m(Some(0x88BF), true)];
        assert!(matches!(
            gate_bus(&a, &card, Probe::ReadByte),
            Some(Probe::Always)
        ));
        // confirmed first
        let b = [m(Some(0x88BF), true), m(None, false)];
        assert!(matches!(
            gate_bus(&b, &card, Probe::ReadByte),
            Some(Probe::Always)
        ));
    }
}
