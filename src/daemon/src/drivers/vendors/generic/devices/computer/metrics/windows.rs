// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]
//! Windows host-metrics backend, reading the numbers from WMI.

use async_trait::async_trait;
use std::collections::HashMap;

use super::{HostMetrics, HostMetricsBackend, MemInfo};

pub struct WindowsMetrics;

#[async_trait]
impl HostMetricsBackend for WindowsMetrics {
    async fn read(&self) -> HostMetrics {
        tokio::task::spawn_blocking(read_blocking)
            .await
            .unwrap_or_default()
    }
}

fn read_blocking() -> HostMetrics {
    use wmi::{COMLibrary, Variant, WMIConnection};

    let com = match COMLibrary::new() {
        Ok(c) => c,
        Err(e) => {
            log::debug!("[ComputerDevice] metrics COM init failed: {e}");
            return HostMetrics::default();
        }
    };
    let conn = match WMIConnection::new(com) {
        Ok(c) => c,
        Err(e) => {
            log::debug!("[ComputerDevice] metrics WMI connect failed: {e}");
            return HostMetrics::default();
        }
    };

    let query = |sql: &str| -> Vec<HashMap<String, Variant>> {
        conn.raw_query(sql).unwrap_or_else(|e| {
            log::debug!("[ComputerDevice] metrics WMI query failed: {e}");
            Vec::new()
        })
    };

    let cpu_load_pct = query(
        "SELECT PercentProcessorTime FROM Win32_PerfFormattedData_PerfOS_Processor WHERE Name='_Total'",
    )
    .first()
    .and_then(|r| num_of(r.get("PercentProcessorTime")));

    let mem = {
        let rows =
            query("SELECT FreePhysicalMemory, TotalVisibleMemorySize FROM Win32_OperatingSystem");
        rows.first().and_then(|r| {
            let free = num_of(r.get("FreePhysicalMemory"))?;
            let total = num_of(r.get("TotalVisibleMemorySize"))?;
            MemInfo::from_kib(total - free, total)
        })
    };

    let cpu_mhz = {
        let rows = query("SELECT CurrentClockSpeed FROM Win32_Processor");
        if rows.is_empty() {
            None
        } else {
            let sum: f64 = rows
                .iter()
                .filter_map(|r| num_of(r.get("CurrentClockSpeed")))
                .sum();
            let count = rows.len() as f64;
            Some(sum / count)
        }
    };

    let uptime_secs = query("SELECT SystemUpTime FROM Win32_PerfFormattedData_PerfOS_System")
        .first()
        .and_then(|r| num_of(r.get("SystemUpTime")));

    HostMetrics {
        cpu_load_pct,
        mem,
        cpu_mhz,
        uptime_secs,
    }
}

/// Coerce a WMI `Variant` to `f64`, tolerating the various integer types (and
/// string-encoded numbers) WMI hands back.
fn num_of(v: Option<&wmi::Variant>) -> Option<f64> {
    use wmi::Variant;
    match v? {
        Variant::UI1(n) => Some(*n as f64),
        Variant::UI2(n) => Some(*n as f64),
        Variant::UI4(n) => Some(*n as f64),
        Variant::UI8(n) => Some(*n as f64),
        Variant::I1(n) => Some(*n as f64),
        Variant::I2(n) => Some(*n as f64),
        Variant::I4(n) => Some(*n as f64),
        Variant::I8(n) => Some(*n as f64),
        Variant::R4(n) => Some(*n as f64),
        Variant::R8(n) => Some(*n),
        Variant::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
