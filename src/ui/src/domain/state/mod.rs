// SPDX-License-Identifier: GPL-3.0-or-later
//! App-wide mutable state: navigation (`Page`) and small transient UI flags
//! shared across the whole window. The `App` struct itself lives at the
//! crate root (`crate::app`) since it is the composition root that ties
//! `domain` state together with `ui` presentation types.

mod debounce;
mod hide_state;
pub mod undo;

pub use debounce::Debouncer;
use halod_shared::ids::{DeviceId, PluginId};
pub use hide_state::HideState;

/// Which top-level screen is showing.
#[derive(Clone, PartialEq, Debug)]
pub enum Page {
    Home,
    Lighting,
    EffectDesigner,
    Cooling,
    Device(DeviceId),
    Settings,
    Plugins,
    Profile(String),
}

/// An in-progress device rename (target device id + edit buffer).
pub struct Rename {
    pub id: DeviceId,
    pub buf: String,
}

/// Rolling sensor history length (seconds, sampled once per second).
pub(crate) const HISTORY_LEN: usize = 60;
/// One hour of 1 Hz write-rate samples, the longest device Info chart window.
pub(crate) const WRITE_RATE_HISTORY_LEN: usize = 3600;

/// Drop optimistic toggle locks whose plugin has vanished or whose `landed`
/// predicate says the daemon state caught up with the queued `target`.
/// Drops every pending entry whose plugin vanished or whose change landed, and
/// returns the ids that landed.
pub fn drain_landed<T>(
    pending: &mut std::collections::HashMap<PluginId, T>,
    plugins: &[halod_shared::types::PluginInfo],
    landed: impl Fn(&halod_shared::types::PluginInfo, &T) -> bool,
) -> Vec<PluginId> {
    let mut done = Vec::new();
    pending.retain(|id, target| match plugins.iter().find(|p| p.id == id) {
        Some(p) if landed(p, target) => {
            done.push(id.clone());
            false
        }
        Some(_) => true,
        None => false,
    });
    done
}
