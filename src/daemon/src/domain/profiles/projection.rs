// SPDX-License-Identifier: GPL-3.0-or-later
//! Projects profile configuration and application icons into retained records.

use std::collections::HashMap;

use crate::config::Config;

pub fn profiles(cfg: &Config) -> halod_shared::types::ProfileState {
    halod_shared::types::ProfileState {
        active: cfg.active_profile.clone(),
        available: cfg.profile_names(),
        app_rules: cfg.app_rules.clone(),
        overrides: cfg.profile_overrides(),
    }
}

pub fn process_icons(cfg: &Config) -> HashMap<String, String> {
    let mut names: Vec<String> = cfg
        .app_rules
        .iter()
        .flat_map(|rule| rule.process_names.iter().cloned())
        .collect();
    names.sort();
    names.dedup();
    super::observers::running_apps::resolve_process_icons(&names)
}
