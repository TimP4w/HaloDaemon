// SPDX-License-Identifier: GPL-3.0-or-later
//! Concrete device discovery, transports, and vendor implementations.

pub mod transports;
pub mod vendors;

mod rate_limit;
pub use rate_limit::*;

// Compatibility re-export for infrastructure implementations while callers
// migrate to the domain-owned port path.
pub use crate::domain::device::*;
