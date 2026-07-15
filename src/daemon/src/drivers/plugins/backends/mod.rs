// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin transport backends: each submits a `PluginTransportDescriptor` via
//! `inventory`, so adding a bus is a new file here rather than a per-bus branch
//! in the plugin core.

mod amd_smn;
mod command;
mod hid;
mod lpcio;
mod smbus;
pub(crate) mod tcp;
pub(crate) mod usb;
