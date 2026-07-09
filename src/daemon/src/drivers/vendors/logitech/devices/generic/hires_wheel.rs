// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::LogitechDeviceState;

impl LogitechDevice {
    pub(super) async fn init_hires_wheel(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        let hidpp = self.hidpp2_with(features).await;
        if let Some(w) = hidpp.read_hires_wheel().await {
            let hw = &mut state.hires_wheel;
            hw.present = true;
            hw.multi = w.multi;
            hw.has_invert = w.has_invert;
            hw.has_ratchet = w.has_ratchet;
            hw.invert = w.invert;
            hw.hi_res = w.hi_res;
            hw.hidpp_target = w.hidpp_target;
            hw.ratchet = w.ratchet;
        }
    }

    pub(super) async fn init_fn_inversion(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        let hidpp = self.hidpp2_with(features).await;
        if let Some((inverted, default_inverted)) = hidpp.read_fn_inversion().await {
            state.fn_inversion.present = true;
            state.fn_inversion.inverted = !inverted;
            state.fn_inversion.default_inverted = !default_inverted;
            state.fn_inversion.writeable =
                hidpp.fn_inversion_version().await.is_some_and(|v| v >= 2);
        }
    }
}
