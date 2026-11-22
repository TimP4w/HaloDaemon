// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]
//! Windows host-metrics backend, reading the numbers from WMI.

use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;

use super::{HostMetrics, HostMetricsBackend, MemInfo};

/// Raw `% Processor Performance` counter and its base.
type PerfSample = (f64, f64);

#[derive(Default)]
pub struct WindowsMetrics {
    /// Previous perf sample, for the effective-frequency delta between polls.
    prev_perf: Mutex<Option<PerfSample>>,
}

#[async_trait]
impl HostMetricsBackend for WindowsMetrics {
    async fn read(&self) -> HostMetrics {
        let prev = *self.prev_perf.lock();
        let (metrics, sample) = tokio::task::spawn_blocking(move || read_blocking(prev))
            .await
            .unwrap_or_default();
        if sample.is_some() {
            *self.prev_perf.lock() = sample;
        }
        metrics
    }
}

fn read_blocking(prev: Option<PerfSample>) -> (HostMetrics, Option<PerfSample>) {
    use wmi::{Variant, WMIConnection};

    let conn = match WMIConnection::new() {
        Ok(c) => c,
        Err(e) => {
            log::debug!("metrics WMI connect failed: {e}");
            return (HostMetrics::default(), None);
        }
    };

    let query = |sql: &str| -> Vec<HashMap<String, Variant>> {
        conn.raw_query(sql).unwrap_or_else(|e| {
            log::debug!("metrics WMI query failed: {e}");
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

    // Win32_Processor.CurrentClockSpeed is pinned to the base clock, so derive
    // the effective clock from the nominal frequency and % Processor Performance.
    let (cpu_mhz, sample) = query(
        "SELECT ProcessorFrequency, PercentProcessorPerformance, PercentProcessorPerformance_Base \
         FROM Win32_PerfRawData_Counters_ProcessorInformation WHERE Name='_Total'",
    )
    .first()
    .map(|r| {
        let nominal = num_of(r.get("ProcessorFrequency"));
        let sample = num_of(r.get("PercentProcessorPerformance"))
            .zip(num_of(r.get("PercentProcessorPerformance_Base")));
        let mhz = match (nominal, prev, sample) {
            (Some(n), Some(p), Some(c)) => effective_mhz(n, p, c).or(Some(n)),
            (n, ..) => n,
        };
        (mhz, sample)
    })
    .unwrap_or_default();

    let uptime_secs = query("SELECT SystemUpTime FROM Win32_PerfFormattedData_PerfOS_System")
        .first()
        .and_then(|r| num_of(r.get("SystemUpTime")));

    (
        HostMetrics {
            cpu_load_pct,
            mem,
            cpu_mhz,
            uptime_secs,
        },
        sample,
    )
}

/// Nominal MHz scaled by the `% Processor Performance` delta between two samples.
fn effective_mhz(nominal: f64, prev: PerfSample, cur: PerfSample) -> Option<f64> {
    let d_perf = cur.0 - prev.0;
    let d_base = cur.1 - prev.1;
    (d_base > 0.0 && d_perf >= 0.0).then(|| nominal * d_perf / d_base / 100.0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_mhz_scales_nominal_by_perf_delta() {
        let mhz = effective_mhz(4300.0, (1000.0, 10.0), (13500.0, 110.0)).unwrap();
        assert!((mhz - 5375.0).abs() < 1e-6);
        assert_eq!(effective_mhz(4300.0, (1000.0, 10.0), (1000.0, 10.0)), None);
        assert_eq!(effective_mhz(4300.0, (2000.0, 10.0), (1000.0, 20.0)), None);
    }
}
