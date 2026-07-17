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

#[test]
fn fresh_integration_runtime_clears_stale_device_errors() {
    let registry = super::Registry::default();
    registry.set_load_warning("openrgb", "logo is unavailable".into());
    registry.set_health(
        "openrgb::old-controller",
        halod_shared::types::PluginIssueKind::RuntimeError,
        "connection reset".into(),
    );
    registry.set_health(
        "unrelated",
        halod_shared::types::PluginIssueKind::ConnectFailed,
        "still offline".into(),
    );

    registry.clear_integration_operational_errors("openrgb");

    let recovered = registry.health_for("openrgb");
    assert_eq!(
        recovered.issue.unwrap().kind,
        halod_shared::types::PluginIssueKind::LoadWarning,
        "non-operational package warnings must survive a reconnect"
    );
    assert_eq!(
        registry.health_for("unrelated").issue.unwrap().kind,
        halod_shared::types::PluginIssueKind::ConnectFailed
    );

    registry.set_health(
        "openrgb",
        halod_shared::types::PluginIssueKind::ConnectFailed,
        "old connect error".into(),
    );
    registry.clear_integration_operational_errors("openrgb");
    assert_eq!(
        registry.health_for("openrgb"),
        halod_shared::types::HealthState::default()
    );
}

#[test]
fn missing_command_requirement_is_daemon_authoritative() {
    let manifest = command_manifest(&["definitely-not-a-real-binary-xyz-42"]);
    let registry = super::Registry::default();
    registry.update(|s| s.manifests = vec![manifest.clone()]);

    // The force-refreshed gate the enable flow consults reports the unmet
    // requirement, so a stale GUI cannot enable past it.
    let missing = registry.missing_blocking_requirements(&manifest.plugin_id);
    assert_eq!(missing.len(), 1);
    assert!(!missing[0].satisfied);

    // An unknown plugin has no requirements rather than erroring.
    assert!(registry.missing_blocking_requirements("nope").is_empty());
}

#[test]
fn refresh_requirements_caches_evaluated_statuses() {
    let manifest = command_manifest(&["definitely-not-a-real-binary-xyz-42"]);
    let registry = super::Registry::default();
    registry.update(|s| s.manifests = vec![manifest.clone()]);

    registry.refresh_requirements();
    // The cache is read (no re-probe) and reflects the missing command.
    let statuses = registry.requirement_statuses(&manifest);
    assert!(statuses.iter().any(|s| !s.satisfied));
}

#[test]
fn consented_integration_with_missing_command_is_gated_out() {
    let manifest = command_manifest(&["definitely-not-a-real-binary-xyz-42"]);
    let registry = super::Registry::default();
    let authority = super::authority_for_manifest(&manifest);
    registry.update(|s| {
        s.manifests = vec![manifest.clone()];
        s.accepted_authorities
            .insert(manifest.plugin_id.clone(), authority);
    });

    // Consent is satisfied, so exclusion is purely the requirement gate — a
    // blocked integration must not be handed to the integration scanner/monitor.
    assert!(registry.consent_satisfied(&manifest));
    assert!(
        registry.integration_manifests().is_empty(),
        "an enabled, consented integration whose command is missing must be inactive"
    );
    assert!(registry.integration_manifest(&manifest.plugin_id).is_none());
}

#[test]
fn disabled_integration_is_not_reported_active() {
    struct NoSecrets;
    impl crate::secrets::SecretStore for NoSecrets {
        fn set(&self, _: &str, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn get(&self, _: &str, _: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn delete(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let mut manifest = command_manifest(&["definitely-not-a-real-binary-xyz-42"]);
    manifest.requirements.push(super::manifest::RequirementDef {
        kind: super::manifest::RequirementDefKind::Command,
        name: "definitely-not-a-real-binary-xyz-42".into(),
        platforms: vec![],
    });
    let registry = super::Registry::default();
    let authority = super::authority_for_manifest(&manifest);
    registry.update(|s| {
        s.manifests = vec![manifest.clone()];
        s.accepted_authorities
            .insert(manifest.plugin_id.clone(), authority);
        s.integrations_disabled.insert(manifest.plugin_id.clone());
    });
    let info = registry.list(&NoSecrets).pop().unwrap();
    assert!(info.enabled, "the plugin-level toggle remains enabled");
    assert!(!info.integration_enabled);
    assert!(!info.active);
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
            data_reads: vec![],
        },
    );

    assert!(!super::consent_satisfied_in(&state, &manifest));
}

#[test]
fn consent_rejects_new_cross_plugin_data_read() {
    let accepted = halod_shared::types::PluginAuthority::default();
    let requested = halod_shared::types::PluginAuthority {
        data_reads: vec!["telemetry.current".into()],
        ..Default::default()
    };
    assert!(!requested.is_subset_of(&accepted));
    assert!(requested.is_subset_of(&requested));
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
            data_reads: vec![],
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
            "schema: 1\nid: test-repo\nname: Test repository\nversion: 1.0.0\ncompatibility:\n  halod: '>=0.0.0'\n  plugin_api: 1\npackages:\n  - id: demo\n    path: plugins/demo\n    version: 1.0.0\n    sha256: {}\n",
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

#[test]
fn trusted_repo_scan_loads_packages_despite_a_bad_digest() {
    // The dev repo (`--dev-plugin-repo`) is edited in place, so its hashes won't
    // match the generated index. A trusted scan must load anyway.
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
            "schema: 1\nid: test-repo\nname: Test repository\nversion: 1.0.0\ncompatibility:\n  halod: '>=0.0.0'\n  plugin_api: 1\npackages:\n  - id: demo\n    path: plugins/demo\n    version: 1.0.0\n    sha256: {}\n",
            "0".repeat(64)
        ),
    )
    .unwrap();

    let mut scan = super::LoadScan::default();
    super::scan_repo_trusted(root.path(), &mut scan);

    assert_eq!(scan.manifests.len(), 1);
    assert_eq!(scan.manifests[0].plugin_id, "demo");
    assert!(scan.invalid.is_empty());
}
