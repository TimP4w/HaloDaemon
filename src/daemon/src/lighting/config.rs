// SPDX-License-Identifier: GPL-3.0-or-later
use crate::config::Config;
use halod_shared::types::{CanvasState, LightingTargets, DEFAULT_PROFILE_NAME};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileLighting {
    /// `None` = inherit canvas from the default profile. `Some` = this profile
    /// overrides the canvas. The `default` profile may be either; effective
    /// readers fall back to `CanvasState::default()`.
    #[serde(default)]
    pub canvas: Option<CanvasState>,

    /// RGB Lighting view selection; strictly per profile (no inherit).
    #[serde(default)]
    pub targets: LightingTargets,
}

impl Config {
    /// Borrow the effective canvas state (active profile's override, else
    /// `default`'s, else a shared default) without cloning. Hot-path readers
    /// like the canvas engine tick use this to avoid a per-frame deep clone.
    pub fn effective_canvas_state_ref(&self) -> &CanvasState {
        static FALLBACK: std::sync::OnceLock<CanvasState> = std::sync::OnceLock::new();
        if self.active_profile != DEFAULT_PROFILE_NAME {
            if let Some(cs) = self
                .profiles
                .get(&self.active_profile)
                .and_then(|p| p.lighting.canvas.as_ref())
            {
                return cs;
            }
        }
        self.profiles
            .get(DEFAULT_PROFILE_NAME)
            .and_then(|p| p.lighting.canvas.as_ref())
            .unwrap_or_else(|| FALLBACK.get_or_init(CanvasState::default))
    }

    pub fn effective_canvas_state(&self) -> CanvasState {
        self.effective_canvas_state_ref().clone()
    }

    /// Mutable canvas state for the active profile, seeded from the effective
    /// state on first override so inherited fields are preserved.
    pub fn canvas_state_for_edit(&mut self) -> &mut CanvasState {
        let seed = if self.active_profile_data().lighting.canvas.is_none() {
            Some(self.effective_canvas_state())
        } else {
            None
        };
        let profile = self.active_profile_data_mut();
        if let Some(seed) = seed {
            profile.lighting.canvas = Some(seed);
        }
        profile
            .lighting
            .canvas
            .as_mut()
            .expect("canvas seeded above")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::config::Profile;

    #[test]
    fn first_canvas_edit_seeds_from_effective() {
        let mut cfg = Config::default();
        let def = CanvasState {
            sample_radius: 5.0,
            ..Default::default()
        };
        cfg.profiles
            .get_mut(DEFAULT_PROFILE_NAME)
            .unwrap()
            .lighting
            .canvas = Some(def);
        cfg.profiles.insert("gaming".into(), Profile::default());
        cfg.active_profile = "gaming".into();

        // Simulate a canvas edit: seed Some from effective, then mutate.
        let eff = cfg.effective_canvas_state();
        let cs = cfg
            .active_profile_data_mut()
            .lighting
            .canvas
            .get_or_insert(eff);
        cs.sample_radius = 9.0;

        let gaming = cfg.profiles.get("gaming").unwrap();
        // Inherited baseline carried in, then overridden value applied.
        assert_eq!(gaming.lighting.canvas.as_ref().unwrap().sample_radius, 9.0);
    }

    #[test]
    fn effective_canvas_inherits_default_when_profile_none() {
        let mut cfg = Config::default();
        let def_canvas = CanvasState {
            sample_radius: 7.0,
            ..Default::default()
        };
        cfg.profiles
            .get_mut(DEFAULT_PROFILE_NAME)
            .unwrap()
            .lighting
            .canvas = Some(def_canvas);
        cfg.profiles.insert("gaming".into(), Profile::default()); // canvas None
        cfg.active_profile = "gaming".into();
        assert_eq!(cfg.effective_canvas_state().sample_radius, 7.0);
    }

    #[test]
    fn new_profile_does_not_track_canvas() {
        let p = Profile::default();
        assert!(p.lighting.canvas.is_none());
    }

    #[test]
    fn concrete_canvas_deserializes_as_tracked() {
        // A present canvas object is an override (Some); only an absent
        // field inherits (None).
        let yaml = "lighting:\n  canvas:\n    sample_radius: 3.0\n";
        let p: Profile = serde_yaml::from_str(yaml).unwrap();
        assert!(
            p.lighting.canvas.is_some(),
            "present canvas must be tracked"
        );
    }
}
