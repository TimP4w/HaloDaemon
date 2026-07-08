// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026-present HaloDaemon contributors

#![cfg(target_os = "linux")]

//! `nvidia-smi` thermal helper for Linux.
//!
//! NVIDIA exposes no public sysfs thermal node for its proprietary driver, so
//! on Linux we shell out to the `nvidia-smi` CLI (shipped with the driver) to
//! enumerate GPUs and read their temperatures. Queries use the stable
//! `--format=csv,noheader,nounits` machine output.

use anyhow::{anyhow, Result};
use tokio::process::Command;

/// A GPU as reported by `nvidia-smi`.
pub struct SmiGpu {
    /// Stable per-GPU UUID (e.g. `GPU-54968926-…`), constant across reboots.
    pub uuid: String,
    /// Full board name (e.g. `NVIDIA GeForce RTX 5080`).
    pub name: String,
}

/// A single temperature reading for a GPU.
pub struct SmiReading {
    pub label: &'static str,
    pub temperature_c: f64,
}

/// Run `nvidia-smi` with the given arguments, returning trimmed stdout on
/// success. Returns `Err` if the binary is missing or exits non-zero.
async fn run_smi(args: &[&str]) -> Result<String> {
    let output = Command::new("nvidia-smi")
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("failed to spawn nvidia-smi: {e}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "nvidia-smi exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Enumerate the NVIDIA GPUs visible to `nvidia-smi`. Returns an empty list (not
/// an error) when the driver/CLI is absent or no GPU is present.
pub async fn enumerate_gpus() -> Vec<SmiGpu> {
    let stdout = match run_smi(&["--query-gpu=uuid,name", "--format=csv,noheader,nounits"]).await {
        Ok(s) => s,
        Err(e) => {
            log::debug!("[nvidia-smi] enumerate failed: {e}");
            return vec![];
        }
    };

    parse_gpu_list(&stdout)
}

/// Parse `nvidia-smi --query-gpu=uuid,name` CSV output into GPU descriptors.
/// Lines with an empty UUID are skipped. Extracted for testability.
fn parse_gpu_list(stdout: &str) -> Vec<SmiGpu> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut cols = line.split(',').map(str::trim);
            let uuid = cols.next()?.to_string();
            let name = cols.next()?.to_string();
            if uuid.is_empty() {
                return None;
            }
            Some(SmiGpu { uuid, name })
        })
        .collect()
}

/// Read the temperatures for a single GPU, addressed by its UUID. Sensors that
/// report `N/A` (unsupported on the part) are skipped.
pub async fn read_temperatures(uuid: &str) -> Result<Vec<SmiReading>> {
    let stdout = run_smi(&[
        "-i",
        uuid,
        "--query-gpu=temperature.gpu,temperature.memory",
        "--format=csv,noheader,nounits",
    ])
    .await?;

    let line = stdout
        .lines()
        .next()
        .ok_or_else(|| anyhow!("nvidia-smi returned no rows for {uuid}"))?;

    Ok(parse_temperature_line(line, &["GPU Core", "GPU Memory"]))
}

/// Parse a single CSV line from `nvidia-smi --query-gpu=temperature.gpu,...`.
/// Columns that parse as `N/A` or otherwise unparseable are silently skipped.
fn parse_temperature_line(line: &str, labels: &[&'static str]) -> Vec<SmiReading> {
    let mut readings = Vec::new();
    for (label, col) in labels.iter().copied().zip(line.split(',').map(str::trim)) {
        if let Ok(value) = col.parse::<f64>() {
            readings.push(SmiReading {
                label,
                temperature_c: value,
            });
        }
    }
    readings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gpu_list_multiple_gpus() {
        let csv = "GPU-aabb0011, NVIDIA GeForce RTX 5080\nGPU-ccdd0022, NVIDIA GeForce RTX 4090\n";
        let gpus = parse_gpu_list(csv);
        assert_eq!(gpus.len(), 2);
        assert_eq!(gpus[0].uuid, "GPU-aabb0011");
        assert_eq!(gpus[1].name, "NVIDIA GeForce RTX 4090");
    }

    #[test]
    fn parse_gpu_list_skips_empty_uuid() {
        let csv = ", NVIDIA GeForce RTX 5080\nGPU-ccdd0022, NVIDIA GeForce RTX 4090\n";
        let gpus = parse_gpu_list(csv);
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].uuid, "GPU-ccdd0022");
    }

    #[test]
    fn parse_gpu_list_empty_input() {
        assert!(parse_gpu_list("").is_empty());
    }

    #[test]
    fn parse_temperature_line_both_sensors() {
        let readings = parse_temperature_line("50, 70", &["GPU Core", "GPU Memory"]);
        assert_eq!(readings.len(), 2);
        assert_eq!(readings[0].temperature_c, 50.0);
        assert_eq!(readings[0].label, "GPU Core");
        assert_eq!(readings[1].temperature_c, 70.0);
    }

    #[test]
    fn parse_temperature_line_skips_na() {
        let readings = parse_temperature_line("65, N/A", &["GPU Core", "GPU Memory"]);
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].temperature_c, 65.0);
        assert_eq!(readings[0].label, "GPU Core");
    }

    #[test]
    fn parse_temperature_line_empty_string() {
        assert!(parse_temperature_line("", &["GPU Core"]).is_empty());
    }
}
