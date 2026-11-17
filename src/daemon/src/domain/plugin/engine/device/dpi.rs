// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use halod_shared::types::{DpiMode, DpiStatus};
use parking_lot::Mutex;

use super::super::worker::InitDpi;

/// Host-side DPI state: the step cycle, the device's limits and mode, and the
/// host-mode steps to restore when the device returns to host mode.
#[derive(Default)]
pub(super) struct DpiFacet(Mutex<Dpi>);

struct Dpi {
    steps: Vec<u16>,
    available: Vec<u16>,
    index: usize,
    current: u16,
    min: u16,
    max: u16,
    mode: DpiMode,
    mode_control: Option<String>,
    host_steps: Option<Vec<u16>>,
    reapply_attempted: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ModeSwitch {
    NotModeControl,
    Onboard,
    Host(Option<Vec<u16>>),
}

impl DpiFacet {
    /// Install the descriptor `initialize` reported. Host steps already
    /// restored from saved state win over the device's own.
    pub(super) fn install(&self, dpi: &InitDpi) {
        let mut state = self.0.lock();
        let host_steps = state.host_steps.take();
        *state = Dpi::from_runtime(dpi);
        if host_steps.is_some() || dpi.onboard {
            state.host_steps = host_steps;
        }
    }

    /// Replace the state with a fresh device report. Host steps the device
    /// reports differently are reapplied once; a second mismatch adopts the
    /// device's own steps so a firmware that normalises them is not rewritten
    /// on every poll.
    pub(super) fn refresh(&self, dpi: &InitDpi) -> Option<Vec<u16>> {
        let mut state = self.0.lock();
        let host_steps = state.host_steps.take();
        let attempted = state.reapply_attempted;
        *state = Dpi::from_runtime(dpi);
        let known = host_steps?;
        if dpi.onboard || known == dpi.steps {
            state.host_steps = Some(known);
            return None;
        }
        if attempted {
            state.host_steps = Some(dpi.steps.clone());
            return None;
        }
        state.host_steps = Some(known.clone());
        state.reapply_attempted = true;
        Some(known)
    }

    pub(super) fn validate_steps(&self, steps: &[u16]) -> Result<()> {
        anyhow::ensure!(!steps.is_empty(), "DPI steps list cannot be empty");
        let state = self.0.lock();
        for &step in steps {
            anyhow::ensure!(
                step >= state.min && step <= state.max,
                "DPI {step} is outside {}..={}",
                state.min,
                state.max
            );
            anyhow::ensure!(
                state.available.is_empty() || state.available.contains(&step),
                "DPI {step} is not supported by this device"
            );
        }
        Ok(())
    }

    pub(super) fn clamp(&self, dpi: u16) -> u16 {
        let state = self.0.lock();
        dpi.clamp(state.min, state.max)
    }

    pub(super) fn host_steps(&self) -> Option<Vec<u16>> {
        self.0.lock().host_steps.clone()
    }

    pub(super) fn is_onboard(&self) -> bool {
        self.0.lock().mode == DpiMode::Onboard
    }

    /// Keep `steps` for the next host-mode switch without writing them.
    pub(super) fn remember_host_steps(&self, steps: Vec<u16>) {
        self.0.lock().host_steps = Some(steps);
    }

    pub(super) fn step_at(&self, index: usize) -> Result<u16> {
        self.0
            .lock()
            .steps
            .get(index)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("dpi index {index} out of range"))
    }

    pub(super) fn record_steps(&self, steps: Vec<u16>) {
        let mut state = self.0.lock();
        if state.mode == DpiMode::Host {
            state.host_steps = Some(steps.clone());
        }
        state.index = steps
            .iter()
            .position(|step| *step == state.current)
            .unwrap_or(state.index.min(steps.len() - 1));
        state.steps = steps;
    }

    pub(super) fn record_index(&self, index: usize, value: u16) {
        let mut state = self.0.lock();
        state.index = index;
        state.current = value;
    }

    pub(super) fn record_current(&self, value: u16) {
        let mut state = self.0.lock();
        state.current = value;
        if let Some(index) = state.steps.iter().position(|step| *step == value) {
            state.index = index;
        }
    }

    /// Apply a boolean write to the mode control; switching to host mode
    /// yields the host steps to reapply.
    pub(super) fn switch_mode(&self, key: &str, host: bool) -> ModeSwitch {
        let mut state = self.0.lock();
        if state.mode_control.as_deref() != Some(key) {
            return ModeSwitch::NotModeControl;
        }
        if host {
            state.mode = DpiMode::Host;
            ModeSwitch::Host(state.host_steps.clone())
        } else {
            state.mode = DpiMode::Onboard;
            ModeSwitch::Onboard
        }
    }

    pub(super) fn status(&self) -> DpiStatus {
        let state = self.0.lock();
        DpiStatus {
            steps: state.steps.clone(),
            current_index: state.index,
            current_dpi: state.current,
            available_dpis: if state.available.is_empty() {
                (state.min..=state.max).step_by(100).collect()
            } else {
                state.available.clone()
            },
            mode: state.mode,
        }
    }

    #[cfg(test)]
    pub(super) fn set_mode(&self, mode: DpiMode) {
        self.0.lock().mode = mode;
    }
}

impl Default for Dpi {
    fn default() -> Self {
        Self {
            steps: Vec::new(),
            available: Vec::new(),
            index: 0,
            current: 0,
            min: 0,
            max: 0,
            mode: DpiMode::Host,
            mode_control: None,
            host_steps: None,
            reapply_attempted: false,
        }
    }
}

impl Dpi {
    /// Select the reported current DPI, else the midpoint step.
    fn from_runtime(dpi: &InitDpi) -> Self {
        let steps = dpi.steps.clone();
        let current = dpi
            .current
            .unwrap_or_else(|| steps.get(steps.len() / 2).copied().unwrap_or(dpi.min));
        let index = steps
            .iter()
            .position(|&step| step == current)
            .unwrap_or(steps.len() / 2);
        Self {
            host_steps: (!dpi.onboard).then(|| steps.clone()),
            reapply_attempted: false,
            steps,
            available: dpi.available_dpis.clone(),
            index,
            current,
            min: dpi.min,
            max: dpi.max,
            mode: if dpi.onboard {
                DpiMode::Onboard
            } else {
                DpiMode::Host
            },
            mode_control: dpi.mode_control.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(steps: &[u16], onboard: bool) -> InitDpi {
        InitDpi {
            min: 100,
            max: 3200,
            steps: steps.to_vec(),
            available_dpis: Vec::new(),
            onboard,
            mode_control: Some("host_mode".into()),
            current: None,
        }
    }

    #[test]
    fn install_and_refresh_keep_restored_host_steps_and_ask_to_reapply_them() {
        let facet = DpiFacet::default();
        facet.remember_host_steps(vec![400, 800]);
        facet.install(&descriptor(&[1600], false));
        assert_eq!(facet.host_steps(), Some(vec![400, 800]));
        assert_eq!(
            facet.refresh(&descriptor(&[1600], false)),
            Some(vec![400, 800])
        );
        assert_eq!(facet.refresh(&descriptor(&[400, 800], false)), None);
        assert_eq!(facet.refresh(&descriptor(&[3200], true)), None);
        assert_eq!(facet.host_steps(), Some(vec![400, 800]));
    }

    #[test]
    fn a_first_report_seeds_the_host_steps() {
        let facet = DpiFacet::default();
        assert_eq!(facet.refresh(&descriptor(&[800, 1600], false)), None);
        assert_eq!(facet.host_steps(), Some(vec![800, 1600]));
    }

    #[test]
    fn a_device_report_after_reinitialize_does_not_replace_known_host_steps() {
        let facet = DpiFacet::default();
        facet.install(&descriptor(&[400, 800, 1600], false));
        facet.record_steps(vec![1600, 400, 800]);
        assert_eq!(
            facet.refresh(&descriptor(&[400, 800, 1600], false)),
            Some(vec![1600, 400, 800])
        );
        assert_eq!(facet.host_steps(), Some(vec![1600, 400, 800]));
    }

    #[test]
    fn steps_a_device_keeps_normalising_are_reapplied_once_then_adopted() {
        let facet = DpiFacet::default();
        facet.install(&descriptor(&[400, 800, 1600], false));
        facet.record_steps(vec![1600, 400, 800]);
        assert!(facet
            .refresh(&descriptor(&[400, 800, 1600], false))
            .is_some());
        assert_eq!(facet.refresh(&descriptor(&[400, 800, 1600], false)), None);
        assert_eq!(facet.host_steps(), Some(vec![400, 800, 1600]));
        assert_eq!(facet.refresh(&descriptor(&[400, 800, 1600], false)), None);
    }

    #[test]
    fn only_the_mode_control_switches_mode_and_host_mode_reapplies_steps() {
        let facet = DpiFacet::default();
        facet.install(&descriptor(&[800, 1600], false));
        assert_eq!(
            facet.switch_mode("other", false),
            ModeSwitch::NotModeControl
        );
        assert_eq!(facet.switch_mode("host_mode", false), ModeSwitch::Onboard);
        assert!(facet.is_onboard());
        assert_eq!(
            facet.switch_mode("host_mode", true),
            ModeSwitch::Host(Some(vec![800, 1600]))
        );
        assert!(!facet.is_onboard());
    }

    #[test]
    fn onboard_step_writes_do_not_replace_the_host_steps() {
        let facet = DpiFacet::default();
        facet.install(&descriptor(&[800], false));
        facet.switch_mode("host_mode", false);
        facet.record_steps(vec![400]);
        assert_eq!(facet.host_steps(), Some(vec![800]));
        assert_eq!(facet.status().steps, vec![400]);
    }
}
