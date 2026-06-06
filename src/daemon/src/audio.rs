/// Handle to the PulseAudio null-sinks created for ChatMix. The fields are only
/// read by the Linux teardown/volume paths; on Windows the stub treats it as an
/// opaque token.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub struct ChatMixSinks {
    media_name: String,
    chat_name: String,
    module_ids: Vec<u32>,
}

#[cfg(not(target_os = "windows"))]
pub use linux::*;

#[cfg(target_os = "windows")]
pub use windows_stub::*;

#[cfg(not(target_os = "windows"))]
mod linux {
    use super::*;
    use anyhow::Result;
    use tokio::process::Command;

    /// Derives the internal PulseAudio sink name from the full device display name.
    /// "SteelSeries Arctis Nova Pro Wireless" → "steelseries_arctis_nova_pro_wireless"
    fn sink_base_name(vendor: &str, model: &str) -> String {
        format!("{} {}", vendor, model)
            .to_lowercase()
            .replace(' ', "_")
    }

    async fn pactl_load_module(args: &[&str]) -> Result<u32> {
        let output = Command::new("pactl")
            .arg("load-module")
            .args(args)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("pactl load-module failed: {stderr}");
        }

        let id_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        id_str
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("pactl returned non-numeric module ID: {id_str}"))
    }

    /// Finds the physical SteelSeries USB audio sink (VID 0x1038).
    pub async fn find_arctis_sink() -> Option<String> {
        let output = Command::new("pactl")
            .args(["--format=json", "list", "sinks"])
            .output()
            .await
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let json_str = String::from_utf8_lossy(&output.stdout);
        parse_arctis_sink_from_json(&json_str)
    }

    fn parse_arctis_sink_from_json(json_str: &str) -> Option<String> {
        let json: serde_json::Value = serde_json::from_str(json_str).ok()?;
        let sinks = json.as_array()?;

        for sink in sinks {
            let props = sink.get("properties")?;
            let vendor_id = props.get("device.vendor.id")?.as_str().unwrap_or("");
            if !vendor_id.eq_ignore_ascii_case("0x1038") {
                continue;
            }
            let name = sink.get("name")?.as_str()?;
            // Skip virtual sinks we created
            if name.ends_with("_media") || name.ends_with("_chat") {
                continue;
            }
            return Some(name.to_string());
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn make_sink(name: &str, vendor_id: &str) -> serde_json::Value {
            serde_json::json!({
                "name": name,
                "properties": {
                    "device.vendor.id": vendor_id
                }
            })
        }

        #[test]
        fn finds_steelseries_sink() {
            let json = serde_json::json!([
                make_sink("alsa_output.usb-SteelSeries_Arctis_Nova_Pro-00.analog-stereo", "0x1038")
            ])
            .to_string();
            assert_eq!(
                parse_arctis_sink_from_json(&json),
                Some("alsa_output.usb-SteelSeries_Arctis_Nova_Pro-00.analog-stereo".to_string())
            );
        }

        #[test]
        fn skips_other_vendor_sinks() {
            let json = serde_json::json!([
                make_sink("alsa_output.pci-0000_00_1f.3.analog-stereo", "0x8086")
            ])
            .to_string();
            assert_eq!(parse_arctis_sink_from_json(&json), None);
        }

        #[test]
        fn skips_virtual_media_sink() {
            let json = serde_json::json!([
                make_sink("steelseries_arctis_nova_pro_wireless_media", "0x1038")
            ])
            .to_string();
            assert_eq!(parse_arctis_sink_from_json(&json), None);
        }

        #[test]
        fn skips_virtual_chat_sink() {
            let json = serde_json::json!([
                make_sink("steelseries_arctis_nova_pro_wireless_chat", "0x1038")
            ])
            .to_string();
            assert_eq!(parse_arctis_sink_from_json(&json), None);
        }

        #[test]
        fn picks_physical_when_virtuals_also_present() {
            let json = serde_json::json!([
                make_sink("steelseries_arctis_nova_pro_wireless_media", "0x1038"),
                make_sink("steelseries_arctis_nova_pro_wireless_chat", "0x1038"),
                make_sink("alsa_output.usb-SteelSeries_Arctis_Nova_Pro-00.analog-stereo", "0x1038"),
            ])
            .to_string();
            assert_eq!(
                parse_arctis_sink_from_json(&json),
                Some("alsa_output.usb-SteelSeries_Arctis_Nova_Pro-00.analog-stereo".to_string())
            );
        }

        #[test]
        fn returns_none_for_empty_array() {
            assert_eq!(parse_arctis_sink_from_json("[]"), None);
        }

        #[test]
        fn returns_none_for_invalid_json() {
            assert_eq!(parse_arctis_sink_from_json("not json"), None);
        }

        #[test]
        fn vendor_id_match_is_case_insensitive() {
            let json = serde_json::json!([
                make_sink("alsa_output.usb-SteelSeries_Arctis-00.analog-stereo", "0x1038"),
            ])
            .to_string();
            // uppercase variant
            let json_upper = json.replace("0x1038", "0X1038");
            assert!(parse_arctis_sink_from_json(&json_upper).is_some());
        }

        #[test]
        fn sink_base_name_standard() {
            assert_eq!(
                sink_base_name("SteelSeries", "Arctis Nova Pro Wireless"),
                "steelseries_arctis_nova_pro_wireless"
            );
        }

        #[test]
        fn sink_base_name_x_variant() {
            assert_eq!(
                sink_base_name("SteelSeries", "Arctis Nova Pro Wireless X"),
                "steelseries_arctis_nova_pro_wireless_x"
            );
        }
    }

    pub async fn setup_chatmix_sinks(vendor: &str, model: &str) -> Option<ChatMixSinks> {
        let physical_sink = match find_arctis_sink().await {
            Some(s) => s,
            None => {
                log::warn!("ChatMix: no physical SteelSeries audio sink found; skipping sink setup");
                return None;
            }
        };

        let base = sink_base_name(vendor, model);
        let media_name = format!("{base}_media");
        let chat_name = format!("{base}_chat");
        let full_name = format!("{vendor} {model}");
        let media_desc = format!("{full_name} Media");
        let chat_desc = format!("{full_name} Chat");

        let mut module_ids = Vec::new();

        match create_null_sink(&media_name, &media_desc).await {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                log::warn!("ChatMix: failed to create media sink: {e}");
                return None;
            }
        }
        match create_loopback(&media_name, &physical_sink).await {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                log::warn!("ChatMix: failed to create media loopback: {e}");
                teardown_by_ids(&module_ids).await;
                return None;
            }
        }
        match create_null_sink(&chat_name, &chat_desc).await {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                log::warn!("ChatMix: failed to create chat sink: {e}");
                teardown_by_ids(&module_ids).await;
                return None;
            }
        }
        match create_loopback(&chat_name, &physical_sink).await {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                log::warn!("ChatMix: failed to create chat loopback: {e}");
                teardown_by_ids(&module_ids).await;
                return None;
            }
        }

        log::info!("ChatMix: created sinks '{media_name}' and '{chat_name}' → '{physical_sink}'");
        Some(ChatMixSinks { media_name, chat_name, module_ids })
    }

    async fn create_null_sink(name: &str, description: &str) -> Result<u32> {
        pactl_load_module(&[
            "module-null-sink",
            &format!("sink_name={name}"),
            // Outer '"' is the first char after '=', triggering PulseAudio quote mode so the
            // space-separated description is preserved as a single value.
            &format!("sink_properties=\"node.description='{description}'\""),
        ])
        .await
    }

    async fn create_loopback(source_sink: &str, dest_sink: &str) -> Result<u32> {
        pactl_load_module(&[
            "module-loopback",
            &format!("source={source_sink}.monitor"),
            &format!("sink={dest_sink}"),
            "latency_msec=0",
        ])
        .await
    }

    async fn teardown_by_ids(ids: &[u32]) {
        for &id in ids {
            let _ = Command::new("pactl")
                .args(["unload-module", &id.to_string()])
                .output()
                .await;
        }
    }

    pub async fn teardown_chatmix_sinks(sinks: ChatMixSinks) {
        log::info!(
            "ChatMix: removing sinks '{}' and '{}'",
            sinks.media_name,
            sinks.chat_name
        );
        teardown_by_ids(&sinks.module_ids).await;
    }

    pub async fn set_chatmix_volume(sinks: &ChatMixSinks, media_pct: u8, chat_pct: u8) {
        for (name, pct) in [(&sinks.media_name, media_pct), (&sinks.chat_name, chat_pct)] {
            let _ = Command::new("pactl")
                .args(["set-sink-volume", name, &format!("{pct}%")])
                .output()
                .await;
        }
    }
}

#[cfg(target_os = "windows")]
mod windows_stub {
    use super::*;

    pub async fn setup_chatmix_sinks(_vendor: &str, _model: &str) -> Option<ChatMixSinks> {
        log::warn!("ChatMix: PulseAudio sink creation is not supported on Windows");
        None
    }

    pub async fn teardown_chatmix_sinks(_sinks: ChatMixSinks) {}

    pub async fn set_chatmix_volume(_sinks: &ChatMixSinks, _media_pct: u8, _chat_pct: u8) {}
}
