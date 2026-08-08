// SPDX-License-Identifier: GPL-3.0-or-later
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
    use std::collections::HashSet;
    use std::sync::{LazyLock, Mutex, MutexGuard};
    use std::time::Duration;
    use tokio::process::Command;
    use tokio::time::sleep;

    const SINK_PREFIX: &str = crate::constants::AUDIO_SINK_PREFIX;
    const PACTL_TIMEOUT: Duration = Duration::from_secs(5);

    /// A session that has just started keeps publishing nodes for seconds after
    /// it accepts connections, so lookups poll. Budgets are hard wall-clock
    /// bounds: `register_sink` runs inside a plugin callback the watchdog kills
    /// at 30 s.
    const READY_POLL: Duration = Duration::from_millis(250);
    const SINK_BUDGET: Duration = Duration::from_secs(5);
    const MONITOR_BUDGET: Duration = Duration::from_secs(3);
    const STRAY_BUDGET: Duration = Duration::from_secs(1);

    /// Module ids held by live sinks. Sink names are not unique across devices,
    /// so name-keyed sweeps must skip these.
    static OWNED_MODULES: LazyLock<Mutex<HashSet<u32>>> = LazyLock::new(Mutex::default);

    fn owned_modules() -> MutexGuard<'static, HashSet<u32>> {
        OWNED_MODULES.lock().unwrap_or_else(|e| e.into_inner())
    }

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
                teardown_failed_registration(&sink_name, &module_ids, is_timeout(&e)).await;
                return None;
            }
        }
        // A loopback whose `source=` does not resolve is not rejected: it
        // autoconnects to the default source — a microphone — and feeds it into
        // the device's own speakers.
        if !wait_for_monitor_source(&sink_name).await {
            log::warn!("audio: monitor for '{sink_name}' never appeared; skipping loopback");
            teardown_failed_registration(&sink_name, &module_ids, false).await;
            return None;
        }
        match create_loopback(&sink_name, &physical_sink).await {
            Ok(id) => module_ids.push(id),
            Err(e) => {
                log::warn!("audio: failed to create loopback for '{sink_name}': {e}");
                teardown_failed_registration(&sink_name, &module_ids, is_timeout(&e)).await;
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
    /// "Acme Headset Pro Wireless Media" → "halod_acme_headset_pro_wireless_media".
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
        // Orphans die with the audio server, so a server that isn't answering
        // has nothing to reclaim: one attempt, no polling.
        match pactl_list(&["list", "modules", "short"]).await {
            Listing::Ready(modules) => {
                let ids = parse_orphan_module_ids(&modules);
                if !ids.is_empty() {
                    log::info!("audio: reclaiming {} orphaned sink module(s)", ids.len());
                    unload_all(&ids).await;
                }
            }
            Listing::NotReady(reason) => {
                log::warn!("audio: skipped orphaned-sink cleanup: {reason}");
            }
            Listing::Unavailable => {}
        }
    }

    /// `Unavailable` is the one outcome worth not retrying: no pactl at all.
    enum Listing {
        Ready(String),
        NotReady(String),
        Unavailable,
    }

    async fn pactl_list(args: &[&str]) -> Listing {
        match pactl_output(Command::new("pactl").args(args)).await {
            Ok(o) if o.status.success() => Listing::Ready(
                String::from_utf8(o.stdout)
                    .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()),
            ),
            Ok(o) => Listing::NotReady(String::from_utf8_lossy(&o.stderr).trim().to_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::warn!("audio: pactl is not on PATH");
                Listing::Unavailable
            }
            Err(e) => Listing::NotReady(e.to_string()),
        }
    }

    /// Poll a `pactl` listing until `extract` yields a value or `budget` is
    /// spent. The budget is a hard bound: a call still in flight at the
    /// deadline is abandoned.
    async fn poll_listing<T>(
        args: &[&str],
        budget: Duration,
        extract: impl Fn(&str) -> Option<T>,
    ) -> Option<T> {
        let what = format!("pactl {}", args.join(" "));
        poll_with(&what, budget, || pactl_list(args), extract).await
    }

    async fn poll_with<T, F, Fut>(
        what: &str,
        budget: Duration,
        mut source: F,
        extract: impl Fn(&str) -> Option<T>,
    ) -> Option<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Listing>,
    {
        let deadline = tokio::time::Instant::now() + budget;
        let mut last_error = None;
        loop {
            match tokio::time::timeout_at(deadline, source()).await {
                Ok(Listing::Ready(out)) => {
                    if let Some(found) = extract(&out) {
                        return Some(found);
                    }
                    last_error = None;
                }
                Ok(Listing::NotReady(reason)) => last_error = Some(reason),
                Ok(Listing::Unavailable) => return None,
                Err(_) => break,
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            sleep(READY_POLL).await;
        }
        match last_error {
            Some(reason) => log::warn!("audio: '{what}' not ready within {budget:?}: {reason}"),
            None => log::debug!("audio: '{what}' never matched within {budget:?}"),
        }
        None
    }

    fn has_monitor_source(short_output: &str, sink_name: &str) -> bool {
        let want = format!("{sink_name}.monitor");
        short_output
            .lines()
            .any(|line| line.split('\t').nth(1) == Some(want.as_str()))
    }

    async fn wait_for_monitor_source(sink_name: &str) -> bool {
        poll_listing(&["list", "sources", "short"], MONITOR_BUDGET, |out| {
            has_monitor_source(out, sink_name).then_some(())
        })
        .await
        .is_some()
    }

    fn is_timeout(e: &anyhow::Error) -> bool {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
    }

    /// A load that timed out may still have completed server-side; hunt that
    /// stray down before unloading the ids we know, so a stray loopback never
    /// outlives the sink it captures. Owned ids are excluded: sink names are
    /// not unique across devices.
    async fn teardown_failed_registration(sink_name: &str, ids: &[u32], timed_out: bool) {
        if timed_out {
            let strays = poll_listing(&["list", "modules", "short"], STRAY_BUDGET, |out| {
                let strays: Vec<u32> = parse_sink_module_ids(out, sink_name)
                    .into_iter()
                    .filter(|id| !owned_modules().contains(id))
                    .collect();
                (!strays.is_empty()).then_some(strays)
            })
            .await
            .unwrap_or_default();
            if !strays.is_empty() {
                log::warn!(
                    "audio: unloading {} stray module(s) left by '{sink_name}'",
                    strays.len()
                );
                unload_all(&strays).await;
            }
        }
        unload_all(ids).await;
    }

    /// Parse `pactl list modules short` output (tab-separated
    /// `index<TAB>name<TAB>argument`) for halod-managed null-sink/loopback
    /// modules, identified by [`SINK_PREFIX`] appearing in the argument.
    fn parse_orphan_module_ids(short_output: &str) -> Vec<u32> {
        parse_module_ids(short_output, |argument| argument.contains(SINK_PREFIX))
    }

    /// Matched on whole argument tokens, so a sink name that merely extends
    /// `sink_name` is never torn down with it.
    fn parse_sink_module_ids(short_output: &str, sink_name: &str) -> Vec<u32> {
        let null_arg = format!("sink_name={sink_name}");
        let loopback_arg = format!("source={sink_name}.monitor");
        parse_module_ids(short_output, |argument| {
            argument
                .split_whitespace()
                .any(|token| token == null_arg || token == loopback_arg)
        })
    }

    /// Returned in load order — null sinks before the loopbacks that capture
    /// them — regardless of listing order or recycled module ids.
    fn parse_module_ids(short_output: &str, keep: impl Fn(&str) -> bool) -> Vec<u32> {
        let mut null_sinks = Vec::new();
        let mut loopbacks = Vec::new();
        for line in short_output.lines() {
            let mut cols = line.split('\t');
            let (Some(index), Some(name), Some(argument)) = (cols.next(), cols.next(), cols.next())
            else {
                continue;
            };
            let bucket = match name {
                "module-null-sink" => &mut null_sinks,
                "module-loopback" => &mut loopbacks,
                _ => continue,
            };
            if keep(argument) {
                if let Ok(id) = index.trim().parse::<u32>() {
                    bucket.push(id);
                }
            }
        }
        null_sinks.extend(loopbacks);
        null_sinks
    }

    /// Locate the physical sink for a USB device by matching the PipeWire/PulseAudio
    /// `device.vendor.id` / `device.product.id` properties (e.g. "0x1038"/"0x12e0").
    async fn find_physical_sink(vid: u16, pid: u16) -> Option<String> {
        let sink = poll_listing(&["--format=json", "list", "sinks"], SINK_BUDGET, |out| {
            parse_physical_sink(out, vid, pid)
        })
        .await;
        if sink.is_none() {
            log::warn!("audio: no sink found for device {vid:#06x}:{pid:#06x}");
        }
        sink
    }

    fn parse_physical_sink(json_str: &str, vid: u16, pid: u16) -> Option<String> {
        let want_vid = format!("{vid:#06x}");
        let want_pid = format!("{pid:#06x}");

        let json: serde_json::Value = serde_json::from_str(json_str).ok()?;
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

    /// Escape a plugin-controlled description for PulseAudio's quoted-value
    /// syntax: drop control chars, cap length, then escape `\` and `'`.
    fn escape_description(description: &str) -> String {
        description
            .chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect::<String>()
            .replace('\\', "\\\\")
            .replace('\'', "\\'")
    }

    async fn create_null_sink(name: &str, description: &str) -> Result<u32> {
        load_module(&[
            "module-null-sink",
            &format!("sink_name={name}"),
            &format!(
                "sink_properties=\"node.description='{}'\"",
                escape_description(description)
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
        let id = id_str
            .parse::<u32>()
            .map_err(|_| anyhow::anyhow!("pactl returned non-numeric module ID: {id_str}"))?;
        owned_modules().insert(id);
        Ok(id)
    }

    async fn unload_all(ids: &[u32]) {
        // ids arrive in load order; walk in reverse so a loopback is unloaded
        // before the sink it captures, which would otherwise re-bind it to the
        // default source.
        for &id in ids.iter().rev() {
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
            owned_modules().remove(&id);
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
                "alsa_output.usb-Acme_Headset_Pro-00.analog-stereo",
                "0x1038",
                "0x12e0"
            )])
            .to_string();
            assert_eq!(
                parse_physical_sink(&json, 0x1038, 0x12e0),
                Some("alsa_output.usb-Acme_Headset_Pro-00.analog-stereo".to_string())
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
                "alsa_output.usb-Acme_Headset-00.analog-stereo",
                "0X1038",
                "0X12E0"
            )])
            .to_string();
            assert!(parse_physical_sink(&json, 0x1038, 0x12e0).is_some());
        }

        #[test]
        fn sanitize_standard() {
            assert_eq!(
                sanitize("Acme Headset Pro Wireless Media"),
                "halod_acme_headset_pro_wireless_media"
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
            assert_eq!(sanitize("Headset Nova Pro-X"), "halod_headset_nova_pro-x");
        }

        #[test]
        fn escape_description_neutralizes_quotes_backslashes_and_controls() {
            assert_eq!(escape_description(r"a'b\c"), r"a\'b\\c");
            assert!(!escape_description("a\nb\tc\0").contains('\n'));
            assert!(escape_description(&"x".repeat(500)).len() <= 128 * 2);
        }

        #[test]
        fn parses_managed_null_sink_and_loopback_ids() {
            let short = "\
536870939\tmodule-null-sink\tsink_name=halod_headset_media sink_properties=node.description='Headset Media'\t
536870940\tmodule-loopback\tsource=halod_headset_media.monitor sink=alsa_output.usb-real latency_msec=0\t";
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

        const PAIR: &str = "\
10\tmodule-null-sink\tsink_name=halod_headset_media sink_properties=node.description='Media'\t
11\tmodule-loopback\tsource=halod_headset_media.monitor sink=alsa_output.usb-real latency_msec=0\t";

        #[test]
        fn sink_module_ids_match_both_modules_of_one_sink() {
            assert_eq!(
                parse_sink_module_ids(PAIR, "halod_headset_media"),
                vec![10, 11]
            );
        }

        #[test]
        fn sink_module_ids_ignore_names_that_merely_share_a_prefix() {
            assert!(parse_sink_module_ids(PAIR, "halod_headset").is_empty());
            assert!(parse_sink_module_ids(PAIR, "halod_headset_media_extra").is_empty());
        }

        #[test]
        fn monitor_source_is_detected_only_when_published() {
            let sources = "\
7\talsa_input.usb-headset.mono-fallback\tPipeWire\ts16le 1ch 48000Hz\tSUSPENDED
9\thalod_headset_media.monitor\tPipeWire\tfloat32le 2ch 48000Hz\tIDLE";
            assert!(has_monitor_source(sources, "halod_headset_media"));
            assert!(!has_monitor_source(sources, "halod_headset_chat"));
            assert!(!has_monitor_source("", "halod_headset_media"));
        }

        #[test]
        fn parse_orders_null_sinks_before_loopbacks_despite_recycled_ids() {
            // Loopback listed first with a *lower* id than its null sink, as
            // after the server recycles a freed slot.
            let short = "\
5\tmodule-loopback\tsource=halod_x.monitor sink=alsa_output.usb latency_msec=0\t
9\tmodule-null-sink\tsink_name=halod_x\t";
            assert_eq!(parse_sink_module_ids(short, "halod_x"), vec![9, 5]);
            assert_eq!(parse_orphan_module_ids(short), vec![9, 5]);
        }

        async fn scripted(outcomes: &std::sync::Mutex<Vec<Listing>>) -> Listing {
            outcomes.lock().unwrap().pop().unwrap()
        }

        #[tokio::test(start_paused = true)]
        async fn poll_returns_the_extracted_value_once_ready() {
            // Popped back-to-front: NotReady first, then the match.
            let outcomes = std::sync::Mutex::new(vec![
                Listing::Ready("match".into()),
                Listing::NotReady("booting".into()),
            ]);
            let got = poll_with(
                "t",
                Duration::from_secs(5),
                || scripted(&outcomes),
                |out| (out == "match").then(|| out.to_owned()),
            )
            .await;
            assert_eq!(got.as_deref(), Some("match"));
        }

        #[tokio::test(start_paused = true)]
        async fn poll_gives_up_immediately_when_pactl_is_missing() {
            let start = tokio::time::Instant::now();
            let got = poll_with(
                "t",
                Duration::from_secs(5),
                || async { Listing::Unavailable },
                |_: &str| Some(()),
            )
            .await;
            assert!(got.is_none());
            assert_eq!(start.elapsed(), Duration::ZERO);
        }

        #[tokio::test(start_paused = true)]
        async fn poll_stops_at_its_budget_when_never_ready() {
            let start = tokio::time::Instant::now();
            let got = poll_with(
                "t",
                Duration::from_secs(2),
                || async { Listing::NotReady("down".into()) },
                |_: &str| Some(()),
            )
            .await;
            assert!(got.is_none());
            assert!(start.elapsed() >= Duration::from_secs(2));
            assert!(start.elapsed() <= Duration::from_secs(2) + READY_POLL);
        }

        #[tokio::test(start_paused = true)]
        async fn a_hung_call_cannot_overrun_the_budget() {
            let start = tokio::time::Instant::now();
            let got = poll_with(
                "t",
                Duration::from_secs(1),
                || async {
                    sleep(Duration::from_secs(30)).await;
                    Listing::Ready(String::new())
                },
                |_: &str| Some(()),
            )
            .await;
            assert!(got.is_none());
            assert_eq!(start.elapsed(), Duration::from_secs(1));
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
