// SPDX-License-Identifier: GPL-3.0-or-later
//! Host metrics for the [`super::ComputerDevice`]: CPU load, memory usage, CPU
//! frequency, and uptime, surfaced as read-only [`Sensor`]s. The pure parsers
//! and the [`HostMetrics`] -> [`Sensor`] mapping live here (unit-tested); the
//! platform submodules read the raw numbers (`/proc` on Linux, WMI on Windows).

use async_trait::async_trait;
use halod_shared::types::{Sensor, SensorType, SensorUnit};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Stable sensor ids (also used as the visibility key).
const ID_CPU_LOAD: &str = "computer_cpu_load";
const ID_MEMORY: &str = "computer_memory";
const ID_CPU_FREQ: &str = "computer_cpu_freq";
const ID_UPTIME: &str = "computer_uptime";

/// Memory usage, already reduced to what the UI shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MemInfo {
    pub used_pct: f64,
    pub used_gib: f64,
    pub total_gib: f64,
}

impl MemInfo {
    /// Construct from kibibyte values (as returned by `/proc/meminfo` and WMI).
    pub fn from_kib(used_kib: f64, total_kib: f64) -> Option<Self> {
        if total_kib <= 0.0 {
            return None;
        }
        let used = used_kib.max(0.0);
        Some(Self {
            used_pct: (used / total_kib * 100.0).clamp(0.0, 100.0),
            used_gib: used / 1_048_576.0,
            total_gib: total_kib / 1_048_576.0,
        })
    }
}

/// One poll's worth of host metrics. Each field is `None` when unavailable, so a
/// platform can surface only what it can read.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostMetrics {
    pub cpu_load_pct: Option<f64>,
    pub mem: Option<MemInfo>,
    pub cpu_mhz: Option<f64>,
    pub uptime_secs: Option<f64>,
}

/// Map a metrics snapshot to the wire sensors, skipping anything unavailable.
pub fn to_sensors(m: &HostMetrics) -> Vec<Sensor> {
    let mut out = Vec::new();
    if let Some(load) = m.cpu_load_pct {
        out.push(sensor(
            ID_CPU_LOAD,
            "CPU Load",
            load,
            SensorUnit::Percent,
            SensorType::Load,
        ));
    }
    if let Some(mem) = m.mem {
        out.push(sensor(
            ID_MEMORY,
            &format!("Memory ({:.1}/{:.1} GiB)", mem.used_gib, mem.total_gib),
            mem.used_pct,
            SensorUnit::Percent,
            SensorType::Memory,
        ));
    }
    if let Some(mhz) = m.cpu_mhz {
        out.push(sensor(
            ID_CPU_FREQ,
            "CPU Frequency",
            mhz,
            SensorUnit::Megahertz,
            SensorType::Frequency,
        ));
    }
    if let Some(secs) = m.uptime_secs {
        out.push(sensor(
            ID_UPTIME,
            "Uptime",
            secs / 3600.0,
            SensorUnit::Hours,
            SensorType::Uptime,
        ));
    }
    out
}

fn sensor(id: &str, name: &str, value: f64, unit: SensorUnit, sensor_type: SensorType) -> Sensor {
    Sensor {
        id: id.to_string(),
        name: name.to_string(),
        value,
        unit,
        sensor_type,
        visibility: Default::default(),
    }
}

/// Reads host metrics each poll. Holds any state needed to derive deltas (e.g.
/// the previous CPU jiffies used to compute load).
#[async_trait]
pub trait HostMetricsBackend: Send + Sync {
    async fn read(&self) -> HostMetrics;
}

/// The platform metrics backend. Metrics are always available on Linux/Windows,
/// so this never returns `None` there.
#[cfg(target_os = "linux")]
pub fn make_backend() -> Option<std::sync::Arc<dyn HostMetricsBackend>> {
    Some(std::sync::Arc::new(linux::LinuxMetrics::default()))
}

#[cfg(target_os = "windows")]
pub fn make_backend() -> Option<std::sync::Arc<dyn HostMetricsBackend>> {
    Some(std::sync::Arc::new(windows::WindowsMetrics))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn make_backend() -> Option<std::sync::Arc<dyn HostMetricsBackend>> {
    None
}

// --- Pure `/proc` parsers, used only by the Linux backend (exercised by tests). ---

/// `(idle, total)` jiffies from the aggregate `cpu ` line of `/proc/stat`.
#[cfg(target_os = "linux")]
pub fn parse_proc_stat(content: &str) -> Option<(u64, u64)> {
    let line = content.lines().find(|l| l.starts_with("cpu "))?;
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|t| t.parse().ok())
        .collect();
    if vals.len() < 4 {
        return None;
    }
    // Fields: user nice system idle iowait irq softirq steal ...
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0); // idle + iowait
    let total: u64 = vals.iter().sum();
    Some((idle, total))
}

/// CPU load percent from two `/proc/stat` snapshots. `None` if the interval had
/// no elapsed jiffies (e.g. identical samples).
#[cfg(target_os = "linux")]
pub fn cpu_load_pct(prev: (u64, u64), cur: (u64, u64)) -> Option<f64> {
    let d_idle = cur.0.saturating_sub(prev.0) as f64;
    let d_total = cur.1.saturating_sub(prev.1) as f64;
    if d_total <= 0.0 {
        return None;
    }
    Some(((d_total - d_idle) / d_total * 100.0).clamp(0.0, 100.0))
}

/// Parse `/proc/meminfo` into a [`MemInfo`], using `MemAvailable` for "used".
#[cfg(target_os = "linux")]
pub fn parse_meminfo(content: &str) -> Option<MemInfo> {
    let mut total_kib = None;
    let mut avail_kib = None;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            total_kib = v
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<f64>().ok());
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            avail_kib = v
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<f64>().ok());
        }
    }
    let (total, avail) = (total_kib?, avail_kib?);
    MemInfo::from_kib(total - avail, total)
}

/// Average `cpu MHz` across all cores in `/proc/cpuinfo`.
#[cfg(target_os = "linux")]
pub fn parse_cpu_mhz(content: &str) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0u32;
    for line in content.lines() {
        if let Some(v) = line
            .split_once(':')
            .and_then(|(k, v)| k.trim().eq_ignore_ascii_case("cpu MHz").then(|| v.trim()))
        {
            if let Ok(mhz) = v.parse::<f64>() {
                sum += mhz;
                n += 1;
            }
        }
    }
    (n > 0).then(|| sum / n as f64)
}

/// Uptime seconds from `/proc/uptime` (its first field).
#[cfg(target_os = "linux")]
pub fn parse_uptime(content: &str) -> Option<f64> {
    content
        .split_whitespace()
        .next()
        .and_then(|t| t.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_proc_stat_and_derives_load() {
        let prev = parse_proc_stat("cpu  100 0 100 800 0 0 0 0\nintr 1").unwrap();
        assert_eq!(prev, (800, 1000));
        let cur = parse_proc_stat("cpu  150 0 150 900 0 0 0 0").unwrap();
        assert_eq!(cur, (900, 1200));
        // 200 total elapsed, 100 idle -> 50% busy.
        assert_eq!(cpu_load_pct(prev, cur), Some(50.0));
        // Identical snapshots yield no load.
        assert_eq!(cpu_load_pct(cur, cur), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_meminfo() {
        let m = parse_meminfo(
            "MemTotal:       32000000 kB\nMemFree: 1 kB\nMemAvailable:   8000000 kB\n",
        )
        .unwrap();
        assert!((m.used_pct - 75.0).abs() < 0.01);
        assert!((m.total_gib - 30.517).abs() < 0.01);
        assert!((m.used_gib - 22.888).abs() < 0.01);
        assert!(parse_meminfo("MemTotal: 0 kB\nMemAvailable: 0 kB").is_none());
    }

    #[test]
    fn mem_info_from_kib_rejects_zero_total_and_clamps() {
        assert!(MemInfo::from_kib(0.0, 0.0).is_none());
        let m = MemInfo::from_kib(16_000_000.0, 32_000_000.0).unwrap();
        assert!((m.used_pct - 50.0).abs() < 0.01);
        assert!((m.used_gib - 15.258).abs() < 0.01);
        assert!((m.total_gib - 30.517).abs() < 0.01);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_cpu_mhz_average_and_uptime() {
        let info = "processor: 0\ncpu MHz\t\t: 3000.0\nprocessor: 1\ncpu MHz\t\t: 4000.0\n";
        assert_eq!(parse_cpu_mhz(info), Some(3500.0));
        assert_eq!(parse_cpu_mhz("no freq here"), None);
        assert_eq!(parse_uptime("12345.67 89000.0"), Some(12345.67));
    }

    #[test]
    fn to_sensors_skips_unavailable_and_maps_units() {
        let empty = to_sensors(&HostMetrics::default());
        assert!(empty.is_empty());

        let full = to_sensors(&HostMetrics {
            cpu_load_pct: Some(42.0),
            mem: Some(MemInfo {
                used_pct: 50.0,
                used_gib: 16.0,
                total_gib: 32.0,
            }),
            cpu_mhz: Some(3600.0),
            uptime_secs: Some(7200.0),
        });
        assert_eq!(full.len(), 4);
        assert_eq!(full[0].id, ID_CPU_LOAD);
        assert!(matches!(full[0].unit, SensorUnit::Percent));
        assert!(matches!(full[0].sensor_type, SensorType::Load));
        assert!(full[1].name.contains("16.0/32.0 GiB"));
        assert!(matches!(full[2].unit, SensorUnit::Megahertz));
        assert_eq!(full[3].value, 2.0); // 7200s -> 2h
        assert!(matches!(full[3].unit, SensorUnit::Hours));
    }
}
