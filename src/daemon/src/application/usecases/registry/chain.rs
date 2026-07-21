// SPDX-License-Identifier: GPL-3.0-or-later
//! IPC use cases for chainable ARGB channels. Each handler forwards to the
//! device's shared [`crate::domain::device::chain::LightingDivisionHost`], then persists and commits the affected topics.

use crate::domain::events::ChangeSink as _;

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::device::chain::LightingDivisionHost;
use crate::domain::device::{ChainLinkSpec, Device};
use crate::domain::registry::model::{ChainLinkRecord, ChannelLayoutRecord, DeviceLayout};
use halod_shared::types::ZoneTopology;

fn require_chain(device: &Arc<dyn Device>) -> Result<&Arc<LightingDivisionHost>> {
    device
        .chain_host()
        .context("device does not support chainable channels")
}

pub(crate) async fn persist_layout(app: &Arc<AppState>, device: &dyn Device) -> Result<()> {
    let dev_id = device.id().to_owned();
    let host = require_chain_ref(device)?;
    let channels: std::collections::HashMap<String, ChannelLayoutRecord> = host
        .persistent_links()
        .into_iter()
        .map(|(channel_id, links)| {
            let chain_links = links
                .into_iter()
                .map(|link| ChainLinkRecord {
                    id: link.child_id,
                    name: link.name,
                    topology: link.topology,
                    led_count: link.led_count,
                })
                .collect();
            (channel_id, ChannelLayoutRecord { chain_links })
        })
        .collect();
    {
        let mut cfg = app.config.write().await;
        if channels.is_empty() {
            cfg.device_layouts.remove(&dev_id);
        } else {
            cfg.device_layouts.insert(dev_id, DeviceLayout { channels });
        }
    }
    app.request_config_save();
    Ok(())
}

fn require_chain_ref(device: &dyn Device) -> Result<&Arc<LightingDivisionHost>> {
    device
        .chain_host()
        .context("device does not support chainable channels")
}

pub async fn lighting_add_segment(
    id: String,
    channel_id: String,
    name: String,
    led_count: u32,
    topology: ZoneTopology,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::domain::registry::require_device_owned_id(&id, &app).await?;
    if led_count == 0 {
        anyhow::bail!("led_count must be at least 1");
    }

    let spec = ChainLinkSpec {
        name,
        topology,
        led_count,
    };

    {
        let chain = require_chain(&device)?;
        let (_child_id, child_device) = chain.add_link(&channel_id, spec).await?;
        app.device_registry.write().await.push(child_device);
    }

    persist_layout(&app, device.as_ref()).await?;
    app.record_change(crate::domain::events::Change::LightingTopology)
        .await;
    Ok(())
}

pub async fn lighting_remove_segment(
    id: String,
    channel_id: String,
    child_device_id: String,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::domain::registry::require_device_owned_id(&id, &app).await?;

    {
        let chain = require_chain(&device)?;
        let removed_id = chain.remove_link(&channel_id, &child_device_id).await?;
        app.device_registry
            .write()
            .await
            .retain(|d| d.id() != removed_id);
    }

    persist_layout(&app, device.as_ref()).await?;
    app.record_change(crate::domain::events::Change::LightingTopology)
        .await;
    Ok(())
}

pub async fn lighting_reorder_segment(
    id: String,
    channel_id: String,
    child_device_id: String,
    new_index: usize,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::domain::registry::require_device_owned_id(&id, &app).await?;

    {
        let chain = require_chain(&device)?;
        chain.reorder_link(&channel_id, &child_device_id, new_index)?;
    }

    persist_layout(&app, device.as_ref()).await?;
    app.record_change(crate::domain::events::Change::LightingDevice(id))
        .await;
    Ok(())
}

pub async fn lighting_detect_segments(
    id: String,
    channel_id: String,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::domain::registry::require_device_owned_id(&id, &app).await?;
    let chain = require_chain(&device)?;
    chain.detect_channel(&channel_id).await
}

pub async fn restore_saved_chains(app: Arc<AppState>) {
    let saved = app.config.read().await.device_layouts.clone();
    let devices = app.device_registry.read().await.clone();
    let mut restored_layouts = saved.clone();

    for device in devices {
        let Some(layout) = saved.get(device.id()) else {
            continue;
        };
        let Some(host) = device.chain_host() else {
            restored_layouts.remove(device.id());
            continue;
        };
        let mut restored_channels = std::collections::HashMap::new();
        for (channel_id, channel) in &layout.channels {
            let mut restored_links = Vec::new();
            for link in &channel.chain_links {
                let spec = ChainLinkSpec {
                    name: link.name.clone(),
                    topology: link.topology.clone(),
                    led_count: link.led_count,
                };
                match host.restore_link(channel_id, &link.id, spec).await {
                    Ok(child) => {
                        app.device_registry.write().await.push(child);
                        restored_links.push(link.clone());
                    }
                    Err(error) => log::warn!(
                        "failed to restore chain link {} on {}:{}: {error:#}",
                        link.id,
                        device.id(),
                        channel_id
                    ),
                }
            }
            if !restored_links.is_empty() {
                restored_channels.insert(
                    channel_id.clone(),
                    ChannelLayoutRecord {
                        chain_links: restored_links,
                    },
                );
            }
        }
        if restored_channels.is_empty() {
            restored_layouts.remove(device.id());
        } else {
            restored_layouts.insert(
                device.id().to_owned(),
                DeviceLayout {
                    channels: restored_channels,
                },
            );
        }
    }

    app.config.write().await.device_layouts = restored_layouts;
    app.request_config_save();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::device::chain::{
        ChannelDescriptor, LightingDivisionAdapter, LightingDivisionHost,
    };
    use crate::domain::device::{CapabilityRef, Device};
    use crate::domain::registry::model::{ChainLinkRecord, ChannelLayoutRecord, DeviceLayout};
    use async_trait::async_trait;
    use halod_shared::types::{ColorOrder, ZoneTopology};
    use std::sync::Arc;

    fn make_app_with(dev: Arc<dyn Device>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry.try_write().unwrap().push(dev);
        app
    }

    struct MockAdapter {
        id: String,
        channels: Vec<ChannelDescriptor>,
    }

    #[async_trait]
    impl LightingDivisionAdapter for MockAdapter {
        fn parent_id(&self) -> String {
            self.id.clone()
        }

        fn channels(&self) -> Vec<ChannelDescriptor> {
            self.channels.clone()
        }

        async fn write_divided_frame(
            &self,
            _channel_id: &str,
            _composed: &[u8],
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Minimal Device with a real LightingDivisionHost controlled by the test.
    struct MockChainDevice {
        id: String,
        host: Arc<LightingDivisionHost>,
    }

    impl MockChainDevice {
        fn new(id: &str) -> Self {
            let host = LightingDivisionHost::new(Arc::new(MockAdapter {
                id: id.to_owned(),
                channels: vec![],
            }));
            Self {
                id: id.to_string(),
                host,
            }
        }
        fn with_channels(self, descriptors: Vec<ChannelDescriptor>) -> Self {
            let host = LightingDivisionHost::new(Arc::new(MockAdapter {
                id: self.id.clone(),
                channels: descriptors,
            }));
            {
                let mut state = host.state.lock().unwrap();
                let _ = &mut state;
            }
            Self { id: self.id, host }
        }
        fn restore_fails(self) -> Self {
            self
        }
    }

    #[async_trait]
    impl Device for MockChainDevice {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            "mock-chain"
        }
        fn vendor(&self) -> &str {
            "mock"
        }
        fn model(&self) -> &str {
            "mock"
        }
        async fn initialize(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            vec![]
        }
        fn chain_host(&self) -> Option<&Arc<LightingDivisionHost>> {
            Some(&self.host)
        }
    }

    fn strip_topology() -> ZoneTopology {
        ZoneTopology::Linear
    }

    fn channel(id: &str, name: &str, max_leds: u32) -> ChannelDescriptor {
        ChannelDescriptor {
            channel_id: id.into(),
            display_name: name.into(),
            max_leds,
            color_order: ColorOrder::Rgb,
            cooling_channel: None,
        }
    }

    // led_count == 0

    #[tokio::test]
    async fn rgb_chain_add_link_rejects_led_count_zero() {
        let dev = Arc::new(MockChainDevice::new("dev1").with_channels(vec![channel(
            "ch0",
            "Channel 0",
            100,
        )]));
        let app = make_app_with(dev as Arc<dyn Device>);
        let err = lighting_add_segment(
            "dev1".into(),
            "ch0".into(),
            "strip".into(),
            0,
            ZoneTopology::Linear,
            app,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("led_count"),
            "expected led_count error, got: {err}"
        );
    }

    #[tokio::test]
    async fn persist_layout_records_user_link_with_stable_id() {
        let dev = Arc::new(MockChainDevice::new("dev1").with_channels(vec![channel(
            "ch0",
            "Channel 0",
            100,
        )]));
        let app = make_app_with(dev.clone() as Arc<dyn Device>);

        let (child_id, child) = dev
            .host
            .add_link(
                "ch0",
                ChainLinkSpec {
                    name: "Desk strip".into(),
                    topology: ZoneTopology::Linear,
                    led_count: 24,
                },
            )
            .await
            .unwrap();
        app.device_registry.write().await.push(child);

        persist_layout(&app, dev.as_ref()).await.unwrap();

        let cfg = app.config.read().await;
        let links = &cfg.device_layouts["dev1"].channels["ch0"].chain_links;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].id, child_id);
        assert_eq!(links[0].name, "Desk strip");
        assert_eq!(links[0].led_count, 24);
    }

    #[tokio::test]
    async fn persist_layout_removes_device_entry_when_all_channels_empty() {
        // All links are locked → nothing to persist → device entry removed.
        let dev = Arc::new(MockChainDevice::new("dev1").with_channels(vec![channel(
            "ch0",
            "Channel 0",
            100,
        )]));
        let app = make_app_with(dev.clone() as Arc<dyn Device>);
        // Pre-seed a stale entry so we can confirm it's removed.
        app.config.write().await.device_layouts.insert(
            "dev1".into(),
            DeviceLayout {
                channels: Default::default(),
            },
        );

        persist_layout(&app, dev.as_ref()).await.unwrap();

        assert!(
            !app.config.read().await.device_layouts.contains_key("dev1"),
            "stale device layout must be removed when all channels are empty after filtering"
        );
    }

    // ── UC3-3: restore_saved_chains prunes failed records ────────────────────

    #[tokio::test]
    async fn restore_saved_chains_prunes_failed_records() {
        let dev = Arc::new(MockChainDevice::new("dev1").restore_fails());
        let app = make_app_with(dev as Arc<dyn Device>);

        // Insert a saved layout with one record that will fail to restore.
        {
            let mut cfg = app.config.write().await;
            cfg.device_layouts.insert(
                "dev1".into(),
                DeviceLayout {
                    channels: std::collections::HashMap::from([(
                        "ch0".into(),
                        ChannelLayoutRecord {
                            chain_links: vec![ChainLinkRecord {
                                id: "bad-link".into(),
                                name: "Bad Link".into(),
                                topology: strip_topology(),
                                led_count: 1,
                            }],
                        },
                    )]),
                },
            );
        }

        restore_saved_chains(app.clone()).await;

        // The failed record must have been removed from the persisted config.
        let cfg = app.config.read().await;
        assert!(
            !cfg.device_layouts.contains_key("dev1"),
            "device layout entry must be removed when all restore attempts fail"
        );
    }

    #[tokio::test]
    async fn restore_saved_chains_recreates_child_with_persisted_id() {
        let dev = Arc::new(MockChainDevice::new("dev1").with_channels(vec![channel(
            "ch0",
            "Channel 0",
            100,
        )]));
        let app = make_app_with(dev.clone() as Arc<dyn Device>);
        app.config.write().await.device_layouts.insert(
            "dev1".into(),
            DeviceLayout {
                channels: std::collections::HashMap::from([(
                    "ch0".into(),
                    ChannelLayoutRecord {
                        chain_links: vec![ChainLinkRecord {
                            id: "stable-child-id".into(),
                            name: "Restored strip".into(),
                            topology: ZoneTopology::Linear,
                            led_count: 16,
                        }],
                    },
                )]),
            },
        );

        restore_saved_chains(app.clone()).await;

        let registry = app.device_registry.read().await;
        assert!(registry
            .iter()
            .any(|device| device.id() == "stable-child-id"));
        drop(registry);
        let links = dev.host.persistent_links();
        assert_eq!(links["ch0"][0].child_id, "stable-child-id");
        assert_eq!(links["ch0"][0].name, "Restored strip");
    }
}
