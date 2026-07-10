// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin transport backends. Each submits a `PluginTransportDescriptor` via
//! `inventory`, so adding a bus is a new file here (plus, if its I/O shape is
//! new, a `PluginIo` variant) — the plugin core never grows a per-bus branch.

mod hid;
mod smbus;
mod tcp;
