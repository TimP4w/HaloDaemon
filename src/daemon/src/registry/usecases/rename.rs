// SPDX-License-Identifier: GPL-3.0-or-later
//! Unified device-rename IPC. Routes chain-link children (whose name lives in
//! the parent's `ChainHost`) through that host, and
//! every other device through its `DeviceRecord.name` in the persisted config.

use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::drivers::Device;
use crate::ipc::broadcast_state;
use crate::registry::config::ensure_record;
use crate::state::AppState;

const MAX_NAME_LEN: usize = 64;

pub async fn set_device_name(device_id: String, name: String, app: Arc<AppState>) -> Result<()> {
    let raw = name.trim();
    let trimmed: Option<String> = if raw.is_empty() {
        None
    } else {
        let mut s = raw.to_string();
        if s.chars().count() > MAX_NAME_LEN {
            s = s.chars().take(MAX_NAME_LEN).collect();
        }
        Some(s)
    };

    let device = {
        let devices = app.devices.read().await;
        devices.iter().find(|d| d.id() == device_id).cloned()
    };

    if let Some(dev) = &device {
        if dev.has_external_name() {
            return rename_chain_link(&app, &device_id, trimmed).await;
        }
    }

    rename_normal_device(&app, device.as_deref(), &device_id, trimmed).await
}

/// Walk every chain host in `app.devices` until we find the one that owns
/// `child_id`, then dispatch to its `rename_chain_link`. Persistence and
/// broadcast piggy-back on the existing chain-layout machinery.
async fn rename_chain_link(
    app: &Arc<AppState>,
    child_id: &str,
    new_name: Option<String>,
) -> Result<()> {
    let new_name = new_name.ok_or_else(|| anyhow!("chain link name cannot be empty"))?;

    let devices = app.devices.read().await.clone();
    let mut found: Option<(Arc<dyn Device>, String)> = None;
    for parent in &devices {
        let Some(chain) = parent.chain_host() else {
            continue;
        };
        for channel in chain.chainable_channels() {
            if channel.links.iter().any(|l| l.child_device_id == child_id) {
                found = Some((parent.clone(), channel.channel_id));
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }

    let (parent, channel_id) =
        found.ok_or_else(|| anyhow!("chain parent not found for child {child_id}"))?;

    let chain = parent
        .chain_host()
        .expect("parent passed chain-host check above");
    chain.rename_link(&channel_id, child_id, &new_name)?;

    super::chain::persist_layout(app, parent.as_ref()).await?;
    broadcast_state(app).await;
    Ok(())
}

/// Empty `new_name` resets `DeviceRecord.name` to the device's descriptor
/// name (re-seeded from `device.name()`). For offline devices the entry just
/// gets cleared back to the empty string and the serializer falls back to
/// whatever `device.serialize()` produced.
async fn rename_normal_device(
    app: &Arc<AppState>,
    device: Option<&dyn Device>,
    device_id: &str,
    new_name: Option<String>,
) -> Result<()> {
    let resolved = match (new_name, device) {
        (Some(name), _) => name,
        (None, Some(d)) => d.name().to_string(),
        (None, None) => String::new(),
    };

    {
        let mut cfg = app.config.write().await;
        let record = ensure_record(&mut cfg.known_devices, device_id, device);
        record.name = resolved;
        drop(cfg);
        app.request_config_save();
    }

    broadcast_state(app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::chain::{ChainAdapter, ChainHost, ChannelDescriptor};
    use crate::drivers::{CapabilityRef, ChainLinkSpec, Device};
    use async_trait::async_trait;
    use halod_shared::types::{RgbColor, ZoneTopology};

    #[tokio::test]
    async fn writes_name_to_device_record() {
        let app = Arc::new(AppState::new(Config::default()));
        set_device_name("dev_a".into(), "My Fan".into(), app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(
            cfg.known_devices.get("dev_a").map(|r| r.name.as_str()),
            Some("My Fan")
        );
    }

    #[tokio::test]
    async fn trims_whitespace() {
        let app = Arc::new(AppState::new(Config::default()));
        set_device_name("dev_a".into(), "  CPU Fan  ".into(), app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(
            cfg.known_devices.get("dev_a").map(|r| r.name.as_str()),
            Some("CPU Fan")
        );
    }

    #[tokio::test]
    async fn caps_length_at_64_chars() {
        let app = Arc::new(AppState::new(Config::default()));
        set_device_name("dev_a".into(), "a".repeat(100), app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(
            cfg.known_devices
                .get("dev_a")
                .map(|r| r.name.chars().count()),
            Some(MAX_NAME_LEN)
        );
    }

    #[tokio::test]
    async fn blank_name_clears_to_empty_for_unknown_device() {
        let app = Arc::new(AppState::new(Config::default()));
        set_device_name("dev_a".into(), "First".into(), app.clone())
            .await
            .unwrap();
        set_device_name("dev_a".into(), "   ".into(), app.clone())
            .await
            .unwrap();
        let cfg = app.config.read().await;
        assert_eq!(
            cfg.known_devices.get("dev_a").map(|r| r.name.as_str()),
            Some("")
        );
    }

    struct NameOnlyDevice {
        id: &'static str,
        name: &'static str,
    }

    #[async_trait]
    impl Device for NameOnlyDevice {
        fn id(&self) -> &str {
            self.id
        }
        fn name(&self) -> &str {
            self.name
        }
        fn vendor(&self) -> &str {
            "Acme"
        }
        fn model(&self) -> &str {
            "Stub"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<crate::drivers::CapabilityRef<'_>> {
            vec![]
        }
    }

    #[tokio::test]
    async fn blank_name_resets_to_device_name_when_device_present() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(NameOnlyDevice {
            id: "dev_a",
            name: "Stock Name",
        });
        app.devices.write().await.push(dev);

        // First override, then clear.
        set_device_name("dev_a".into(), "Custom".into(), app.clone())
            .await
            .unwrap();
        set_device_name("dev_a".into(), "".into(), app.clone())
            .await
            .unwrap();

        let cfg = app.config.read().await;
        assert_eq!(
            cfg.known_devices.get("dev_a").map(|r| r.name.as_str()),
            Some("Stock Name")
        );
    }

    struct StubAdapter {
        parent_id: String,
    }

    #[async_trait]
    impl ChainAdapter for StubAdapter {
        fn parent_id(&self) -> String {
            self.parent_id.clone()
        }
        fn channels(&self) -> Vec<ChannelDescriptor> {
            vec![ChannelDescriptor {
                channel_id: "ch0".into(),
                display_name: "Channel".into(),
                max_leds: 120,
            }]
        }
        async fn write_composed_frame(
            &self,
            _channel_id: &str,
            _composed: &[RgbColor],
        ) -> Result<()> {
            Ok(())
        }
    }

    struct ChainParent {
        host: Arc<ChainHost>,
    }

    #[async_trait]
    impl Device for ChainParent {
        fn id(&self) -> &str {
            "parent_x"
        }
        fn name(&self) -> &str {
            "Parent"
        }
        fn vendor(&self) -> &str {
            "Acme"
        }
        fn model(&self) -> &str {
            "Hub"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![]
        }
        fn chain_host(&self) -> Option<&Arc<ChainHost>> {
            Some(&self.host)
        }
    }

    async fn setup_chain_parent_with_one_link(app: &Arc<AppState>) -> String {
        let host = ChainHost::new(Arc::new(StubAdapter {
            parent_id: "parent_x".into(),
        }));
        let parent: Arc<dyn Device> = Arc::new(ChainParent { host: host.clone() });
        app.devices.write().await.push(parent.clone());
        let (child_id, child_dev) = host
            .add_link(
                "ch0",
                ChainLinkSpec {
                    name: "Original".into(),
                    topology: ZoneTopology::Linear,
                    led_count: 8,
                },
            )
            .await
            .unwrap();
        app.devices.write().await.push(child_dev);
        child_id
    }

    #[tokio::test]
    async fn chain_link_rename_routes_through_chain_host() {
        let app = Arc::new(AppState::new(Config::default()));
        let child_id = setup_chain_parent_with_one_link(&app).await;

        set_device_name(child_id.clone(), "Top Strip".into(), app.clone())
            .await
            .unwrap();

        let devices = app.devices.read().await;
        let parent = devices.iter().find(|d| d.id() == "parent_x").unwrap();
        let chain = parent.chain_host().unwrap();
        let info = chain.chainable_channels();
        assert_eq!(info[0].links.last().unwrap().name, "Top Strip");

        // Chain-link rename must NOT pollute the parent's DeviceRecord.
        let cfg = app.config.read().await;
        assert!(!cfg.known_devices.contains_key(&child_id));
    }

    #[tokio::test]
    async fn chain_link_rename_rejects_blank_name() {
        let app = Arc::new(AppState::new(Config::default()));
        let child_id = setup_chain_parent_with_one_link(&app).await;

        let err = set_device_name(child_id, "   ".into(), app.clone())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be empty"), "got: {err}");
    }
}
