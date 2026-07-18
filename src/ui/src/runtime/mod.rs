// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-communication runtime: the IPC client and the Linux window-loop
//! backend that pumps it. Both are OS/daemon boundary code, not domain logic
//! or presentation.

pub mod ipc;
pub mod single_instance;
#[cfg(target_os = "linux")]
pub mod wayland_hide;
