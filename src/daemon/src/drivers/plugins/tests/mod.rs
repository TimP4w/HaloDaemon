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

fn command_manifest(commands: &[&str]) -> super::PluginManifest {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("command_scope_test");
    std::fs::create_dir(&dir).unwrap();
    let commands = commands.join(", ");
    std::fs::write(
        dir.join("plugin.yaml"),
        format!(
            "id: command_scope_test\ntype: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [{commands}]\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("main.lua"), "return {}\n").unwrap();
    super::parse_manifest_from_dir(&dir).unwrap()
}

#[test]
fn consent_rejects_transport_scope_widening_under_old_acceptance() {
    let manifest = command_manifest(&["nvidia-smi", "rocm-smi"]);
    let mut state = super::PluginState::default();
    state.granted.insert(
        manifest.plugin_id.clone(),
        vec![halod_shared::types::Permission::Command],
    );
    state.accepted_authorities.insert(
        manifest.plugin_id.clone(),
        halod_shared::types::PluginAuthority {
            permissions: vec![halod_shared::types::Permission::Command],
            transport_scopes: vec!["command:nvidia-smi".to_owned()],
        },
    );

    assert!(!super::consent_satisfied_in(&state, &manifest));
}

#[test]
fn consent_accepts_authority_within_the_accepted_snapshot() {
    let manifest = command_manifest(&["nvidia-smi"]);
    let mut state = super::PluginState::default();
    state.accepted_authorities.insert(
        manifest.plugin_id.clone(),
        halod_shared::types::PluginAuthority {
            permissions: vec![halod_shared::types::Permission::Command],
            transport_scopes: vec![
                "command:nvidia-smi".to_owned(),
                "command:rocm-smi".to_owned(),
            ],
        },
    );

    assert!(super::consent_satisfied_in(&state, &manifest));
}

#[test]
fn indexed_repository_with_a_bad_digest_does_not_fall_back_to_loose_scanning() {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("plugins").join("demo");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("plugin.yaml"),
        "id: demo\nversion: 1.0.0\ntype: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [nvidia-smi]\n",
    )
    .unwrap();
    std::fs::write(package.join("main.lua"), "return {}\n").unwrap();
    std::fs::write(
        root.path().join("repository.yaml"),
        format!(
            "schema: 1\nid: test-repo\nname: Test repository\nversion: 1.0.0\ncompatibility:\n  halod: '>=0.3.0, <0.4.0'\n  plugin_api: 1\npackages:\n  - id: demo\n    path: plugins/demo\n    version: 1.0.0\n    sha256: {}\n",
            "0".repeat(64)
        ),
    )
    .unwrap();

    let mut scan = super::LoadScan::default();
    super::scan_repo(root.path(), &mut scan);

    assert!(scan.manifests.is_empty());
    assert_eq!(scan.invalid.len(), 1);
    assert_eq!(scan.invalid[0].0.plugin_id, "demo");
    assert!(scan.invalid[0].1.contains("integrity validation"));
    assert!(matches!(
        scan.invalid[0].2,
        Some(halod_shared::types::PluginIssueContext::RepositoryHashMismatch {
            ref package,
            ..
        }) if package == "demo"
    ));
}
