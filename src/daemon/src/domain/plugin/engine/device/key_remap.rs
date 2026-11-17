// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;

use anyhow::Result;
use halod_shared::types::{ButtonAction, ButtonDescriptor, ButtonMapping};
use parking_lot::Mutex;

use super::super::worker::InitKeyRemap;
use crate::domain::input::validate::validate_cid;

/// The remappable buttons `initialize` reported and the host's mapping cache.
#[derive(Default)]
pub(super) struct KeyRemapFacet(Mutex<KeyRemap>);

#[derive(Default)]
struct KeyRemap {
    buttons: Vec<ButtonDescriptor>,
    requires_host_mode: bool,
    defaults: Vec<ButtonMapping>,
    mappings: HashMap<u16, ButtonMapping>,
}

pub(super) struct KeyRemapSnapshot {
    pub buttons: Vec<ButtonDescriptor>,
    pub mappings: Vec<ButtonMapping>,
    pub requires_host_mode: bool,
}

impl KeyRemapFacet {
    /// Install the reported buttons, seeding the device defaults unless
    /// mappings were already restored. Returns the mappings to re-arm.
    pub(super) fn install(&self, init: InitKeyRemap) -> Vec<ButtonMapping> {
        let mut state = self.0.lock();
        if state.mappings.is_empty() {
            state.mappings = init
                .default_mappings
                .iter()
                .map(|mapping| (mapping.cid, mapping.clone()))
                .collect();
        }
        state.buttons = init.buttons;
        state.requires_host_mode = init.requires_host_mode;
        state.defaults = init.default_mappings;
        state
            .mappings
            .values()
            .filter(|m| m.base != ButtonAction::Native || m.shifted != ButtonAction::Native)
            .cloned()
            .collect()
    }

    pub(super) fn validate(&self, mapping: &ButtonMapping) -> Result<()> {
        validate_cid(&self.0.lock().buttons, mapping)
    }

    pub(super) fn saved(&self) -> Vec<ButtonMapping> {
        let mut mappings: Vec<_> = self.0.lock().mappings.values().cloned().collect();
        mappings.sort_by_key(|mapping| mapping.cid);
        mappings
    }

    pub(super) fn snapshot(&self) -> KeyRemapSnapshot {
        let state = self.0.lock();
        KeyRemapSnapshot {
            buttons: state.buttons.clone(),
            mappings: state.mappings.values().cloned().collect(),
            requires_host_mode: state.requires_host_mode,
        }
    }

    pub(super) fn default_for(&self, cid: u16) -> Option<ButtonMapping> {
        self.0
            .lock()
            .defaults
            .iter()
            .find(|mapping| mapping.cid == cid)
            .cloned()
    }

    pub(super) fn defaults(&self) -> Vec<ButtonMapping> {
        self.0.lock().defaults.clone()
    }

    pub(super) fn record(&self, mapping: ButtonMapping) {
        self.0.lock().mappings.insert(mapping.cid, mapping);
    }

    pub(super) fn forget(&self, cid: u16) {
        self.0.lock().mappings.remove(&cid);
    }

    pub(super) fn clear(&self) {
        self.0.lock().mappings.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(cid: u16, base: ButtonAction) -> ButtonMapping {
        ButtonMapping {
            cid,
            base,
            shifted: ButtonAction::Native,
        }
    }

    fn init(defaults: Vec<ButtonMapping>) -> InitKeyRemap {
        InitKeyRemap {
            buttons: Vec::new(),
            requires_host_mode: false,
            default_mappings: defaults,
        }
    }

    #[test]
    fn install_seeds_defaults_and_rearms_only_non_native_mappings() {
        let facet = KeyRemapFacet::default();
        let rearm = facet.install(init(vec![
            mapping(1, ButtonAction::Native),
            mapping(2, ButtonAction::MomentaryDpi { dpi: 800 }),
        ]));
        assert_eq!(rearm.len(), 1);
        assert_eq!(rearm[0].cid, 2);
        assert_eq!(facet.saved().len(), 2);
    }

    #[test]
    fn restored_mappings_win_over_device_defaults() {
        let facet = KeyRemapFacet::default();
        facet.record(mapping(3, ButtonAction::MomentaryDpi { dpi: 1600 }));
        let rearm = facet.install(init(vec![mapping(1, ButtonAction::Native)]));
        assert_eq!(facet.saved().iter().map(|m| m.cid).collect::<Vec<_>>(), [3]);
        assert_eq!(rearm[0].cid, 3);
        assert_eq!(facet.default_for(1).map(|m| m.cid), Some(1));
    }
}
