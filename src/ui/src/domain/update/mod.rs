// SPDX-License-Identifier: GPL-3.0-or-later
//! Application update banner state: what the daemon reported, what the
//! updater process is doing, and the pure derivation of what to show.

#[cfg(windows)]
mod windows;

use std::sync::{Arc, Mutex};

use halod_shared::types::UpdateStatus;
use halod_shared::update::{UpdateError, UpdaterEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    RunUpdater,
    OpenReleasePage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum UpdaterPhase {
    #[default]
    Idle,
    Downloading {
        done: u64,
        total: Option<u64>,
    },
    Verifying,
    Consent,
    Installing,
    Failed(UpdateError),
}

#[cfg_attr(not(windows), allow(dead_code))]
impl UpdaterPhase {
    pub fn after(self, event: UpdaterEvent) -> Self {
        match event {
            UpdaterEvent::Progress { done, total } => Self::Downloading { done, total },
            UpdaterEvent::Verifying => Self::Verifying,
            UpdaterEvent::Consent => Self::Consent,
            UpdaterEvent::Installing => Self::Installing,
            UpdaterEvent::Error { error } => Self::Failed(error),
        }
    }

    /// The phase once the updater's output ends without a final event.
    pub fn on_exit(self) -> Self {
        match self {
            Self::Downloading { .. } | Self::Verifying | Self::Consent => {
                Self::Failed(UpdateError::Io)
            }
            other => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BannerModel {
    Hidden,
    Available {
        version: String,
        action: UpdateAction,
    },
    Downloading {
        version: String,
        done: u64,
        total: Option<u64>,
    },
    Verifying {
        version: String,
    },
    Consent {
        version: String,
    },
    Failed {
        version: String,
        error: UpdateError,
        action: UpdateAction,
    },
}

impl BannerModel {
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Hidden => None,
            Self::Available { version, .. }
            | Self::Downloading { version, .. }
            | Self::Verifying { version }
            | Self::Consent { version }
            | Self::Failed { version, .. } => Some(version),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerEvent {
    Primary,
    ReleaseNotes,
    Cancel,
    Dismiss,
}

pub fn update_action(windows: bool, release_build: bool, updater_present: bool) -> UpdateAction {
    if windows && release_build && updater_present {
        UpdateAction::RunUpdater
    } else {
        UpdateAction::OpenReleasePage
    }
}

pub fn banner_model(
    status: &UpdateStatus,
    phase: &UpdaterPhase,
    dismissed: Option<&str>,
    action: UpdateAction,
) -> BannerModel {
    let Some(version) = status.available.clone() else {
        return BannerModel::Hidden;
    };
    match *phase {
        UpdaterPhase::Idle if dismissed == Some(version.as_str()) => BannerModel::Hidden,
        UpdaterPhase::Idle => BannerModel::Available { version, action },
        UpdaterPhase::Downloading { done, total } => BannerModel::Downloading {
            version,
            done,
            total,
        },
        UpdaterPhase::Verifying => BannerModel::Verifying { version },
        UpdaterPhase::Consent | UpdaterPhase::Installing => BannerModel::Consent { version },
        UpdaterPhase::Failed(error) => BannerModel::Failed {
            version,
            error,
            action,
        },
    }
}

pub fn fraction(done: u64, total: Option<u64>) -> Option<f32> {
    let total = total.filter(|total| *total > 0)?;
    Some((done as f64 / total as f64).clamp(0.0, 1.0) as f32)
}

pub fn format_bytes(done: u64, total: Option<u64>) -> String {
    let mb = |bytes: u64| format!("{:.1}", bytes as f64 / 1_000_000.0);
    match total {
        Some(total) => t!("shell.update_bytes", done = mb(done), total = mb(total)).to_string(),
        None => t!("shell.update_bytes_unknown", done = mb(done)).to_string(),
    }
}

/// The updater child process and the phase its output drives.
#[derive(Default)]
pub struct Updater {
    phase: Arc<Mutex<UpdaterPhase>>,
    #[cfg(windows)]
    run: Option<windows::Run>,
}

impl Updater {
    pub fn phase(&self) -> UpdaterPhase {
        self.phase.lock().map(|p| *p).unwrap_or_default()
    }

    fn set(&self, phase: UpdaterPhase) {
        if let Ok(mut current) = self.phase.lock() {
            *current = phase;
        }
    }

    #[cfg(windows)]
    pub fn fail(&self, error: UpdateError) {
        self.set(UpdaterPhase::Failed(error));
    }

    pub fn cancel(&mut self) {
        #[cfg(windows)]
        if let Some(run) = self.run.take() {
            run.stop();
        }
        self.set(UpdaterPhase::Idle);
    }

    #[cfg(windows)]
    pub fn start(&mut self, version: &str, ctx: &egui::Context) {
        self.cancel();
        self.set(UpdaterPhase::Downloading {
            done: 0,
            total: None,
        });
        match windows::start(version, self.phase.clone(), ctx.clone()) {
            Ok(run) => self.run = Some(run),
            Err(error) => {
                log::warn!("could not start the updater: {error}");
                self.fail(UpdateError::Io);
            }
        }
    }
}

#[cfg(windows)]
pub use windows::{remove_leftover_installer, updater_present};

#[cfg(not(windows))]
pub fn updater_present() -> bool {
    false
}

#[cfg(windows)]
pub fn update_failed_requested<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter()
        .any(|a| a == halod_shared::lifecycle::UPDATE_FAILED_ARG)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn available(version: &str) -> UpdateStatus {
        UpdateStatus {
            available: Some(version.into()),
            ..Default::default()
        }
    }

    const RUN: UpdateAction = UpdateAction::RunUpdater;

    #[test]
    fn only_a_hidden_banner_has_no_version() {
        assert_eq!(BannerModel::Hidden.version(), None);
        let model = banner_model(&available("0.14.0"), &UpdaterPhase::Consent, None, RUN);
        assert_eq!(model.version(), Some("0.14.0"));
    }

    #[test]
    fn hidden_without_an_available_version() {
        for phase in [UpdaterPhase::Idle, UpdaterPhase::Verifying] {
            assert_eq!(
                banner_model(&UpdateStatus::default(), &phase, None, RUN),
                BannerModel::Hidden
            );
        }
    }

    #[test]
    fn dismissing_hides_only_that_version() {
        let idle = UpdaterPhase::Idle;
        assert_eq!(
            banner_model(&available("0.14.0"), &idle, Some("0.14.0"), RUN),
            BannerModel::Hidden
        );
        assert_eq!(
            banner_model(&available("0.15.0"), &idle, Some("0.14.0"), RUN),
            BannerModel::Available {
                version: "0.15.0".into(),
                action: RUN
            }
        );
    }

    #[test]
    fn an_active_phase_wins_over_dismissal() {
        let phase = UpdaterPhase::Failed(UpdateError::Hash);
        assert_eq!(
            banner_model(&available("0.14.0"), &phase, Some("0.14.0"), RUN),
            BannerModel::Failed {
                version: "0.14.0".into(),
                error: UpdateError::Hash,
                action: RUN
            }
        );
    }

    #[test]
    fn installing_keeps_showing_the_consent_banner() {
        assert_eq!(
            banner_model(&available("0.14.0"), &UpdaterPhase::Installing, None, RUN),
            BannerModel::Consent {
                version: "0.14.0".into()
            }
        );
    }

    #[test]
    fn the_updater_runs_only_on_windows_release_builds_that_ship_it() {
        for windows in [false, true] {
            for release in [false, true] {
                for present in [false, true] {
                    let expected = if windows && release && present {
                        UpdateAction::RunUpdater
                    } else {
                        UpdateAction::OpenReleasePage
                    };
                    assert_eq!(update_action(windows, release, present), expected);
                }
            }
        }
    }

    #[test]
    fn events_drive_the_phase_and_an_early_exit_fails() {
        let phase = UpdaterPhase::Idle.after(UpdaterEvent::Progress {
            done: 5,
            total: Some(10),
        });
        assert_eq!(
            phase,
            UpdaterPhase::Downloading {
                done: 5,
                total: Some(10)
            }
        );
        assert_eq!(phase.on_exit(), UpdaterPhase::Failed(UpdateError::Io));
        assert_eq!(UpdaterPhase::Installing.on_exit(), UpdaterPhase::Installing);
        assert_eq!(UpdaterPhase::Idle.on_exit(), UpdaterPhase::Idle);
        assert_eq!(
            UpdaterPhase::Consent.after(UpdaterEvent::Error {
                error: UpdateError::Declined
            }),
            UpdaterPhase::Failed(UpdateError::Declined)
        );
    }

    #[test]
    fn format_bytes_shows_megabytes() {
        assert_eq!(
            format_bytes(23_400_000, Some(55_100_000)),
            "23.4 of 55.1 MB"
        );
        assert_eq!(format_bytes(1_500_000, None), "1.5 MB");
    }

    #[cfg(windows)]
    #[test]
    fn update_failed_flag_is_detected() {
        assert!(update_failed_requested([
            "halod-gui".into(),
            "--update-failed".into()
        ]));
        assert!(!update_failed_requested(["halod-gui".into()]));
    }

    proptest! {
        #[test]
        fn fraction_is_bounded(done in any::<u64>(), total in proptest::option::of(any::<u64>())) {
            if let Some(f) = fraction(done, total) {
                prop_assert!((0.0..=1.0).contains(&f));
            }
        }
    }

    #[test]
    fn fraction_is_unknown_without_a_total() {
        assert_eq!(fraction(5, None), None);
        assert_eq!(fraction(5, Some(0)), None);
        assert_eq!(fraction(5, Some(10)), Some(0.5));
    }
}
