/// A virtual PulseAudio/PipeWire sink created on behalf of a device and looped
/// into that device's physical sink. Drivers obtain one via [`register_sink`]
/// and drive its volume with [`Sink::set_volume`]; they must call
/// [`Sink::remove`] to tear it down (pactl teardown is async, so it can't run
/// from `Drop`).
///
/// The fields are only read by the Linux implementation; on Windows the stub
/// treats it as an opaque token.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub struct Sink {
    name: String,
    module_ids: Vec<u32>,
}

#[cfg(not(target_os = "windows"))]
pub use linux::*;

#[cfg(target_os = "windows")]
pub use windows_stub::*;

#[cfg(not(target_os = "windows"))]
mod linux {
    use super::Sink;
    use anyhow::Result;
    use std::time::Duration;
    use tokio::process::Command;

    const SINK_PREFIX: &str = crate::constants::AUDIO_SINK_PREFIX;
    const PACTL_TIMEOUT: Duration = Duration::from_secs(5);

    async fn pactl_output(cmd: &mut Command) -> std::io::Result<std::process::Output> {
        match tokio::time::timeout(PACTL_TIMEOUT, cmd.output()).await {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "pactl timed out after 5 seconds",
            )),
        }
    }

    impl Sink {
        /// Set this sink's playback volume, as a percentage.
        pub async fn set_volume(&self, pct: u8) {
            match pactl_output(Command::new("pactl").args([
                "set-sink-volume",
                &self.name,
                &format!("{pct}%"),
            ]))
            .await
            {
                Ok(o) if !o.status.success() => log::warn!(
                    "audio: set-sink-volume failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => log::warn!("audio: could not run pactl for set-sink-volume: {e}"),
                Ok(_) => {}
            }
        }

        /// Tear the sink down, unloading the pactl modules backing it.
        pub async fn remove(&self) {
            unload_all(&self.module_ids).await;
        }
    }

    /// Create a virtual null-sink named `name`, looped into the physical sink of
    /// the USB audio device identified by `vid`/`pid`. Returns `None` if that
    /// device has no sink, or if the sink/loopback could not be created.
    pub async fn register_sink(vid: u16, pid: u16, name: &str) -> Option<Sink> {
        let physical_sink = find_physical_sink(vid, pid).await?;
        let sink_name = sanitize(name);

        let mut module_ids = Vec::new();
        match create_null_sink(&sink_name, name).await {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                log::warn!("audio: failed to create sink '{sink_name}': {e}");
                return None;
            }
        }
        match create_loopback(&sink_name, &physical_sink).await {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                log::warn!("audio: failed to create loopback for '{sink_name}': {e}");
                unload_all(&module_ids).await;
                return None;
            }
        }

        log::info!("audio: registered sink '{sink_name}' → '{physical_sink}'");
        Some(Sink {
            name: sink_name,
            module_ids,
        })
    }

    /// Sanitize a display name into a PulseAudio sink name, stamped with
    /// [`SINK_PREFIX`] so the sink is recognizable as halod-managed:
    /// "SteelSeries Arctis Nova Pro Wireless Media" → "halod_steelseries_arctis_nova_pro_wireless_media".
    /// Only `[a-z0-9_-]` characters are kept; anything else becomes `_`.
    fn sanitize(name: &str) -> String {
        let sanitized: String = name
            .to_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("{SINK_PREFIX}{sanitized}")
    }

    /// Unload halod-managed null-sink/loopback modules orphaned by a previous
    /// daemon. Safe once single-instance ownership is established: no live
    /// daemon owns these modules.
    pub async fn cleanup_orphaned_sinks() {
        let output =
            match pactl_output(Command::new("pactl").args(["list", "modules", "short"])).await {
                Ok(o) if o.status.success() => o,
                Ok(o) => {
                    log::warn!(
                        "audio: 'pactl list modules short' failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                    return;
                }
                Err(e) => {
                    log::warn!("audio: could not run pactl to clean up orphaned sinks: {e}");
                    return;
                }
            };

        let ids = parse_orphan_module_ids(&String::from_utf8_lossy(&output.stdout));
        if !ids.is_empty() {
            log::info!("audio: reclaiming {} orphaned sink module(s)", ids.len());
            unload_all(&ids).await;
        }
    }

    /// Parse `pactl list modules short` output (tab-separated
    /// `index<TAB>name<TAB>argument`) for halod-managed null-sink/loopback
    /// modules, identified by [`SINK_PREFIX`] appearing in the argument.
    fn parse_orphan_module_ids(short_output: &str) -> Vec<u32> {
        short_output
            .lines()
            .filter_map(|line| {
                let mut cols = line.split('\t');
                let index = cols.next()?;
                let name = cols.next()?;
                let argument = cols.next()?;
                let is_managed_kind = name == "module-null-sink" || name == "module-loopback";
                if is_managed_kind && argument.contains(SINK_PREFIX) {
                    index.trim().parse::<u32>().ok()
                } else {
                    None
                }
            })
            .collect()
    }

    /// Locate the physical sink for a USB device by matching the PipeWire/PulseAudio
    /// `device.vendor.id` / `device.product.id` properties (e.g. "0x1038"/"0x12e0").
    async fn find_physical_sink(vid: u16, pid: u16) -> Option<String> {
        let output = match pactl_output(Command::new("pactl").args([
            "--format=json",
            "list",
            "sinks",
        ]))
        .await
        {
            Ok(o) => o,
            Err(e) => {
                log::warn!("audio: could not run pactl (is it on PATH?): {e}");
                return None;
            }
        };

        if !output.status.success() {
            log::warn!(
                "audio: 'pactl list sinks' failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return None;
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        let sink = parse_physical_sink(&json_str, vid, pid);
        if sink.is_none() {
            log::warn!("audio: no sink found for device {vid:#06x}:{pid:#06x}");
        }
        sink
    }

    fn parse_physical_sink(json_str: &str, vid: u16, pid: u16) -> Option<String> {
        let want_vid = format!("{vid:#06x}");
        let want_pid = format!("{pid:#06x}");

        let json: serde_json::Value = serde_json::from_str(json_str).ok().or_else(|| {
            log::warn!("audio: failed to parse 'pactl list sinks' JSON");
            None
        })?;
        for sink in json.as_array()? {
            // Virtual/null sinks lack these props — skip, don't abort the scan.
            let Some(props) = sink.get("properties") else {
                continue;
            };
            let Some(vendor_id) = props.get("device.vendor.id").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(product_id) = props.get("device.product.id").and_then(|v| v.as_str()) else {
                continue;
            };
            if !vendor_id.eq_ignore_ascii_case(&want_vid)
                || !product_id.eq_ignore_ascii_case(&want_pid)
            {
                continue;
            }
            if let Some(name) = sink.get("name").and_then(|v| v.as_str()) {
                return Some(name.to_string());
            }
        }
        None
    }

    async fn create_null_sink(name: &str, description: &str) -> Result<u32> {
        load_module(&[
            "module-null-sink",
            &format!("sink_name={name}"),
            // Outer '"' is the first char after '=', triggering PulseAudio quote mode so the
            // space-separated description is preserved as a single value. Single quotes inside
            // the description would end the value early, so they are escaped.
            &format!(
                "sink_properties=\"node.description='{}'\"",
                description.replace('\'', "\\'")
            ),
        ])
        .await
    }

    async fn create_loopback(source_sink: &str, dest_sink: &str) -> Result<u32> {
        load_module(&[
            "module-loopback",
            &format!("source={source_sink}.monitor"),
            &format!("sink={dest_sink}"),
            "latency_msec=0",
        ])
        .await
    }

    async fn load_module(args: &[&str]) -> Result<u32> {
        let output = pactl_output(Command::new("pactl").arg("load-module").args(args)).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("pactl load-module failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let id_str = stdout.trim();
        id_str
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("pactl returned non-numeric module ID: {id_str}"))
    }

    async fn unload_all(ids: &[u32]) {
        for &id in ids {
            let output =
                pactl_output(Command::new("pactl").args(["unload-module", &id.to_string()])).await;
            match output {
                Ok(o) if !o.status.success() => {
                    log::warn!(
                        "audio: failed to unload module {id}: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                }
                Err(e) => log::warn!("audio: failed to run pactl to unload module {id}: {e}"),
                _ => {}
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn make_sink(name: &str, vendor_id: &str, product_id: &str) -> serde_json::Value {
            serde_json::json!({
                "name": name,
                "properties": {
                    "device.vendor.id": vendor_id,
                    "device.product.id": product_id,
                }
            })
        }

        #[test]
        fn finds_sink_matching_vid_and_pid() {
            let json = serde_json::json!([make_sink(
                "alsa_output.usb-SteelSeries_Arctis_Nova_Pro-00.analog-stereo",
                "0x1038",
                "0x12e0"
            )])
            .to_string();
            assert_eq!(
                parse_physical_sink(&json, 0x1038, 0x12e0),
                Some("alsa_output.usb-SteelSeries_Arctis_Nova_Pro-00.analog-stereo".to_string())
            );
        }

        #[test]
        fn skips_matching_vendor_but_wrong_product() {
            let json =
                serde_json::json!([make_sink("alsa_output.other", "0x1038", "0xffff")]).to_string();
            assert_eq!(parse_physical_sink(&json, 0x1038, 0x12e0), None);
        }

        #[test]
        fn skips_other_vendor_sinks() {
            let json = serde_json::json!([make_sink(
                "alsa_output.pci-0000_00_1f.3.analog-stereo",
                "0x8086",
                "0x1234"
            )])
            .to_string();
            assert_eq!(parse_physical_sink(&json, 0x1038, 0x12e0), None);
        }

        #[test]
        fn skips_sinks_without_device_ids_without_aborting() {
            let json = serde_json::json!([
                { "name": "some_null_sink", "properties": { "media.class": "Audio/Sink" } },
                make_sink("alsa_output.usb-real.analog-stereo", "0x1038", "0x12e0"),
            ])
            .to_string();
            assert_eq!(
                parse_physical_sink(&json, 0x1038, 0x12e0),
                Some("alsa_output.usb-real.analog-stereo".to_string())
            );
        }

        #[test]
        fn returns_none_for_empty_array() {
            assert_eq!(parse_physical_sink("[]", 0x1038, 0x12e0), None);
        }

        #[test]
        fn returns_none_for_invalid_json() {
            assert_eq!(parse_physical_sink("not json", 0x1038, 0x12e0), None);
        }

        #[test]
        fn vendor_and_product_match_is_case_insensitive() {
            let json = serde_json::json!([make_sink(
                "alsa_output.usb-SteelSeries_Arctis-00.analog-stereo",
                "0X1038",
                "0X12E0"
            )])
            .to_string();
            assert!(parse_physical_sink(&json, 0x1038, 0x12e0).is_some());
        }

        #[test]
        fn sanitize_standard() {
            assert_eq!(
                sanitize("SteelSeries Arctis Nova Pro Wireless Media"),
                "halod_steelseries_arctis_nova_pro_wireless_media"
            );
        }

        #[test]
        fn sanitize_strips_special_characters() {
            assert_eq!(
                sanitize("Device (Pro's & More!)"),
                "halod_device__pro_s___more__"
            );
        }

        #[test]
        fn sanitize_preserves_hyphens() {
            assert_eq!(sanitize("Arctis Nova Pro-X"), "halod_arctis_nova_pro-x");
        }

        #[test]
        fn parses_managed_null_sink_and_loopback_ids() {
            let short = "\
536870939\tmodule-null-sink\tsink_name=halod_arctis_media sink_properties=node.description='Arctis Media'\t
536870940\tmodule-loopback\tsource=halod_arctis_media.monitor sink=alsa_output.usb-real latency_msec=0\t";
            assert_eq!(parse_orphan_module_ids(short), vec![536870939, 536870940]);
        }

        #[test]
        fn ignores_unmanaged_and_foreign_modules() {
            let short = "\
1\tlibpipewire-module-rt\t{ nice.level = -11 }\t
2\tmodule-null-sink\tsink_name=some_other_app_sink\t
3\tmodule-loopback\tsource=microphone.monitor sink=speakers\t";
            assert!(parse_orphan_module_ids(short).is_empty());
        }

        #[test]
        fn skips_malformed_lines_without_panicking() {
            let short = "garbage\n\t\tno-index\nnotanumber\tmodule-null-sink\tsink_name=halod_x\t";
            assert!(parse_orphan_module_ids(short).is_empty());
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_stub {
    use super::Sink;

    impl Sink {
        pub async fn set_volume(&self, _pct: u8) {}
        pub async fn remove(&self) {}
    }

    pub async fn register_sink(_vid: u16, _pid: u16, _name: &str) -> Option<Sink> {
        log::warn!("audio: virtual sink creation is not supported on Windows");
        None
    }

    pub async fn cleanup_orphaned_sinks() {}
}
