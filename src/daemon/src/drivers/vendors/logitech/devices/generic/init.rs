// SPDX-License-Identifier: GPL-3.0-or-later
//! The shared HID++ 2.0 init spine — feature enumeration and the device-name
//! read. Capability-specific initialisation lives next to each capability impl
//! (`battery::init_battery`, `rgb::init_rgb`, `onboard::init_dpi`, …).

use std::collections::HashMap;

use anyhow::Result;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
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
        self.hidpp2_with(features).await.device_name().await
    }
}
