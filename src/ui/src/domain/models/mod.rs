// SPDX-License-Identifier: GPL-3.0-or-later
//! Pure derivations from the GUI topic store and `WireDevice` — no egui, no
//! channels, no mutation. Screens call these to turn wire types into display
//! values.

pub mod device;
pub mod device_tabs;
pub mod notifications;
pub mod plugin_issues;
pub mod sensors;
pub mod udev;
