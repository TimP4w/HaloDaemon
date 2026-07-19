// SPDX-License-Identifier: GPL-3.0-or-later
//! Cross-domain semantic changes emitted after successful state mutations.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Bootstrap,
    DiscoveryTopology,
    Device(String),
    Devices(Vec<String>),
    SensorTelemetry(Vec<String>),
    Lighting,
    LightingDevice(String),
    LightingCatalog,
    LightingTopology,
    Canvas,
    CanvasDevice(String),
    Cooling,
    CoolingDevice(String),
    Lcd,
    LcdDevice(String),
    LcdCatalog,
    Gui,
    Profiles,
    AppRules,
    ProfileSwitch,
    PluginTopology,
    PluginData,
    PluginDeviceStatus(String),
}

#[async_trait::async_trait]
pub trait ChangeSink: Send + Sync {
    async fn record_change(&self, change: Change);
}
