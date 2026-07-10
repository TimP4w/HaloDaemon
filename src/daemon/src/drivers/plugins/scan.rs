// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime SMBus scan entries contributed by plugins. `inventory` (what native
//! drivers use for `SmBusScanEntry`) is link-time, so plugins — loaded from a
//! directory at runtime — hand the SMBus scanner these instead. The scanner
//! probes only the declared `addresses`; a plugin can never widen the set at
//! runtime, which keeps the raw bus off-limits to untrusted scripts.

use halod_shared::types::WriteRateLimit;

use super::manifest::ProbeMode;
use crate::drivers::transports::smbus::{PciMatch, SmbusBusKind};

/// One plugin-declared SMBus scan, mirroring the fields the scanner reads from a
/// native `SmBusScanEntry` plus what it needs to run the plugin's `pre_scan`.
pub struct PluginScanEntry {
    pub plugin_id: String,
    /// Full script text, so the scanner can build a throwaway VM for `pre_scan`.
    pub script_source: String,
    pub bus_kind: SmbusBusKind,
    pub addresses: Vec<u8>,
    /// Addresses `pre_scan` may additionally write (e.g. an ENE broadcast addr).
    pub extra_addresses: Vec<u8>,
    pub write_rate_limit: Option<WriteRateLimit>,
    pub pre_scan: bool,
    pub probe: ProbeMode,
    /// PCI-identity gate (GPU buses). Mirrors `MatchSpec.pci_match`.
    pub pci_match: Vec<PciMatch>,
}

impl PluginScanEntry {
    /// The address scope a `pre_scan` runs under: declared + extras.
    pub fn pre_scan_scope(&self) -> Vec<u8> {
        let mut v = self.addresses.clone();
        v.extend_from_slice(&self.extra_addresses);
        v
    }
}

/// Build the SMBus scan entries every enabled plugin declares. Called by the
/// SMBus scanner each discovery pass, so enable/disable and reloads take effect.
pub fn plugin_smbus_scan_entries() -> Vec<PluginScanEntry> {
    let registry = match super::PLUGIN_REGISTRY.read() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for m in registry.iter() {
        if super::is_disabled(&m.plugin_id) {
            continue;
        }
        for spec in m.smbus_specs() {
            let Some(bus_kind) = spec.bus_kind() else {
                continue;
            };
            out.push(PluginScanEntry {
                plugin_id: m.plugin_id.clone(),
                script_source: m.script_source.clone(),
                bus_kind,
                addresses: spec.addresses.clone().unwrap_or_default(),
                extra_addresses: spec.extra_addresses.clone().unwrap_or_default(),
                write_rate_limit: spec
                    .max_bytes_per_sec
                    .map(|max_bytes_per_sec| WriteRateLimit { max_bytes_per_sec }),
                pre_scan: spec.pre_scan,
                probe: spec.probe,
                pci_match: spec.pci_match.clone(),
            });
        }
    }
    out
}
