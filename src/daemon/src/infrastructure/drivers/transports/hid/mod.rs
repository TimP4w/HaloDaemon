// SPDX-License-Identifier: GPL-3.0-or-later
//! HID byte transport; discovery orchestration lives in `application::observers::hid`.

mod transport;

pub use transport::HidTransport;
