// SPDX-License-Identifier: GPL-3.0-or-later
//! Localized detail text for structured plugin failures.

use halod_shared::types::{PluginIssue, PluginIssueContext};

pub fn plugin_issue_detail(issue: &PluginIssue) -> String {
    match &issue.context {
        Some(PluginIssueContext::RepositoryHashMismatch {
            package,
            expected,
            actual,
        }) => t!(
            "plugins.repository_hash_mismatch",
            package = package,
            expected = expected,
            actual = actual
        )
        .to_string(),
        None => issue.detail.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{PluginIssueContext, PluginIssueKind};

    #[test]
    fn repository_hash_mismatch_uses_translated_copy() {
        let issue = PluginIssue {
            kind: PluginIssueKind::LoadFailed,
            detail: "raw backend text".into(),
            context: Some(PluginIssueContext::RepositoryHashMismatch {
                package: "ene_smbus".into(),
                expected: "expected-hash".into(),
                actual: "actual-hash".into(),
            }),
            timestamp_ms: 0,
        };

        let detail = plugin_issue_detail(&issue);
        assert!(detail.contains("ene_smbus"));
        assert!(detail.contains("expected-hash"));
        assert!(detail.contains("actual-hash"));
        assert!(!detail.contains("raw backend text"));
    }
}
