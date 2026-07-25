// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin transport backends: each defines a `PluginTransportDescriptor` listed
//! in `DESCRIPTORS`, so adding a bus is a new file here plus one table line,
//! rather than a per-bus branch in the plugin core.

mod amd_smn;
mod command;
mod hid;
mod hwmon;
mod lpcio;
pub(crate) mod net_guard;
mod serial;
mod smbus;
pub(crate) mod tcp;
pub(crate) mod usb;

use super::transport::PluginTransportDescriptor;

pub(super) static DESCRIPTORS: &[PluginTransportDescriptor] = &[
    hid::DESCRIPTOR,
    smbus::DESCRIPTOR,
    usb::DESCRIPTOR,
    serial::DESCRIPTOR,
    tcp::DESCRIPTOR,
    hwmon::DESCRIPTOR,
    command::DESCRIPTOR,
    lpcio::DESCRIPTOR,
    amd_smn::DESCRIPTOR,
];
