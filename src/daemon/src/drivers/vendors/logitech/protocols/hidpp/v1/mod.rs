// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! HID++ 1.0 — the register-based protocol.
//!
//! Older Logitech devices and every Unifying/Lightspeed receiver speak HID++
//! 1.0: a flat set of numbered *registers* read/written with sub-ids
//! `0x80`–`0x83`. This module owns the register address space and the typed
//! [`Hidpp10`] handle the device layer calls; byte-level register access lives
//! on [`HidppMessenger`]. Receiver-specific operations (pairing, device count,
//! link notifications) live in [`receiver`].
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp10.py
use std::sync::Arc;

use anyhow::Result;

use super::HidppChannel;

pub mod receiver;

/// Receiver register: number of currently-paired devices.
pub const REG_DEVICE_COUNT: u16 = 0x0002;
/// Receiver register: per-slot pairing / device info.
pub const REG_RECEIVER_INFO: u16 = 0x02B5;
/// Receiver register: open/close the pairing lock and unpair a slot
/// (Unifying-style `0xB2` pairing register).
pub const REG_RECEIVER_PAIRING: u16 = 0x00B2;

/// `REG_RECEIVER_PAIRING` action: open the pairing lock (params `[0x01, 0x00, timeout]`).
pub const PAIRING_OPEN_LOCK: u8 = 0x01;
/// `REG_RECEIVER_PAIRING` action: close the pairing lock (params `[0x02, 0x00, 0x00]`).
pub const PAIRING_CLOSE_LOCK: u8 = 0x02;
/// `REG_RECEIVER_PAIRING` action: unpair a slot (params `[0x03, slot]`).
pub const PAIRING_UNPAIR: u8 = 0x03;

/// `REG_RECEIVER_INFO` sub-address for a slot's pairing info (`+ devnum - 1`).
pub const INFO_PAIRING: u8 = 0x20;
/// `REG_RECEIVER_INFO` sub-address for a slot's extended pairing info (`+ devnum - 1`).
pub const INFO_EXTENDED_PAIRING: u8 = 0x30;
/// `REG_RECEIVER_INFO` sub-address for a slot's device name (`+ devnum - 1`).
#[allow(dead_code)]
pub const INFO_DEVICE_NAME: u8 = 0x40;

/// Typed HID++ 1.0 handle: a messenger bound to one device number.
///
/// Cheap to construct (an `Arc` clone) and to drop, so it composes with the
/// device's snapshot-and-drop transport pattern. Register access is private;
/// callers use the typed operations in this module and [`receiver`].
#[derive(Clone)]
pub struct Hidpp10 {
    pub(crate) msg: Arc<dyn HidppChannel>,
    pub(crate) devnum: u8,
}

impl Hidpp10 {
    pub fn new(msg: Arc<dyn HidppChannel>, devnum: u8) -> Self {
        Self { msg, devnum }
    }

    /// Read a register, returning its raw reply payload.
    pub(crate) async fn read(&self, register: u16, params: &[u8]) -> Result<Vec<u8>> {
        self.msg.hidpp10_read(self.devnum, register, params).await
    }

    /// Write a register (fire-and-forget).
    pub(crate) async fn write(&self, register: u16, params: &[u8]) -> Result<()> {
        self.msg.hidpp10_write(self.devnum, register, params).await
    }
}
