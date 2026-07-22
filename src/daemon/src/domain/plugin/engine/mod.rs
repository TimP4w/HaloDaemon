// SPDX-License-Identifier: GPL-3.0-or-later
//! Sandboxed plugin execution and host runtime adapters.

mod audio_api;
pub(crate) mod backends;
mod bytebuf;
mod chain_leaf;
pub(crate) mod command_resolve;
pub(crate) mod data_api;
pub(crate) mod device;
pub(crate) mod effect_worker;
mod ffi;
pub(crate) mod http_api;
mod image_api;
mod lua_worker;
mod raster;
pub(super) mod sandbox;
pub(crate) mod transport;
mod transport_api;
pub(crate) mod udp_api;
pub(crate) mod widget_worker;
pub(crate) mod worker;

use crate::domain::plugin::manifest;
use crate::domain::plugin::manifest::contract;
use crate::domain::plugin::{PLUGIN_INSTRUCTION_BUDGET, PLUGIN_VM_MEMORY_BYTES};
