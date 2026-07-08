// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Terry Cain and OpenRazer contributors <https://github.com/openrazer/openrazer>

//! DPI (class `0x04`) and polling-rate (class `0x00`) operations.

use anyhow::Result;

use super::{Razer, VARSTORE};
use crate::drivers::transports::Transport;

const CLASS_DPI: u8 = 0x04;
const ID_SET_DPI: u8 = 0x05;

const CLASS_MISC: u8 = 0x00;
const ID_SET_POLLING: u8 = 0x05;

/// Encode the DPI set arguments (`0x04 0x05`): X and Y are **big-endian** u16.
pub fn encode_dpi_xy(dpi_x: u16, dpi_y: u16) -> [u8; 7] {
    let [x_hi, x_lo] = dpi_x.to_be_bytes();
    let [y_hi, y_lo] = dpi_y.to_be_bytes();
    [VARSTORE, x_hi, x_lo, y_hi, y_lo, 0x00, 0x00]
}

impl<T: Transport> Razer<T> {
    pub async fn set_dpi(&self, dpi_x: u16, dpi_y: u16) -> Result<()> {
        self.send(CLASS_DPI, ID_SET_DPI, 0x07, &encode_dpi_xy(dpi_x, dpi_y))
            .await
    }

    pub async fn set_polling_rate(&self, code: u8) -> Result<()> {
        self.send(CLASS_MISC, ID_SET_POLLING, 0x01, &[code]).await
    }
}

#[cfg(test)]
mod tests {
    use super::super::build_report;
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn dpi_is_big_endian_x_then_y_after_storage() {
        assert_eq!(
            encode_dpi_xy(1800, 900),
            [VARSTORE, 0x07, 0x08, 0x03, 0x84, 0x00, 0x00]
        );
    }

    #[test]
    fn dpi_report_shape() {
        let buf = build_report(
            0x1F,
            CLASS_DPI,
            ID_SET_DPI,
            0x07,
            &encode_dpi_xy(1600, 1600),
        );
        assert_eq!(buf[7], 0x04, "class");
        assert_eq!(buf[8], 0x05, "id");
        assert_eq!(buf[6], 0x07, "data_size");
        assert_eq!(&buf[9..14], &[VARSTORE, 0x06, 0x40, 0x06, 0x40]);
    }

    proptest! {
        #[test]
        fn dpi_round_trips(x: u16, y: u16) {
            let e = encode_dpi_xy(x, y);
            prop_assert_eq!(u16::from_be_bytes([e[1], e[2]]), x);
            prop_assert_eq!(u16::from_be_bytes([e[3], e[4]]), y);
        }
    }
}
