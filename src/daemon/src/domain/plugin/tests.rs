// SPDX-License-Identifier: GPL-3.0-or-later
//! Generic plugin-machinery tests. Vendor-specific equivalence tests live in
//! the official plugin repository's `halod plugin-test` CI run.
use crate::infrastructure::secrets::FileKeyStore;
use crate::infrastructure::secrets::SecretStore;
use crate::test_support;
use crate::test_support::MockDevice;
use crate::util::backoff;
use crate::util::backoff::FailureStreak;
use crate::util::backoff::Retry;
use halod_shared::ids::PluginId;

// Registry coverage uses real directory packages beside the production loader.
#[test]
fn declared_write_rate_limit_preserves_manifest_value() {
    let limit = super::declared_write_rate_limit(Some(12_345)).expect("declared limit");
    assert_eq!(limit.max_bytes_per_sec, 12_345);
    assert!(super::declared_write_rate_limit(None).is_none());
}

#[tokio::test(start_paused = true)]
async fn transport_open_attempt_resets_have_the_right_scope() {
    let registry = super::Registry::default();
    registry.note_transport_open_failure(("plug_a".parse().unwrap(), "plug_a-1".parse().unwrap()));
    registry.note_transport_open_failure(("other".parse().unwrap(), "other-1".parse().unwrap()));
    assert!(queue_recovery(&registry, &"plug_a".parse().unwrap()));
    assert!(queue_recovery(&registry, &"other".parse().unwrap()));

    registry.reset_transport_open_failures_for(&"plug_a".parse().unwrap());
    {
        let failures = registry.transport_open_failures.read();
        assert!(!failures.keys().any(|(plugin_id, _)| plugin_id == "plug_a"));
        assert!(failures.contains_key(&("other".parse().unwrap(), "other-1".parse().unwrap())));
    }
    assert!(
        queue_recovery(&registry, &"plug_a".parse().unwrap()),
        "the reset cancelled plug_a's queued recovery"
    );
    assert!(
        !queue_recovery(&registry, &"other".parse().unwrap()),
        "an unrelated plugin keeps waiting on its own"
    );

    registry.reset_transport_open_failures();
    assert!(registry.transport_open_failures.read().is_empty());
    assert!(queue_recovery(&registry, &"other".parse().unwrap()));
}

#[test]
fn transport_open_failures_back_off_between_attempts() {
    let registry = super::Registry::default();
    let key = ("plug_a".parse().unwrap(), "plug_a-1".parse().unwrap());
    assert!(!registry.transport_open_blocked(&key));

    // A fresh failure blocks immediate re-offers from polling scanners...
    assert_eq!(registry.note_transport_open_failure(key.clone()), 1);
    assert!(registry.transport_open_blocked(&key));

    // ...but only until the backoff elapses.
    let age = |registry: &super::Registry| {
        let mut failures = registry.transport_open_failures.write();
        let streak = failures.get_mut(&key).unwrap();
        let then = tokio::time::Instant::now() - super::TRANSPORT_OPEN_BACKOFF;
        let mut aged = FailureStreak::first(then);
        for _ in 1..streak.failures() {
            aged.record(then);
        }
        *streak = aged;
    };
    age(&registry);
    assert!(!registry.transport_open_blocked(&key));

    // Exhausting the budget blocks regardless of elapsed time.
    for attempt in 2..=super::TRANSPORT_OPEN_ATTEMPTS {
        assert_eq!(registry.note_transport_open_failure(key.clone()), attempt);
    }
    age(&registry);
    assert!(registry.transport_open_blocked(&key));
}

/// Queue a recovery for `plugin_id` the way a runtime error would, standing in
/// for the reconcile the daemon would run.
fn queue_recovery(registry: &super::Registry, plugin_id: &PluginId) -> bool {
    backoff::schedule(
        &registry.runtime_recovery,
        plugin_id,
        super::RUNTIME_RECOVERY_ATTEMPTS,
        |_| async { Retry::Stop },
    )
}

#[tokio::test(start_paused = true)]
async fn runtime_recovery_budget_is_spent_once_and_refunded_by_a_real_recovery() {
    let registry = super::Registry::default();
    for _ in 0..super::RUNTIME_RECOVERY_ATTEMPTS {
        assert!(queue_recovery(&registry, &"plug_a".parse().unwrap()));
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    }
    assert!(
        !queue_recovery(&registry, &"plug_a".parse().unwrap()),
        "the budget is spent"
    );

    // A sibling device that was never failing succeeds on every frame; that
    // must not refund the budget of the one that keeps failing.
    registry.clear_runtime_error(&"plug_a".parse().unwrap(), &"healthy-dev".parse().unwrap());
    assert!(!queue_recovery(&registry, &"plug_a".parse().unwrap()));

    registry.set_health(
        super::HealthScope::device(&"plug_a".parse().unwrap(), &"sick-dev".parse().unwrap()),
        halod_shared::types::PluginIssueKind::RuntimeError,
        "connection reset".into(),
    );
    registry.clear_runtime_error(&"plug_a".parse().unwrap(), &"sick-dev".parse().unwrap());
    assert!(
        queue_recovery(&registry, &"plug_a".parse().unwrap()),
        "a recovered device starts a fresh episode"
    );
}

#[test]
fn init_failure_is_aggregated_and_clears_after_recovery() {
    let registry = super::Registry::default();
    registry.report_init_error(
        &"plug_b".parse().unwrap(),
        &"plug_b-dev".parse().unwrap(),
        "ROOT timeout".into(),
    );

    let failed = registry.health_for(&"plug_b".parse().unwrap());
    assert_eq!(failed.status, halod_shared::types::HealthStatus::Failed);
    assert_eq!(
        failed.issue.unwrap().kind,
        halod_shared::types::PluginIssueKind::InitFailed
    );

    registry.clear_init_error(&"plug_b".parse().unwrap(), &"plug_b-dev".parse().unwrap());
    assert_eq!(
        registry.health_for(&"plug_b".parse().unwrap()),
        halod_shared::types::HealthState::default()
    );
}

#[test]
fn pid_filters_admit_listed_or_any_pid() {
    assert!(super::ids_list_pid(&[0x3012], Some(0), 0x3012));
    assert!(!super::ids_list_pid(&[0x3012], Some(0x3013), 0x3013));
    assert!(super::ids_list_pid(&[], Some(0x3013), 0x3013));
    assert!(!super::ids_list_pid(&[], Some(0x3013), 0x3012));
    assert!(super::ids_list_pid(&[], None, 0xc547));
    assert!(super::ids_list_pid(&[], Some(0), 0xc547));
}

#[test]
fn init_failures_share_the_open_budget_until_a_successful_init() {
    let registry = super::Registry::default();
    let key = ("plug_b".parse().unwrap(), "plug_b-dev".parse().unwrap());
    for attempt in 1..=super::TRANSPORT_OPEN_ATTEMPTS {
        assert_eq!(
            registry.report_init_error(
                &"plug_b".parse().unwrap(),
                &"plug_b-dev".parse().unwrap(),
                "timeout".into()
            ),
            attempt
        );
    }
    assert!(registry.transport_open_blocked(&key));
    assert!(registry.retryable_open_failure_kinds().is_empty());

    registry.clear_init_error(&"plug_b".parse().unwrap(), &"plug_b-dev".parse().unwrap());
    assert!(!registry.transport_open_blocked(&key));
}

#[tokio::test(start_paused = true)]
async fn runtime_error_from_a_device_that_left_is_dropped_after_the_grace() {
    let app = test_support::app_with(vec![]);
    app.registry
        .report_runtime_error(
            &app,
            &"plug_a".parse().unwrap(),
            &"plug_a-gone".parse().unwrap(),
            "Broken pipe".into(),
        )
        .await;
    assert_eq!(
        app.registry.health_for(&"plug_a".parse().unwrap()).status,
        halod_shared::types::HealthStatus::Failed
    );

    tokio::time::sleep(super::RUNTIME_ERROR_GRACE * 2).await;

    assert_eq!(
        app.registry.health_for(&"plug_a".parse().unwrap()),
        halod_shared::types::HealthState::default()
    );
    assert!(
        queue_recovery(&app.registry, &"plug_a".parse().unwrap()),
        "no recovery was queued"
    );
}

#[tokio::test(start_paused = true)]
async fn runtime_error_from_a_present_device_queues_recovery_after_the_grace() {
    let device = std::sync::Arc::new(MockDevice::new("plug_a-live"));
    let app = test_support::app_with(vec![device]);
    app.registry
        .report_runtime_error(
            &app,
            &"plug_a".parse().unwrap(),
            &"plug_a-live".parse().unwrap(),
            "Broken pipe".into(),
        )
        .await;
    assert!(
        queue_recovery(&app.registry, &"plug_a".parse().unwrap()),
        "nothing queued yet"
    );
    app.registry.runtime_recovery.clear("plug_a");

    tokio::time::sleep(super::RUNTIME_ERROR_GRACE + std::time::Duration::from_millis(10)).await;

    assert!(!queue_recovery(&app.registry, &"plug_a".parse().unwrap()));
}

#[tokio::test(start_paused = true)]
async fn runtime_error_that_recovers_within_the_grace_stays_silent() {
    let device = std::sync::Arc::new(MockDevice::new("plug_a-live"));
    let app = test_support::app_with(vec![device]);
    app.registry
        .report_runtime_error(
            &app,
            &"plug_a".parse().unwrap(),
            &"plug_a-live".parse().unwrap(),
            "Broken pipe".into(),
        )
        .await;
    app.registry
        .clear_runtime_error(&"plug_a".parse().unwrap(), &"plug_a-live".parse().unwrap());

    tokio::time::sleep(super::RUNTIME_ERROR_GRACE * 2).await;

    assert!(queue_recovery(&app.registry, &"plug_a".parse().unwrap()));
}

#[test]
fn fresh_integration_runtime_clears_stale_device_errors() {
    let registry = super::Registry::default();
    registry.set_load_warning(&"openrgb".parse().unwrap(), "logo is unavailable".into());
    registry.set_health(
        super::HealthScope::device(
            &"openrgb".parse().unwrap(),
            &"old-controller".parse().unwrap(),
        ),
        halod_shared::types::PluginIssueKind::RuntimeError,
        "connection reset".into(),
    );
    registry.set_health(
        super::HealthScope::plugin(&"unrelated".parse().unwrap()),
        halod_shared::types::PluginIssueKind::ConnectFailed,
        "still offline".into(),
    );

    registry.clear_integration_operational_errors(&"openrgb".parse().unwrap());

    let recovered = registry.health_for(&"openrgb".parse().unwrap());
    assert_eq!(
        recovered.issue.unwrap().kind,
        halod_shared::types::PluginIssueKind::LoadWarning,
        "non-operational package warnings must survive a reconnect"
    );
    assert_eq!(
        registry
            .health_for(&"unrelated".parse().unwrap())
            .issue
            .unwrap()
            .kind,
        halod_shared::types::PluginIssueKind::ConnectFailed
    );

    registry.set_health(
        super::HealthScope::plugin(&"openrgb".parse().unwrap()),
        halod_shared::types::PluginIssueKind::ConnectFailed,
        "old connect error".into(),
    );
    registry.clear_integration_operational_errors(&"openrgb".parse().unwrap());
    assert_eq!(
        registry.health_for(&"openrgb".parse().unwrap()),
        halod_shared::types::HealthState::default()
    );
}

#[test]
fn refresh_requirements_caches_evaluated_statuses() {
    let manifest = command_manifest(&["liquidctl"]);
    let registry = super::Registry::default();
    registry.update(|s| s.manifests = vec![manifest.clone()]);

    registry.refresh_requirements();
    // The cache is read (no re-probe) and reflects the missing command.
    let statuses = registry.requirement_statuses(&manifest);
    assert!(statuses.iter().any(|s| !s.satisfied));
}

#[test]
fn integration_with_missing_command_is_gated_out() {
    let manifest = command_manifest(&["liquidctl"]);
    let registry = super::Registry::default();
    registry.update(|s| s.manifests = vec![manifest.clone()]);

    assert!(
        registry.integration_manifests().is_empty(),
        "an enabled integration whose command is missing must be inactive"
    );
    assert!(registry.integration_manifest(&manifest.plugin_id).is_none());
}

#[test]
fn disabled_integration_is_not_reported_active() {
    struct NoSecrets;
    impl SecretStore for NoSecrets {
        fn set(&self, _: &PluginId, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn get(&self, _: &PluginId, _: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn delete(&self, _: &PluginId, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    let mut manifest = command_manifest(&["liquidctl"]);
    manifest.requirements.push(super::manifest::RequirementDef {
        kind: super::manifest::RequirementDefKind::Command,
        name: "liquidctl".into(),
        platforms: vec![],
    });
    let registry = super::Registry::default();
    registry.update(|s| {
        s.manifests = vec![manifest.clone()];
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
            "id: command_scope_test\nversion: 1.0.0\napi: \"1.0\"\ntype: integration\ntransports:\n  command:\n    commands: [{commands}]\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("main.lua"), "return {}\n").unwrap();
    super::parse_manifest_from_dir(&dir).unwrap()
}

fn write_plugin(root: &std::path::Path, id: &str, api: &str) {
    test_support::write_plugin_package(root, id, "1.0.0", api);
}

#[test]
fn a_local_repo_shadows_an_installed_plugin() {
    use crate::config::{PluginRepoRecord, PluginRepoSource};
    use halod_shared::types::PluginProvenance;
    let _config = test_support::tmp_config_dir();
    let installed = crate::config::plugins_dir().join("community");
    write_plugin(&installed, "demo", "1.0");
    write_plugin(&installed, "other", "1.0");
    std::fs::create_dir_all(installed.join(".demo.staging-1")).unwrap();
    let local = tempfile::tempdir().unwrap();
    write_plugin(local.path(), "demo", "1.0");

    let registry = super::Registry::default();
    registry.load_all(&[
        PluginRepoRecord {
            slug: "community".into(),
            source: PluginRepoSource::Url("https://example.org/catalog.yaml".into()),
            trusted_key: None,
        },
        PluginRepoRecord {
            slug: "local-dev".into(),
            source: PluginRepoSource::Local(local.path().to_path_buf()),
            trusted_key: None,
        },
    ]);

    let plugins = registry.list(&FileKeyStore::new());
    let origin = |id: &str| {
        let plugin = plugins.iter().find(|p| p.id == id).unwrap();
        (plugin.repo.clone(), plugin.provenance)
    };
    assert_eq!(plugins.len(), 2);
    assert_eq!(
        origin("demo"),
        ("local-dev".into(), PluginProvenance::Local)
    );
    assert_eq!(
        origin("other"),
        ("community".into(), PluginProvenance::Unsigned)
    );
}

#[test]
fn an_incompatible_api_is_listed_but_not_loaded() {
    let root = tempfile::tempdir().unwrap();
    write_plugin(root.path(), "future", "9.0");
    let registry = super::Registry::default();
    registry.load_dir(root.path());

    assert!(registry.installed().is_empty());
    let issue = registry
        .list(&FileKeyStore::new())
        .pop()
        .and_then(|info| info.health.issue)
        .unwrap();
    assert!(matches!(
        issue.context,
        Some(halod_shared::types::PluginIssueContext::ApiIncompatible { ref required, .. })
            if required == "9.0"
    ));
}

#[tokio::test]
async fn the_restore_scope_covers_its_own_task_only() {
    assert!(!super::restore_in_flight());

    let seen = super::as_restore(async {
        // A concurrently spawned task is a separate activation: a user-driven
        // call must keep reporting while a restore is in flight.
        let sibling = tokio::spawn(async { super::restore_in_flight() })
            .await
            .expect("sibling task");
        (super::restore_in_flight(), sibling)
    })
    .await;

    assert_eq!(seen, (true, false));
    assert!(!super::restore_in_flight(), "the scope ends with the pass");
}
