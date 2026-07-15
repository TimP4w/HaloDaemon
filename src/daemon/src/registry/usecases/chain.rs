// SPDX-License-Identifier: GPL-3.0-or-later
//! IPC use cases for chainable ARGB channels. Each handler forwards to the
//! device's shared [`crate::drivers::chain::ChainHost`], then persists and broadcasts.

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::drivers::chain::ChainHost;
use crate::drivers::{ChainLinkSpec, Device};
use crate::ipc;
use crate::platform::notify;
use crate::registry::config::{ChainLinkRecord, ChannelLayoutRecord, DeviceLayout};
use crate::state::AppState;
use halod_shared::types::ZoneTopology;

fn require_chain(device: &Arc<dyn Device>) -> Result<&Arc<ChainHost>> {
    device
        .chain_host()
        .context("device does not support chainable channels")
}

/// Locked (hardware-detected) links are skipped — rediscovered each boot.
pub(super) async fn persist_layout(app: &Arc<AppState>, device: &dyn Device) -> Result<()> {
    let Some(chain) = device.chain_host() else {
        return Ok(());
    };
    let dev_id = device.id().to_owned();

    let layout = {
        let mut channels = std::collections::HashMap::new();
        for channel in chain.chainable_channels() {
            let mut links = Vec::new();
            for link in channel.links {
                if link.locked {
                    continue;
                }
                links.push(ChainLinkRecord {
                    id: link.child_device_id,
                    name: link.name,
                    topology: link.topology,
                    led_count: link.led_count,
                });
            }
            if !links.is_empty() {
                channels.insert(
                    channel.channel_id,
                    ChannelLayoutRecord { chain_links: links },
                );
            }
        }
        DeviceLayout { channels }
    };

    {
        let mut cfg = app.config.write().await;
        if layout.channels.is_empty() {
            cfg.device_layouts.remove(&dev_id);
        } else {
            cfg.device_layouts.insert(dev_id, layout);
        }
    }
    app.request_config_save();
    Ok(())
}

pub async fn rgb_chain_add_link(
    id: String,
    channel_id: String,
    name: String,
    led_count: u32,
    topology: ZoneTopology,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::registry::require_device_owned_id(&id, &app).await?;
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
        app.devices.write().await.push(child_device);
    }

    persist_layout(&app, device.as_ref()).await?;
    ipc::broadcast_state(&app).await;
    Ok(())
}

pub async fn rgb_chain_remove_link(
    id: String,
    channel_id: String,
    child_device_id: String,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::registry::require_device_owned_id(&id, &app).await?;

    {
        let chain = require_chain(&device)?;
        let removed_id = chain.remove_link(&channel_id, &child_device_id).await?;
        app.devices.write().await.retain(|d| d.id() != removed_id);
    }

    persist_layout(&app, device.as_ref()).await?;
    ipc::broadcast_state(&app).await;
    Ok(())
}

pub async fn rgb_chain_reorder_link(
    id: String,
    channel_id: String,
    child_device_id: String,
    new_index: usize,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::registry::require_device_owned_id(&id, &app).await?;

    {
        let chain = require_chain(&device)?;
        chain.reorder_link(&channel_id, &child_device_id, new_index)?;
    }

    persist_layout(&app, device.as_ref()).await?;
    ipc::broadcast_state(&app).await;
    Ok(())
}

pub async fn rgb_chain_detect_channel(
    id: String,
    channel_id: String,
    app: Arc<AppState>,
) -> Result<()> {
    let device = crate::registry::require_device_owned_id(&id, &app).await?;
    let chain = require_chain(&device)?;
    chain.detect_channel(&channel_id).await
}

/// No broadcast — the regular startup broadcast picks the restored links up.
///
/// Failed records (e.g. a topology that no longer validates) are purged from
/// the saved config: the UI cannot edit a link that never materialized as a
/// device, so leaving the record around would fail every restart. The user
/// gets a notification explaining what was dropped.
pub async fn restore_saved_chains(app: Arc<AppState>) {
    let layouts = {
        let cfg = app.config.read().await;
        cfg.device_layouts.clone()
    };
    if layouts.is_empty() {
        return;
    }

    let mut failed: Vec<(String, String, String)> = Vec::new();

    let devices = app.devices.read().await.clone();
    for device in &devices {
        let Some(layout) = layouts.get(device.id()) else {
            continue;
        };
        let Some(chain) = device.chain_host() else {
            log::warn!(
                "[chain restore] device {} has saved layout but no chain host, skipping",
                device.id()
            );
            continue;
        };
        for (channel_id, channel_layout) in &layout.channels {
            for record in &channel_layout.chain_links {
                match chain.restore_link(channel_id, record).await {
                    Ok(child_device) => {
                        crate::registry::usecases::registration::register_device(
                            &app,
                            child_device,
                        )
                        .await;
                    }
                    Err(e) => {
                        notify::send(
                            &app,
                            halod_shared::types::NotificationCode::ChainLinkRestoreFailed {
                                name: record.name.clone(),
                                detail: format!("{}/{}: {e}", device.id(), channel_id),
                            },
                        )
                        .await;
                        failed.push((
                            device.id().to_owned(),
                            channel_id.clone(),
                            record.id.clone(),
                        ));
                    }
                }
            }
        }
    }

    if failed.is_empty() {
        return;
    }

    let mut cfg = app.config.write().await;
    for (device_id, channel_id, record_id) in &failed {
        let Some(layout) = cfg.device_layouts.get_mut(device_id) else {
            continue;
        };
        if let Some(channel) = layout.channels.get_mut(channel_id) {
            channel.chain_links.retain(|r| r.id != *record_id);
            if channel.chain_links.is_empty() {
                layout.channels.remove(channel_id);
            }
        }
        if layout.channels.is_empty() {
            cfg.device_layouts.remove(device_id);
        }
    }
    drop(cfg);
    app.request_config_save();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::chain::{ChainAdapter, ChainHost, ChainLinkRuntime, ChannelDescriptor};
    use crate::drivers::{CapabilityRef, Device};
    use crate::registry::config::{ChannelLayoutRecord, DeviceLayout};
    use async_trait::async_trait;
    use halod_shared::types::{ChainLinkInfo, ChainableChannelInfo, ZoneTopology};
    use std::sync::Arc;

    fn make_app_with(dev: Arc<dyn Device>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.try_write().unwrap().push(dev);
        app
    }

    struct MockAdapter {
        id: String,
        channels: Vec<ChannelDescriptor>,
    }

    #[async_trait]
    impl ChainAdapter for MockAdapter {
        fn parent_id(&self) -> String {
            self.id.clone()
        }

        fn channels(&self) -> Vec<ChannelDescriptor> {
            self.channels.clone()
        }

        async fn write_composed_frame(
            &self,
            _channel_id: &str,
            _composed: &[halod_shared::types::RgbColor],
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Minimal Device with a real ChainHost controlled by the test.
    struct MockChainDevice {
        id: String,
        host: Arc<ChainHost>,
    }

    impl MockChainDevice {
        fn new(id: &str) -> Self {
            let host = ChainHost::new(Arc::new(MockAdapter {
                id: id.to_owned(),
                channels: vec![],
            }));
            Self {
                id: id.to_string(),
                host,
            }
        }
        fn with_channels(self, channels: Vec<ChainableChannelInfo>) -> Self {
            let descriptors = channels
                .iter()
                .map(|channel| ChannelDescriptor {
                    channel_id: channel.channel_id.clone(),
                    display_name: channel.name.clone(),
                    max_leds: channel.max_leds,
                })
                .collect();
            let host = ChainHost::new(Arc::new(MockAdapter {
                id: self.id.clone(),
                channels: descriptors,
            }));
            {
                let mut state = host.state.lock().unwrap();
                for channel in channels {
                    let target = state.get_mut(&channel.channel_id).unwrap();
                    target.links = channel
                        .links
                        .into_iter()
                        .map(|link| {
                            ChainLinkRuntime::new(
                                link.child_device_id,
                                link.name,
                                link.topology,
                                link.led_count,
                                link.locked,
                            )
                        })
                        .collect();
                }
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
        fn chain_host(&self) -> Option<&Arc<ChainHost>> {
            Some(&self.host)
        }
    }

    fn strip_topology() -> ZoneTopology {
        ZoneTopology::Linear
    }

    // led_count == 0

    #[tokio::test]
    async fn rgb_chain_add_link_rejects_led_count_zero() {
        let dev =
            Arc::new(
                MockChainDevice::new("dev1").with_channels(vec![ChainableChannelInfo {
                    channel_id: "ch0".into(),
                    name: "Channel 0".into(),
                    max_leds: 100,
                    links: vec![],
                }]),
            );
        let app = make_app_with(dev as Arc<dyn Device>);
        let err = rgb_chain_add_link(
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

    // persist_layout skips locked links; removes entry when empty

    #[tokio::test]
    async fn persist_layout_skips_locked_links() {
        let dev =
            Arc::new(
                MockChainDevice::new("dev1").with_channels(vec![ChainableChannelInfo {
                    channel_id: "ch0".into(),
                    name: "Channel 0".into(),
                    max_leds: 100,
                    links: vec![
                        ChainLinkInfo {
                            child_device_id: "locked-child".into(),
                            name: "Locked".into(),
                            topology: strip_topology(),
                            led_count: 5,
                            locked: true,
                        },
                        ChainLinkInfo {
                            child_device_id: "user-child".into(),
                            name: "User".into(),
                            topology: strip_topology(),
                            led_count: 3,
                            locked: false,
                        },
                    ],
                }]),
            );
        let app = make_app_with(dev.clone() as Arc<dyn Device>);

        persist_layout(&app, dev.as_ref()).await.unwrap();

        let cfg = app.config.read().await;
        let layout = cfg
            .device_layouts
            .get("dev1")
            .expect("layout must be saved");
        let channel = layout.channels.get("ch0").expect("channel must exist");
        assert_eq!(
            channel.chain_links.len(),
            1,
            "only the unlocked link should be persisted"
        );
        assert_eq!(channel.chain_links[0].id, "user-child");
    }

    #[tokio::test]
    async fn persist_layout_removes_device_entry_when_all_channels_empty() {
        // All links are locked → nothing to persist → device entry removed.
        let dev =
            Arc::new(
                MockChainDevice::new("dev1").with_channels(vec![ChainableChannelInfo {
                    channel_id: "ch0".into(),
                    name: "Channel 0".into(),
                    max_leds: 100,
                    links: vec![ChainLinkInfo {
                        child_device_id: "locked".into(),
                        name: "Locked".into(),
                        topology: strip_topology(),
                        led_count: 5,
                        locked: true,
                    }],
                }]),
            );
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
