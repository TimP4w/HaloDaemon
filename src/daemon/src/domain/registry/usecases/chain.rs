// SPDX-License-Identifier: GPL-3.0-or-later
//! IPC use cases for chainable ARGB channels. Each handler forwards to the
//! device's shared [`crate::infrastructure::drivers::chain::LightingDivisionHost`], then persists and commits the affected topics.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::infrastructure::drivers::chain::LightingDivisionHost;
use crate::infrastructure::drivers::{ChainLinkSpec, Device};
use halod_shared::types::ZoneTopology;

fn require_chain(device: &Arc<dyn Device>) -> Result<&Arc<LightingDivisionHost>> {
    device
        .chain_host()
        .context("device does not support chainable channels")
}

/// Lighting segments are intentionally not persisted across the hard lighting
/// model cut.  The runtime descriptor is the only source of truth.
pub(crate) async fn persist_layout(app: &Arc<AppState>, device: &dyn Device) -> Result<()> {
    let dev_id = device.id().to_owned();
    {
        let mut cfg = app.config.write().await;
        cfg.device_layouts.remove(&dev_id);
    }
    app.request_config_save();
    Ok(())
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
    app.record_change(crate::application::bus::coordinator::Change::LightingTopology)
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
    app.record_change(crate::application::bus::coordinator::Change::LightingTopology)
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
    app.record_change(crate::application::bus::coordinator::Change::LightingDevice(id))
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

/// Legacy chain layouts are deliberately discarded by the lighting hard cut.
pub async fn restore_saved_chains(app: Arc<AppState>) {
    let mut cfg = app.config.write().await;
    cfg.device_layouts.clear();
    drop(cfg);
    app.request_config_save();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::registry::model::{ChainLinkRecord, ChannelLayoutRecord, DeviceLayout};
    use crate::infrastructure::drivers::chain::{
        ChannelDescriptor, LightingDivisionAdapter, LightingDivisionHost,
    };
    use crate::infrastructure::drivers::{CapabilityRef, Device};
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

    // The hard cut drops every persisted segment layout.

    #[tokio::test]
    async fn persist_layout_discards_legacy_layout() {
        let dev = Arc::new(MockChainDevice::new("dev1").with_channels(vec![channel(
            "ch0",
            "Channel 0",
            100,
        )]));
        let app = make_app_with(dev.clone() as Arc<dyn Device>);

        persist_layout(&app, dev.as_ref()).await.unwrap();

        let cfg = app.config.read().await;
        assert!(!cfg.device_layouts.contains_key("dev1"));
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
}
