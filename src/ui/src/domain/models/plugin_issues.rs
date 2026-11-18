// SPDX-License-Identifier: GPL-3.0-or-later
use halod_shared::types::{PluginIssue, PluginIssueContext};

pub fn plugin_issue_detail(issue: &PluginIssue) -> String {
    match &issue.context {
        Some(PluginIssueContext::ApiIncompatible {
            required,
            supported,
        }) => t!(
            "plugins.api_incompatible",
            required = required,
            supported = supported
        )
        .to_string(),
        None => issue.detail.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::PluginIssueKind;

    #[test]
    fn api_incompatibility_uses_translated_copy() {
        let issue = PluginIssue {
            kind: PluginIssueKind::LoadFailed,
            detail: "raw backend text".into(),
            context: Some(PluginIssueContext::ApiIncompatible {
                required: "4.0".into(),
                supported: "1.0".into(),
            }),
            timestamp_ms: 0,
        };

        let detail = plugin_issue_detail(&issue);
        assert!(detail.contains("4.0") && detail.contains("1.0"));
        assert!(!detail.contains("raw backend text"));
    }
}
