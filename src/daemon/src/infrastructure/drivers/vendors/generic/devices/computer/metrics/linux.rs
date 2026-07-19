// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "linux")]
//! Linux host-metrics backend, reading the numbers from `/proc`.

use async_trait::async_trait;
use std::sync::Mutex;

use super::{
    cpu_load_pct, parse_cpu_mhz, parse_meminfo, parse_proc_stat, parse_uptime, HostMetrics,
    HostMetricsBackend,
};

#[derive(Default)]
pub struct LinuxMetrics {
    /// Previous `(idle, total)` jiffies, for the CPU-load delta between polls.
    prev_cpu: Mutex<Option<(u64, u64)>>,
}

#[async_trait]
impl HostMetricsBackend for LinuxMetrics {
    async fn read(&self) -> HostMetrics {
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        let (stat, meminfo, cpuinfo, uptime) = tokio::task::spawn_blocking(move || {
            (
                read("/proc/stat"),
                read("/proc/meminfo"),
                read("/proc/cpuinfo"),
                read("/proc/uptime"),
            )
        })
        .await
        .unwrap_or_default();

        let cpu_load_pct = parse_proc_stat(&stat).and_then(|cur| {
            let mut prev = self.prev_cpu.lock().unwrap();
            let load = prev.and_then(|p| cpu_load_pct(p, cur));
            *prev = Some(cur);
            load
        });

        HostMetrics {
            cpu_load_pct,
            mem: parse_meminfo(&meminfo),
            cpu_mhz: parse_cpu_mhz(&cpuinfo),
            uptime_secs: parse_uptime(&uptime),
        }
    }
}
