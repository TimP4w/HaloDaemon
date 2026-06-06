//! IPC use cases for chainable ARGB channels. Each handler forwards to
//! [`crate::drivers::ChainCapability`] then persists + broadcasts.

use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::require_device_owned;
use crate::config::{ChainLinkRecord, ChannelLayoutRecord, DeviceLayout};
use crate::drivers::{ChainCapability, ChainLinkKind, ChainLinkSpec, Device};
use crate::ipc;
use crate::notify;
use crate::state::AppState;
use halod_protocol::types::ZoneTopology;

fn require_chain<'a>(device: &'a Arc<dyn Device>) -> Result<&'a dyn ChainCapability> {
    device
        .as_chain()
        .ok_or_else(|| anyhow::anyhow!("device does not support chainable channels"))
}

fn parse_channel_id(msg: &Value) -> Result<String> {
    Ok(msg["channel_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing channel_id"))?
        .to_string())
}

fn parse_child_id(msg: &Value) -> Result<String> {
    Ok(msg["child_device_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing child_device_id"))?
        .to_string())
}

fn parse_topology(value: &Value) -> Result<ZoneTopology> {
    serde_json::from_value::<ZoneTopology>(value.clone())
        .map_err(|e| anyhow::anyhow!("invalid topology: {e}"))
}

/// Locked (hardware-detected) links are skipped — rediscovered each boot.
pub(super) async fn persist_layout(app: &Arc<AppState>, device: &dyn Device) -> Result<()> {
    let Some(chain) = device.as_chain() else {
        return Ok(());
    };
    let dev_id = device.id();

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
                    // Persist `link_kind` verbatim so the restorer knows which
                    // factory to call without re-deriving it from the parent.
                    kind: channel.link_kind.clone(),
                    name: link.name,
                    topology: link.topology,
                    led_count: link.led_count,
                });
            }
            if !links.is_empty() {
                channels.insert(channel.channel_id, ChannelLayoutRecord { chain_links: links });
            }
        }
        DeviceLayout { channels }
    };

    let _cfg_snap = {
        let mut cfg = app.config.write().await;
        if layout.channels.is_empty() {
            cfg.device_layouts.remove(&dev_id);
        } else {
            cfg.device_layouts.insert(dev_id, layout);
        }
        cfg.clone()
    };
    #[cfg(not(test))]
    app.request_config_save(_cfg_snap);
    Ok(())
}

pub async fn rgb_chain_add_link(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let channel_id = parse_channel_id(&msg)?;
    let name = msg["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing name"))?
        .to_string();
    let led_count = msg["led_count"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing led_count"))? as u32;
    if led_count == 0 {
        anyhow::bail!("led_count must be at least 1");
    }
    let topology = parse_topology(
        msg.get("topology")
            .ok_or_else(|| anyhow::anyhow!("missing topology"))?,
    )?;
    // The UI only sends `kind` when a device exposes multiple link kinds;
    // single-vendor parents fall back to their device-derived default.
    let kind = match msg.get("kind").and_then(|v| v.as_str()) {
        Some(s) => ChainLinkKind::from_str(s)
            .ok_or_else(|| anyhow::anyhow!("unknown chain link kind: {s}"))?,
        None => default_kind_for_device(device.as_ref())?,
    };

    let spec = ChainLinkSpec {
        kind,
        name,
        topology,
        led_count,
    };

    {
        let chain = require_chain(&device)?;
        chain.add_chain_link(&channel_id, spec, app.clone()).await?;
    }

    persist_layout(&app, device.as_ref()).await?;
    ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn rgb_chain_remove_link(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let channel_id = parse_channel_id(&msg)?;
    let child_id = parse_child_id(&msg)?;

    {
        let chain = require_chain(&device)?;
        chain
            .remove_chain_link(&channel_id, &child_id, app.clone())
            .await?;
    }

    persist_layout(&app, device.as_ref()).await?;
    ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn rgb_chain_reorder_link(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let channel_id = parse_channel_id(&msg)?;
    let child_id = parse_child_id(&msg)?;
    let new_index = msg["new_index"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing new_index"))? as usize;

    {
        let chain = require_chain(&device)?;
        chain
            .reorder_chain_link(&channel_id, &child_id, new_index, app.clone())
            .await?;
    }

    persist_layout(&app, device.as_ref()).await?;
    ipc::broadcast_state(app).await;
    Ok(())
}

pub async fn rgb_chain_detect_channel(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let channel_id = parse_channel_id(&msg)?;
    let chain = require_chain(&device)?;
    chain.detect_channel(&channel_id).await
}

fn default_kind_for_device(device: &dyn Device) -> Result<ChainLinkKind> {
    let chain = device
        .as_chain()
        .ok_or_else(|| anyhow::anyhow!("device does not support chains"))?;
    let channels = chain.chainable_channels();
    let first = channels
        .first()
        .ok_or_else(|| anyhow::anyhow!("device has no chainable channels"))?;
    ChainLinkKind::from_str(&first.link_kind)
        .ok_or_else(|| anyhow::anyhow!("unknown link_kind on device: {}", first.link_kind))
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

    // (device_id, channel_id, record_id) for every record that failed to restore.
    let mut failed: Vec<(String, String, String)> = Vec::new();

    let devices = app.devices.lock().await.clone();
    for device in &devices {
        let Some(layout) = layouts.get(&device.id()) else {
            continue;
        };
        let Some(chain) = device.as_chain() else {
            log::warn!(
                "[chain restore] device {} has saved layout but no ChainCapability — skipping",
                device.id()
            );
            continue;
        };
        for (channel_id, channel_layout) in &layout.channels {
            for record in &channel_layout.chain_links {
                if let Err(e) = chain
                    .restore_chain_link(channel_id, record, app.clone())
                    .await
                {
                    notify::error(
                        &app,
                        format!("Could not restore chain link \"{}\"", record.name),
                        format!(
                            "{}/{}: {e} — removed from saved layout.",
                            device.id(),
                            channel_id
                        ),
                    )
                    .await;
                    failed.push((device.id(), channel_id.clone(), record.id.clone()));
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
    let cfg_snap = cfg.clone();
    drop(cfg);
    app.request_config_save(cfg_snap);
}
