// SPDX-License-Identifier: GPL-3.0-or-later
use halod_plugin_signing::RepositorySigningKey;

use crate::config::PluginRepoRecord;
use crate::constants::OFFICIAL_PLUGIN_REPO_SLUG;
use halod_shared::app::OFFICIAL_PUBLIC_KEYS;

#[derive(Debug, Clone)]
pub enum RepositoryTrust {
    Official,
    Pinned(RepositorySigningKey),
    FirstUse,
}

impl RepositoryTrust {
    pub fn for_record(record: &PluginRepoRecord) -> Self {
        if record.slug == OFFICIAL_PLUGIN_REPO_SLUG {
            Self::Official
        } else if let Some(key) = &record.trusted_key {
            Self::Pinned(key.clone())
        } else {
            Self::FirstUse
        }
    }

    pub fn pinned_keys(&self) -> Option<Vec<(&str, &str)>> {
        match self {
            Self::Official => Some(OFFICIAL_PUBLIC_KEYS.to_vec()),
            Self::Pinned(key) => Some(vec![(key.id.as_str(), key.public_key.as_str())]),
            Self::FirstUse => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PluginRepoSource;
    use halod_plugin_signing::SignatureAlgorithm;

    fn record(slug: &str, trusted_key: Option<RepositorySigningKey>) -> PluginRepoRecord {
        PluginRepoRecord {
            slug: slug.into(),
            source: PluginRepoSource::Url(format!("https://{slug}.example/catalog.yaml")),
            trusted_key,
        }
    }

    #[test]
    fn trust_follows_the_record() {
        let key = RepositorySigningKey {
            id: "acme-2026".into(),
            algorithm: SignatureAlgorithm::Ed25519,
            public_key: "AAAA".into(),
        };
        assert_eq!(
            RepositoryTrust::for_record(&record(OFFICIAL_PLUGIN_REPO_SLUG, None)).pinned_keys(),
            Some(OFFICIAL_PUBLIC_KEYS.to_vec())
        );
        assert_eq!(
            RepositoryTrust::for_record(&record("acme", Some(key))).pinned_keys(),
            Some(vec![("acme-2026", "AAAA")])
        );
        assert_eq!(
            RepositoryTrust::for_record(&record("acme", None)).pinned_keys(),
            None
        );
    }
}
