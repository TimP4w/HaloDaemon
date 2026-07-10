// SPDX-License-Identifier: GPL-3.0-or-later
//! SMBus plugin transport backend: an addressed [`RegisterBus`] scoped to the
//! device's own address, opened from an `SmBusScanEntry`-produced handle.

use anyhow::{bail, Result};

use crate::drivers::plugins::manifest::{MatchSpec, PluginManifest};
use crate::drivers::plugins::transport::{
    AddrScope, PluginIo, PluginTransportDescriptor, RegisterBus,
};
use crate::drivers::transports::smbus::{downcast_smbus_device, SmbusBusKind};
use crate::registry::discovery::DiscoveryHandle;

fn matches(spec: &MatchSpec, handle: &DiscoveryHandle<'_>) -> bool {
    let DiscoveryHandle::Smbus { addr, bus_kind, .. } = handle else {
        return false;
    };
    spec.bus_kind() == Some(*bus_kind) && spec.addresses.as_ref().is_some_and(|a| a.contains(addr))
}

fn open(_manifest: &PluginManifest, handle: &DiscoveryHandle<'_>) -> Result<PluginIo> {
    let DiscoveryHandle::Smbus { bus, addr, .. } = handle else {
        bail!("smbus backend matched a non-SMBus handle");
    };
    let device = downcast_smbus_device(std::sync::Arc::clone(bus));
    // A running device is scoped to its own address only — pre-scan (broadcast
    // remap, candidate probing) is the sole place the wider declared set is
    // writable.
    Ok(PluginIo::Register(RegisterBus::new(
        device,
        AddrScope::single(*addr),
    )))
}

fn id_suffix(handle: &DiscoveryHandle<'_>) -> String {
    match handle {
        DiscoveryHandle::Smbus { addr, .. } => format!("addr{addr:02x}"),
        _ => "0".to_owned(),
    }
}

fn validate(spec: &MatchSpec) -> Result<()> {
    if spec.bus_kind().is_none() {
        bail!("smbus match requires `bus` = \"chipset\" or \"gpu\"");
    }
    match &spec.addresses {
        Some(a) if !a.is_empty() => {}
        _ => bail!("smbus match requires a non-empty `addresses` list"),
    }
    // A GPU bus is shared with the display's DDC/EDID lines; refuse to scan one
    // without a PCI gate confining the probe to known cards.
    if spec.bus_kind() == Some(SmbusBusKind::Gpu) && spec.pci_match.is_empty() {
        bail!("smbus `bus = \"gpu\"` match requires a non-empty `pci_match` list");
    }
    Ok(())
}

inventory::submit!(PluginTransportDescriptor {
    kind: "smbus",
    matches,
    open,
    id_suffix,
    validate,
});
