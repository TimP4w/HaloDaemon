// SPDX-License-Identifier: GPL-3.0-or-later
//! Application-level device domain.

mod capabilities;
pub mod chain;
pub mod lighting_segment;
pub mod policies;
pub mod projection;
mod slots;
mod traits;
pub mod usecases;

pub use capabilities::*;
pub use slots::*;
pub use traits::*;
