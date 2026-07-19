// SPDX-License-Identifier: GPL-3.0-or-later
//! Observes host-level plugin discovery sources.
//! Discovery roots for plugin-owned host transports.

use std::sync::Arc;

use crate::{
    application::state::AppState,
    domain::registry::observers::discovery::{self, DiscoveryHandle, TransportScanner},
};

inventory::submit!(TransportScanner {
    name: "Plugin host transports",
    detail: halod_shared::types::DiscoveryDetail::PluginHostTransports,
    platform: None,
    scan: |app| Box::pin(scan(app)),
});

async fn scan(app: Arc<AppState>) {
    let specs = app.registry.active_device_specs();

    let commands: std::collections::BTreeSet<String> = specs
        .iter()
        .filter_map(|spec| spec.r#match.command.as_ref())
        .map(|command| command.command().to_owned())
        .collect();
    for executable in commands {
        discovery::discover_handle(
            &app,
            DiscoveryHandle::Command {
                executable: &executable,
            },
        )
        .await;
    }

    #[cfg(target_os = "windows")]
    if specs.iter().any(|spec| spec.r#match.amd_smn.is_some()) {
        if let Some((family, model)) = amd_signature() {
            discovery::discover_handle(&app, DiscoveryHandle::AmdSmn { family, model }).await;
        }
    }

    #[cfg(target_os = "windows")]
    match specs
        .iter()
        .any(|spec| spec.r#match.lpcio.is_some())
        .then(|| tokio::task::spawn_blocking(lpcio_chips))
    {
        None => {}
        Some(task) => match task.await {
            Ok(chips) => {
                for (slot, chip_id, revision, hwm_base) in chips {
                    discovery::discover_handle(
                        &app,
                        DiscoveryHandle::Lpcio {
                            slot,
                            chip_id,
                            revision,
                            hwm_base,
                        },
                    )
                    .await;
                }
            }
            Err(error) => log::debug!("LPCIO scan task failed: {error}"),
        },
    }
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn amd_signature() -> Option<(u8, u8)> {
    use std::arch::x86_64::__cpuid;
    let leaf0 = __cpuid(0);
    if leaf0.ebx != 0x6874_7541 || leaf0.edx != 0x6974_6E65 || leaf0.ecx != 0x444D_4163 {
        return None;
    }
    let eax = __cpuid(1).eax;
    let base_family = (eax >> 8) & 0xf;
    let family = if base_family == 0xf {
        base_family + ((eax >> 20) & 0xff)
    } else {
        base_family
    };
    let model = if base_family == 0xf || base_family == 6 {
        (((eax >> 16) & 0xf) << 4) | ((eax >> 4) & 0xf)
    } else {
        (eax >> 4) & 0xf
    };
    matches!(family, 0x17 | 0x19 | 0x1a).then_some((family as u8, model as u8))
}

#[cfg(all(target_os = "windows", not(target_arch = "x86_64")))]
fn amd_signature() -> Option<(u8, u8)> {
    None
}

/// The host owns only the small, safety-critical probe needed to create a
/// typed transport root.  Chip-specific registers, labels, sensors, PWM and
/// restoration all belong to the Nuvoton Lua package.
#[cfg(target_os = "windows")]
fn lpcio_chips() -> Vec<(u8, u16, u8, u16)> {
    use crate::infrastructure::drivers::transports::lpcio::LpcIoBus;
    let mut found = Vec::new();
    for (slot, port) in [(0_u8, 0x2e_u16), (1_u8, 0x4e_u16)] {
        let Ok(bus) = LpcIoBus::open(None) else { break };
        if bus.select_slot(slot).is_err() {
            continue;
        }
        // Winbond/Nuvoton extended-function mode.
        if bus.write_port(port, 0x87).is_err() || bus.write_port(port, 0x87).is_err() {
            continue;
        }
        let id = bus.superio_inb(0x20).unwrap_or(0xff);
        let revision = bus.superio_inb(0x21).unwrap_or(0);
        // Rust reports identity only; the enabled plugin declarations decide
        // which chips are supported.
        if id == 0 || id == 0xff {
            let _ = bus.write_port(port, 0xaa);
            continue;
        }
        if bus.find_bars().is_ok() && bus.superio_outb(0x07, 0x0b).is_ok() {
            let hi = bus.superio_inb(0x60).unwrap_or(0) as u16;
            let lo = bus.superio_inb(0x61).unwrap_or(0) as u16;
            found.push((slot, id as u16, revision, (hi << 8) | lo));
        }
        let _ = bus.write_port(port, 0xaa);
    }
    found
}
