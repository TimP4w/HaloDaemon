// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! Device-settings features: report rate (`0x8060`/`0x8061`), adjustable DPI
//! (`0x2201`) and onboard profiles (`0x8100`).
//!
//! Each submodule holds its codecs plus the typed [`super::Hidpp20`] operations
//! that drive it; nothing here exposes wire bytes to the device.
pub mod dpi;
pub mod onboard;
pub mod report_rate;

pub use onboard::{
    build_onboard_profiles, parse_dpi_steps_from_sector, parse_profile_directory,
    patch_profile_sector, rom_source_sector, set_sector_crc, MODE_HOST, MODE_ONBOARD,
};
pub use report_rate::ReportRateOption;
