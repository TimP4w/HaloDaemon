// SPDX-License-Identifier: GPL-3.0-or-later
//! App-wide mutable state: navigation (`Page`) and small transient UI flags
//! shared across the whole window. The `App` struct itself lives at the
//! crate root (`crate::app`) since it is the composition root that ties
//! `domain` state together with `ui` presentation types.

mod debounce;
mod hide_state;

pub use debounce::Debouncer;
pub use hide_state::HideState;

/// Home device-list layout: grid of cards or a compact list.
#[derive(Clone, Copy, PartialEq)]
pub enum Variant {
    Grid,
    List,
}

/// Which top-level screen is showing.
#[derive(Clone, PartialEq, Debug)]
pub enum Page {
    Home,
    Lighting,
    EffectDesigner,
    Cooling,
    Device(String),
    Settings,
    Plugins,
    Profile(String),
}

/// An in-progress device rename (target device id + edit buffer).
pub struct Rename {
    pub id: String,
    pub buf: String,
}

/// Rolling sensor/write-rate history length (seconds, sampled once per second).
pub(crate) const HISTORY_LEN: usize = 40;
