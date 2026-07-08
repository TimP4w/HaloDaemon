// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Terry Cain and OpenRazer contributors <https://github.com/openrazer/openrazer>

//! Extended-matrix (class `0x0F`) LED operations: enable per-LED custom-frame
//! mode, stream a row of colours, and set zone brightness.

use anyhow::{bail, Result};

use super::{Razer, VARSTORE};
use crate::drivers::transports::Transport;
use halod_shared::types::RgbColor;

const CLASS_EXT_MATRIX: u8 = 0x0F;
const ID_EFFECT: u8 = 0x02;
const ID_CUSTOM_FRAME: u8 = 0x03;
const ID_BRIGHTNESS: u8 = 0x04;

const EFFECT_CUSTOM: u8 = 0x08;
const ENABLE_DATA_SIZE: u8 = 0x0C;

/// Fixed at `0x47` (`razer_chroma_extended_matrix_set_custom_frame`);
/// the report is 90 bytes regardless of how many columns are populated.
const CUSTOM_FRAME_DATA_SIZE: u8 = 0x47;

/// Arguments for the custom-frame row write (`0x0F 0x03`):
/// `[0,1]` unused, `[2]=row`, `[3]=start_col`, `[4]=stop_col`, `[5..]` RGB.
/// Errors when `start_col + colors.len()` would exceed 255.
pub fn custom_frame_args(row: u8, start_col: u8, colors: &[RgbColor]) -> Result<Vec<u8>> {
    if colors.is_empty() {
        return Ok(Vec::new());
    }
    let last = start_col as usize + colors.len() - 1;
    if last > u8::MAX as usize {
        bail!(
            "custom-frame run start_col={start_col} + {} exceeds column 255",
            colors.len()
        );
    }
    let stop_col = last as u8;
    let mut args = vec![0x00, 0x00, row, start_col, stop_col];
    for c in colors {
        args.extend_from_slice(&[c.r, c.g, c.b]);
    }
    Ok(args)
}

impl<T: Transport> Razer<T> {
    pub async fn enable_custom_frame(&self) -> Result<()> {
        self.send(
            CLASS_EXT_MATRIX,
            ID_EFFECT,
            ENABLE_DATA_SIZE,
            &[0x00, 0x00, EFFECT_CUSTOM],
        )
        .await
    }

    /// Stream one row of per-LED colours starting at `start_col`.
    pub async fn set_custom_frame(
        &self,
        row: u8,
        start_col: u8,
        colors: &[RgbColor],
    ) -> Result<()> {
        let args = custom_frame_args(row, start_col, colors)?;
        if args.is_empty() {
            return Ok(());
        }
        self.send(
            CLASS_EXT_MATRIX,
            ID_CUSTOM_FRAME,
            CUSTOM_FRAME_DATA_SIZE,
            &args,
        )
        .await
    }

    /// Set `led` zone brightness (0–255).
    pub async fn set_brightness(&self, led: u8, brightness: u8) -> Result<()> {
        self.send(
            CLASS_EXT_MATRIX,
            ID_BRIGHTNESS,
            0x03,
            &[VARSTORE, led, brightness],
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::build_report;
    use super::*;
    use proptest::prelude::*;

    fn c(r: u8, g: u8, b: u8) -> RgbColor {
        RgbColor { r, g, b }
    }

    #[test]
    fn custom_frame_args_layout() {
        let args = custom_frame_args(0, 0, &[c(1, 2, 3), c(4, 5, 6)]).unwrap();
        assert_eq!(args, vec![0, 0, 0, 0, 1, 1, 2, 3, 4, 5, 6]);
        assert_eq!(args.len(), 5 + 2 * 3);
    }

    #[test]
    fn custom_frame_stop_col_tracks_count_and_start() {
        let args = custom_frame_args(2, 4, &[c(0, 0, 0); 3]).unwrap();
        assert_eq!(args[2], 2, "row");
        assert_eq!(args[3], 4, "start_col");
        assert_eq!(args[4], 6, "stop_col = start + count - 1");
    }

    #[test]
    fn empty_frame_encodes_nothing() {
        assert!(custom_frame_args(0, 0, &[]).unwrap().is_empty());
    }

    #[test]
    fn run_past_last_column_errors_instead_of_wrapping() {
        assert!(custom_frame_args(0, 250, &[c(0, 0, 0); 20]).is_err());
        assert_eq!(custom_frame_args(0, 250, &[c(0, 0, 0); 6]).unwrap()[4], 255);
    }

    #[test]
    fn custom_frame_report_uses_class_0f_id_03_fixed_data_size() {
        let args = custom_frame_args(0, 0, &[c(9, 9, 9)]).unwrap();
        let buf = build_report(
            0x1F,
            CLASS_EXT_MATRIX,
            ID_CUSTOM_FRAME,
            CUSTOM_FRAME_DATA_SIZE,
            &args,
        );
        assert_eq!(buf[7], 0x0F);
        assert_eq!(buf[8], 0x03);
        assert_eq!(buf[6], 0x47);
        assert_eq!(&buf[9..14], &[0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(&buf[14..17], &[9, 9, 9]);
    }

    proptest! {
        #[test]
        fn stop_col_stays_within_u8_or_errors(start_col in 0u8..=255, count in 1usize..=300) {
            let colors = vec![c(0, 0, 0); count];
            match custom_frame_args(0, start_col, &colors) {
                Ok(args) => {
                    prop_assert!(start_col as usize + count - 1 <= u8::MAX as usize);
                    prop_assert_eq!(args[3], start_col);
                    prop_assert_eq!(args[4] as usize, start_col as usize + count - 1);
                }
                Err(_) => prop_assert!(start_col as usize + count - 1 > u8::MAX as usize),
            }
        }
    }
}
