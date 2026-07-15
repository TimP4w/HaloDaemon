// SPDX-License-Identifier: GPL-3.0-or-later
//! Generic plugin-machinery tests. Vendor-specific equivalence tests live in
//! the official plugin repository's `halod plugin-test` CI run.

// Registry coverage uses real directory packages beside the production loader.
// The former inline-Lua manifest fixture suite was removed with the legacy
// manifest format rather than preserved through a test-only compatibility path.

#[test]
fn declared_write_rate_limit_preserves_manifest_value() {
    let limit = super::declared_write_rate_limit(Some(12_345)).expect("declared limit");
    assert_eq!(limit.max_bytes_per_sec, 12_345);
    assert!(super::declared_write_rate_limit(None).is_none());
}

#[test]
fn init_failure_is_aggregated_and_clears_after_recovery() {
    let registry = super::Registry::default();
    registry.report_init_error("logitech", "g502", "ROOT timeout".into());

    let failed = registry.health_for("logitech");
    assert_eq!(failed.status, halod_shared::types::HealthStatus::Failed);
    assert_eq!(
        failed.issue.unwrap().kind,
        halod_shared::types::PluginIssueKind::InitFailed
    );

    registry.clear_init_error("logitech", "g502");
    assert_eq!(
        registry.health_for("logitech"),
        halod_shared::types::HealthState::default()
    );
}
