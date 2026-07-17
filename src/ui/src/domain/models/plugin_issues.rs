// SPDX-License-Identifier: GPL-3.0-or-later
//! Localized detail text for structured plugin failures.

use std::collections::HashSet;

use halod_shared::types::{
    AppState, PluginDownloadConsent, PluginIssue, PluginIssueContext, PluginRepoLocation,
    PluginSource, RepoSignatureStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryIntegrityAlert {
    pub key: String,
    pub repository: Option<String>,
    pub package: String,
    pub expected: String,
    pub actual: String,
    pub restore_slug: Option<String>,
}

pub fn repository_integrity_alert(
    state: &AppState,
    notified: &HashSet<String>,
) -> Option<RepositoryIntegrityAlert> {
    state.plugins.plugins.iter().find_map(|plugin| {
        let (package, field, expected, actual) =
            match plugin.health.issue.as_ref()?.context.as_ref()? {
                PluginIssueContext::RepositoryHashMismatch {
                    package,
                    expected,
                    actual,
                } => (package, "sha256", expected, actual),
                PluginIssueContext::RepositoryManifestMismatch {
                    package,
                    field,
                    expected,
                    actual,
                } => (package, field.as_str(), expected, actual),
            };
        let repository = match &plugin.source {
            PluginSource::Repo { slug } => Some(slug.clone()),
            PluginSource::Local => None,
        };
        let key = format!(
            "{}/{package}/{field}/{expected}/{actual}",
            repository.as_deref().unwrap_or(&plugin.id)
        );
        if notified.contains(&key) {
            return None;
        }
        let restore_slug = repository.as_ref().and_then(|slug| {
            state
                .plugins
                .repos
                .iter()
                .find(|repo| repo.slug == *slug)
                .filter(|repo| {
                    repo.signature == RepoSignatureStatus::Verified
                        && (state.gui.plugin_downloads == PluginDownloadConsent::Allowed
                            || matches!(
                                repo.location,
                                PluginRepoLocation::LocalGit | PluginRepoLocation::LocalArchive
                            ))
                })
                .map(|repo| repo.slug.clone())
        });
        Some(RepositoryIntegrityAlert {
            key,
            repository,
            package: package.clone(),
            expected: expected.clone(),
            actual: actual.clone(),
            restore_slug,
        })
    })
}

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
        Some(PluginIssueContext::RepositoryManifestMismatch {
            package,
            field,
            expected,
            actual,
        }) => t!(
            "plugins.repository_manifest_mismatch",
            package = package,
            field = field,
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
    use halod_shared::types::{PluginIssueContext, PluginIssueKind, PluginRepoInfo};

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

    #[test]
    fn restore_is_offered_only_for_a_verified_repository() {
        let plugin = serde_json::from_value(serde_json::json!({
            "id": "halo_lcd",
            "name": "Halo LCD",
            "path": "/plugins/official/revisions/abc/plugins/halo_lcd/main.lua",
            "enabled": false,
            "source": { "kind": "repo", "slug": "official" },
            "health": {
                "status": "degraded",
                "issue": {
                    "kind": "load_failed",
                    "detail": "hash mismatch",
                    "context": {
                        "type": "repository_hash_mismatch",
                        "package": "halo_lcd",
                        "expected": "expected",
                        "actual": "actual"
                    },
                    "timestamp_ms": 0
                }
            }
        }))
        .unwrap();
        let repo = PluginRepoInfo {
            url: "https://example.invalid/plugins.git".into(),
            slug: "official".into(),
            repository_id: None,
            branch: None,
            locked_sha: "abc".into(),
            active_revision: Some("abc".into()),
            previous_verified_sha: None,
            last_sync: None,
            official: true,
            location: Default::default(),
            signature: RepoSignatureStatus::Verified,
            signing_key_fingerprint: None,
            compatibility: Default::default(),
        };
        let mut state = AppState::default();
        state.gui.plugin_downloads = PluginDownloadConsent::Allowed;
        state.plugins.plugins = vec![plugin];
        state.plugins.repos = vec![repo];

        let alert = repository_integrity_alert(&state, &HashSet::new()).unwrap();
        assert_eq!(alert.restore_slug.as_deref(), Some("official"));

        state.plugins.repos[0].signature = RepoSignatureStatus::Unsigned;
        let alert = repository_integrity_alert(&state, &HashSet::new()).unwrap();
        assert!(alert.restore_slug.is_none());

        state.plugins.repos[0].signature = RepoSignatureStatus::Verified;
        state.gui.plugin_downloads = PluginDownloadConsent::Denied;
        let alert = repository_integrity_alert(&state, &HashSet::new()).unwrap();
        assert!(alert.restore_slug.is_none());

        state.plugins.repos[0].location = PluginRepoLocation::LocalArchive;
        let alert = repository_integrity_alert(&state, &HashSet::new()).unwrap();
        assert_eq!(alert.restore_slug.as_deref(), Some("official"));

        state.plugins.plugins[0]
            .health
            .issue
            .as_mut()
            .unwrap()
            .context = Some(PluginIssueContext::RepositoryManifestMismatch {
            package: "halo_lcd".into(),
            field: "version".into(),
            expected: "2.0.0".into(),
            actual: "1.0.0".into(),
        });
        let alert = repository_integrity_alert(&state, &HashSet::new()).unwrap();
        assert_eq!(alert.restore_slug.as_deref(), Some("official"));
        assert_eq!(alert.expected, "2.0.0");
        assert_eq!(alert.actual, "1.0.0");

        let dismissed = HashSet::from([alert.key]);
        assert!(repository_integrity_alert(&state, &dismissed).is_none());
    }
}
