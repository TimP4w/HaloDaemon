// SPDX-License-Identifier: GPL-3.0-or-later
//! The generic Logitech HID++ 2.0 device: feature-driven capability composition.
//!
//! Discovery is vid/pid-only (see [`profile`]); on init the device enumerates its
//! HID++ feature table and assigns capabilities from what it finds — RGB, DPI,
//! onboard profiles, key remapping, report rate, battery, and audio (equalizer /
//! sidetone). A device with no matching profile still works, exposing whatever its
//! features advertise plus the common basics (name, serial). The model `profile`
//! table is optional enrichment (zone names, keyboard layouts, button labels).
//!
//! Each capability lives in its own file owning both its `init_*` and its
//! `Capability` impl; `device.rs` holds the struct, constructors, and the `Device`
//! trait orchestration.

pub mod audio;
pub mod battery;
pub mod boolean;
pub mod device;
pub mod init;
pub mod key_remap;
pub mod led_positions;
pub mod onboard;
pub mod profile;
pub mod report_rate;
pub mod rgb;
pub mod state;

pub use device::LogitechDevice;
