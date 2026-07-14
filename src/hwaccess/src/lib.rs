// SPDX-License-Identifier: GPL-3.0-or-later
//! `halod-hwaccess` — the raw privileged register-bus primitives.
//!
//! This crate owns *only* the low-level register access that needs elevated
//! privileges on Windows: the SMBus sync-op trait and its platform backends,
//! and the PawnIO kernel-driver bridge. It knows nothing about device
//! discovery or vendor wire formats — those stay in the daemon.
//!
//! On Windows the [`crate::smbus`] backends and [`crate::pawnio`] bridge are the
//! surface the elevated `halod-broker` process serves over RPC (see
//! [`crate::proto`]); the daemon reaches them either directly (Linux, or a
//! monolithic dev build) or through that RPC. On Linux everything here runs
//! in-process, unprivileged, gated only by `/dev/i2c-*` permissions.

pub mod proto;
pub mod smbus;

#[cfg(target_os = "windows")]
pub mod pawnio;

#[cfg(target_os = "windows")]
pub mod winsec;
