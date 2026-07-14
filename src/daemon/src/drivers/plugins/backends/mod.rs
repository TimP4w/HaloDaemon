// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin transport backends: each submits a `PluginTransportDescriptor` via
//! `inventory`, so adding a bus is a new file here rather than a per-bus branch
//! in the plugin core.

mod command;
mod hid;
mod smbus;
pub(crate) mod tcp;
mod usb_control;
