// SPDX-License-Identifier: GPL-3.0-or-later
//! JSON-lines protocol the updater writes to stdout for the GUI.

use serde::{Deserialize, Serialize};

#[cfg(windows)]
pub fn update_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(
        std::path::PathBuf::from(base)
            .join(crate::app::APP_NAME)
            .join("update"),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateError {
    Network,
    Signature,
    Hash,
    TooLarge,
    Io,
    Stale,
    Declined,
    Installer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum UpdaterEvent {
    Progress { done: u64, total: Option<u64> },
    Verifying,
    Consent,
    Installing,
    Error { error: UpdateError },
}

impl UpdaterEvent {
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("updater events serialize")
    }

    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn error() -> impl Strategy<Value = UpdateError> {
        prop_oneof![
            Just(UpdateError::Network),
            Just(UpdateError::Signature),
            Just(UpdateError::Hash),
            Just(UpdateError::TooLarge),
            Just(UpdateError::Io),
            Just(UpdateError::Stale),
            Just(UpdateError::Declined),
            Just(UpdateError::Installer),
        ]
    }

    fn event() -> impl Strategy<Value = UpdaterEvent> {
        prop_oneof![
            (any::<u64>(), proptest::option::of(any::<u64>()))
                .prop_map(|(done, total)| UpdaterEvent::Progress { done, total }),
            Just(UpdaterEvent::Verifying),
            Just(UpdaterEvent::Consent),
            Just(UpdaterEvent::Installing),
            error().prop_map(|error| UpdaterEvent::Error { error }),
        ]
    }

    proptest! {
        #[test]
        fn events_round_trip_on_one_line(e in event()) {
            let line = e.encode();
            prop_assert!(!line.contains('\n'));
            prop_assert_eq!(UpdaterEvent::parse(&line), Some(e));
        }

        #[test]
        fn parse_never_panics(line in ".*") {
            let _ = UpdaterEvent::parse(&line);
        }
    }

    #[test]
    fn wire_format_is_stable() {
        assert_eq!(
            UpdaterEvent::Progress {
                done: 1,
                total: None
            }
            .encode(),
            r#"{"event":"progress","done":1,"total":null}"#
        );
        assert_eq!(
            UpdaterEvent::Error {
                error: UpdateError::TooLarge
            }
            .encode(),
            r#"{"event":"error","error":"too_large"}"#
        );
        assert_eq!(UpdaterEvent::parse(r#"{"event":"nope"}"#), None);
    }
}
