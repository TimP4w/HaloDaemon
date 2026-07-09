// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::LogitechDeviceState;

impl LogitechDevice {
    /// Read keyboard backlight info, current level, and on/off state at init.
    pub(super) async fn init_brightness(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        let hidpp = self.hidpp2_with(features).await;
        state.brightness.info = hidpp.read_brightness_info().await;
        state.brightness.level = hidpp.read_brightness().await;
        if state.brightness.info.as_ref().is_some_and(|i| i.has_on_off) {
            state.brightness.on = hidpp.read_brightness_on_off().await.unwrap_or(true);
        }
    }
}
