// SPDX-License-Identifier: GPL-3.0-or-later
//! The shared HID++ 2.0 init spine — feature enumeration and the device-name
//! read. Capability-specific initialisation lives next to each capability impl
//! (`battery::init_battery`, `rgb::init_rgb`, `onboard::init_dpi`, …).

use std::collections::HashMap;

use anyhow::Result;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::Hidpp20;

impl LogitechDevice {
    pub(super) async fn init_features(&self) -> Result<HashMap<u16, u8>> {
        let (msg, devnum) = self.transport_snapshot().await;
        let table = Hidpp20::enumerate(&msg, devnum).await?;
        log::debug!(
            "[{}] Features: {:?}",
            self.id,
            table.keys().collect::<Vec<_>>()
        );
        Ok(table)
    }

    /// Read the device name via the DEVICE_NAME feature. `None` when the device
    /// doesn't advertise it (e.g. headsets) or the read yields nothing — the
    /// caller then falls back to the static `model_name`.
    pub(super) async fn init_name(&self, features: &HashMap<u16, u8>) -> Option<String> {
        // Prefer DEVICE_FRIENDLY_NAME (0x0007) when both are present (newer
        // devices like MX Keys, Craft, etc.).
        if features.contains_key(&feature::DEVICE_FRIENDLY_NAME) {
            if let Some(name) = self
                .hidpp2_with(features)
                .await
                .device_friendly_name()
                .await
            {
                return Some(name);
            }
        }
        self.hidpp2_with(features).await.device_name().await
    }

    /// Read the MAIN firmware version via FIRMWARE_VERSION (0x0003). `None` when
    /// the device doesn't advertise it or the read fails; purely informational.
    pub(super) async fn init_firmware(&self, features: &HashMap<u16, u8>) -> Option<String> {
        if !features.contains_key(&feature::FIRMWARE_VERSION) {
            return None;
        }
        self.hidpp2_with(features)
            .await
            .read_firmware_version()
            .await
    }
}
