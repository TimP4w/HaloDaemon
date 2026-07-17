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
mod image_api;
mod lua_worker;
mod sandbox;
pub(crate) mod transport;
mod transport_api;
pub(crate) mod widget_worker;
pub(crate) mod worker;

use crate::plugin::manifest;
use crate::plugin::manifest::contract;
use crate::plugin::{PLUGIN_INSTRUCTION_BUDGET, PLUGIN_VM_MEMORY_BYTES};
