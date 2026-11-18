// SPDX-License-Identifier: GPL-3.0-or-later
//! Cross-domain semantic changes emitted after successful state mutations.

use halod_shared::ids::DeviceId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Bootstrap,
    DiscoveryTopology,
    Device(DeviceId),
    Devices(Vec<DeviceId>),
    SensorTelemetry(Vec<DeviceId>),
    Lighting,
    LightingDevice(DeviceId),
    LightingCatalog,
    LightingTopology,
    Canvas,
    CanvasDevice(DeviceId),
    Cooling,
    CoolingDevice(DeviceId),
    Lcd,
    LcdDevice(DeviceId),
    LcdCatalog,
    Gui,
    Profiles,
    AppRules,
    ProfileSwitch,
    PluginTopology,
    PluginData,
    PluginDeviceStatus(DeviceId),
    Update,
}

#[async_trait::async_trait]
pub trait ChangeSink: Send + Sync {
    async fn record_change(&self, change: Change);
}
