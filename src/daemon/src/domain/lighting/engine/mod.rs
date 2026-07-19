// SPDX-License-Identifier: GPL-3.0-or-later
mod canvas;
pub(crate) mod color;
mod direct;
#[allow(clippy::module_inception)] // public domain module and private runtime implementation
mod engine;

pub use engine::RgbEngine;
