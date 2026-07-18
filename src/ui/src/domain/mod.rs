// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain layer: lifecycle, tray, tour state machines, actions, models, and
//! navigation state. Paint-free — it never depends on `crate::ui` — though it
//! may hold bare egui data types (`Context`/`Id`/`Rect` for tour anchors,
//! `IconData`/`ViewportCommand` for the tray). The daemon communication
//! boundary lives in `runtime`. The composition root that ties this state to
//! `ui` presentation types is `App`, at the crate root (`crate::app`).

pub mod battery_notification;
pub mod lifecycle;
pub mod models;
pub mod native_notification;
pub mod state;
pub mod tour;
pub mod tray;
