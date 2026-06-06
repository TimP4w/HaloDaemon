use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::{persist_device_state, require_device_owned};
use crate::ipc::broadcast_state;
use crate::state::AppState;
use halod_protocol::types::ButtonMapping;

pub async fn set_button_mapping(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let cap = device
        .as_key_remap()
        .ok_or_else(|| anyhow::anyhow!("device does not support key remapping"))?;

    let mapping: ButtonMapping = serde_json::from_value(
        msg.get("mapping")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing mapping field"))?,
    )?;

    cap.set_button_mapping(mapping).await?;
    persist_device_state(&app, device.as_ref()).await;
    broadcast_state(app).await;
    Ok(())
}

pub async fn reset_button_mapping(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let cap = device
        .as_key_remap()
        .ok_or_else(|| anyhow::anyhow!("device does not support key remapping"))?;

    let cid = msg["cid"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("missing or invalid cid"))? as u16;

    cap.reset_button_mapping(cid).await?;
    persist_device_state(&app, device.as_ref()).await;
    broadcast_state(app).await;
    Ok(())
}

pub async fn reset_all_button_mappings(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let cap = device
        .as_key_remap()
        .ok_or_else(|| anyhow::anyhow!("device does not support key remapping"))?;

    cap.reset_all_button_mappings().await?;
    persist_device_state(&app, device.as_ref()).await;
    broadcast_state(app).await;
    Ok(())
}

pub async fn set_software_dpi_steps(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned(&msg, &app).await?;
    let cap = device
        .as_dpi()
        .ok_or_else(|| anyhow::anyhow!("device does not support software DPI"))?;

    let steps_raw = msg["steps"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("missing or invalid steps array"))?;
    let steps: Vec<u16> = steps_raw
        .iter()
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| anyhow::anyhow!("DPI step must be a non-negative integer"))
                .map(|n| n as u16)
        })
        .collect::<Result<Vec<_>>>()?;

    cap.set_dpi_steps(steps).await?;
    persist_device_state(&app, device.as_ref()).await;
    broadcast_state(app).await;
    Ok(())
}
