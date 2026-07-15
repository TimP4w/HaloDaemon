// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal discovery roots for plugin-owned host transports.

use std::sync::Arc;

use crate::{
    registry::discovery::{self, DiscoveryHandle, TransportScanner},
    state::AppState,
};

inventory::submit!(TransportScanner {
    name: "Plugin host transports",
    platform: None,
    scan: |app| Box::pin(scan(app)),
});

async fn scan(app: Arc<AppState>) {
    // Command plugins perform their own enumeration in Lua. One root per
    // executable preserves a single serialized worker and keeps argv authority
    // entirely manifest-defined.
    discovery::discover_handle(
        &app,
        DiscoveryHandle::Command {
            executable: "nvidia-smi",
        },
    )
    .await;

    #[cfg(target_os = "windows")]
    if let Some((family, model)) = amd_signature() {
        discovery::discover_handle(&app, DiscoveryHandle::AmdSmn { family, model }).await;
    }

    #[cfg(target_os = "windows")]
    match tokio::task::spawn_blocking(lpcio_chips).await {
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

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
fn amd_signature() -> Option<(u8, u8)> {
    None
}

/// The host owns only the small, safety-critical probe needed to create a
/// typed transport root.  Chip-specific registers, labels, sensors, PWM and
/// restoration all belong to the Nuvoton Lua package.
#[cfg(target_os = "windows")]
fn lpcio_chips() -> Vec<(u8, u16, u8, u16)> {
    use crate::drivers::transports::lpcio::LpcIoBus;
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
        // IDs accepted by the official plugin catalog. Unknown chips never
        // reach Lua, avoiding a broad SuperIO catch-all.
        if !supported_nuvoton(id, revision) {
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

#[cfg(target_os = "windows")]
fn supported_nuvoton(id: u8, revision: u8) -> bool {
    let hi = revision & 0xf0;
    matches!(
        (id, revision, hi),
        (0xb4, _, 0x70)
            | (0xc3, _, 0x30)
            | (0xc4, _, 0x50)
            | (0xc5, _, 0x60)
            | (0xc7, 0x32, _)
            | (0xc8, 0x03, _)
            | (0xc9, 0x11 | 0x13, _)
            | (0xd1, 0x21, _)
            | (0xd3, 0x52, _)
            | (0xd4, 0x23 | 0x2a | 0x51 | 0x2b | 0x40 | 0x41, _)
            | (0xd5, 0x92, _)
            | (0xd8, 0x02 | 0x06, _)
    )
}
