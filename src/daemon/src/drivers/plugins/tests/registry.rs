// SPDX-License-Identifier: GPL-3.0-or-later
//! Tests for the plugin registry itself (matching, consent, config
//! resolution, discovery notifications) — as opposed to the
//! per-plugin equivalence tests in the sibling modules.

use super::super::*;
use crate::secrets::SecretStore as _;
use std::path::Path;
use std::sync::Arc;

fn manifest() -> PluginManifest {
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0x1234, pid = 0x5678, vendor = "Acme", model = "K1", name = "Acme K1" } },
        }
    "#;
    parse_manifest(src, Path::new("acme_k1.lua")).unwrap()
}

fn hid<'a>(vid: u16, pid: u16, serial: Option<&'a str>, idx: usize) -> DiscoveryHandle<'a> {
    DiscoveryHandle::Hid {
        vid,
        pid,
        path: "p",
        serial,
        idx,
        usage_page: 0,
        usage: 0,
        interface_number: None,
    }
}

/// Replace the registry snapshot's manifests (and their derived effect
/// entries) for a test.
fn set_registry(reg: &Registry, manifests: Vec<PluginManifest>) {
    let effects: Vec<PluginEffectEntry> = manifests.iter().flat_map(effect_entries_for).collect();
    reg.update(|s| {
        s.manifests = manifests;
        s.effects = effects;
    });
    reg.update(advance_activation);
}

/// Acknowledge every manifest's current content, as user consent would, so
/// `consent_satisfied` treats them as consented.
fn acknowledge(reg: &Registry, manifests: &[PluginManifest]) {
    let map = manifests
        .iter()
        .map(|m| (m.plugin_id.clone(), m.content_hash()))
        .collect();
    reg.set_acknowledged(&map);
}

#[test]
fn matching_handle_builds_device_with_identity() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let manifests = vec![manifest()];
    set_registry(&app.registry, manifests.clone());
    let dev = app
        .registry
        .match_in(&app, &manifests, &hid(0x1234, 0x5678, Some("SER"), 0))
        .expect("matches");
    assert_eq!(dev.vendor(), "Acme");
    assert_eq!(dev.name(), "Acme K1");
    assert_eq!(dev.id(), "acme_k1-SER");
}

#[test]
fn registries_are_isolated_per_app_state() {
    // The whole point of moving the registry onto `AppState`: disabling a plugin
    // in one `AppState` must not leak into another's registry.
    let a = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let b = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    a.registry.set_disabled(&["p".to_string()]);
    assert!(a.registry.snapshot().disabled.contains("p"));
    assert!(b.registry.snapshot().disabled.is_empty());
}

#[test]
fn device_id_falls_back_to_index_without_serial() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let manifests = vec![manifest()];
    set_registry(&app.registry, manifests.clone());
    let dev = app
        .registry
        .match_in(&app, &manifests, &hid(0x1234, 0x5678, None, 3))
        .expect("matches");
    assert_eq!(dev.id(), "acme_k1-3");
}

#[test]
fn activation_status_reports_disabled_needs_consent_and_ready() {
    // The single activation gate distinguishes the three states, and only yields a
    // `ReadyPlugin` once the plugin is enabled, granted, and content-acknowledged.
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let secrets = FakeSecretStore::default();
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0xCCCC, pid = 0xDDDD, vendor = "Acme", model = "K2" } },
          permissions = { "network" },
        }
    "#;
    let m = parse_manifest(src, Path::new("needs_network.lua")).unwrap();
    app.registry.update(|s| s.manifests = vec![m.clone()]);

    assert!(matches!(
        app.registry.activation_status(&secrets, &m),
        ActivationState::Discovered
    ));

    app.registry.set_disabled(&["needs_network".to_string()]);
    assert!(matches!(
        app.registry.activation_status(&secrets, &m),
        ActivationState::Disabled
    ));

    app.registry.set_disabled(&[]);
    app.registry.set_granted(&HashMap::new());
    assert!(matches!(
        app.registry.activation_status(&secrets, &m),
        ActivationState::AwaitingConsent
    ));

    app.registry.set_granted(&HashMap::from([(
        "needs_network".to_string(),
        vec![Permission::Network],
    )]));
    acknowledge(&app.registry, std::slice::from_ref(&m));
    assert!(matches!(
        app.registry.activation_status(&secrets, &m),
        ActivationState::Ready(_)
    ));

    app.registry.set_granted(&HashMap::new());
    app.registry.set_acknowledged(&HashMap::new());
}

#[test]
fn smbus_scan_entries_require_ready_activation() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { {
            transport = "smbus", bus = "chipset", addresses = { 0x50 },
            vendor = "Acme", model = "RAM", pre_scan = true,
          } },
          permissions = { "smbus" },
        }
    "#;
    let m = parse_manifest(src, Path::new("smbus_scan.lua")).unwrap();
    set_registry(&app.registry, vec![m.clone()]);

    app.registry.set_granted(&HashMap::new());
    assert!(app.registry.plugin_smbus_scan_entries().is_empty());

    app.registry
        .set_disabled(std::slice::from_ref(&m.plugin_id));
    assert!(app.registry.plugin_smbus_scan_entries().is_empty());

    app.registry.set_disabled(&[]);
    app.registry.set_granted(&HashMap::from([(
        m.plugin_id.clone(),
        vec![Permission::Smbus],
    )]));
    acknowledge(&app.registry, std::slice::from_ref(&m));
    let entries = app.registry.plugin_smbus_scan_entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].plugin_id, m.plugin_id);
    assert!(entries[0].pre_scan);
}

#[test]
fn granted_for_intersects_stored_and_declared_permissions() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { { transport = "smbus", bus = "chipset", addresses = { 0x50 }, vendor = "Acme", model = "RAM" } },
          permissions = { "smbus" },
        }
    "#;
    let m = parse_manifest(src, Path::new("declared_smbus.lua")).unwrap();
    set_registry(&app.registry, vec![m.clone()]);
    app.registry.set_granted(&HashMap::from([(
        m.plugin_id.clone(),
        vec![Permission::Smbus, Permission::Os],
    )]));

    assert_eq!(
        app.registry.granted_for(&m.plugin_id),
        vec![Permission::Smbus]
    );
    assert!(app.registry.granted_for("unknown-plugin").is_empty());
}

#[test]
fn granted_permission_is_pinned_to_script_content() {
    // A permissioned plugin activates only while its granted content pin
    // matches the current script; editing the script revokes consent.
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0xCCCC, pid = 0xDDDD, vendor = "Acme", model = "K2" } },
          permissions = { "network" },
        }
    "#;
    let manifests = vec![parse_manifest(src, Path::new("needs_network.lua")).unwrap()];
    set_registry(&app.registry, manifests.clone());
    let handle = hid(0xCCCC, 0xDDDD, Some("S"), 0);
    app.registry.set_disabled(&[]);
    app.registry.set_granted(&HashMap::from([(
        "needs_network".to_string(),
        vec![Permission::Network],
    )]));

    // Granted but not pinned to content → inert.
    app.registry.set_acknowledged(&HashMap::new());
    assert!(
        app.registry.match_in(&app, &manifests, &handle).is_none(),
        "a grant with no content pin must not activate"
    );

    // Pinned to the current content → active.
    acknowledge(&app.registry, &manifests);
    assert!(
        app.registry.match_in(&app, &manifests, &handle).is_some(),
        "grant pinned to the current script activates"
    );

    // Content changed (stale pin) → inert again.
    app.registry.set_acknowledged(&HashMap::from([(
        "needs_network".to_string(),
        "deadbeef".to_string(),
    )]));
    assert!(
        app.registry.match_in(&app, &manifests, &handle).is_none(),
        "a since-modified script must revert to needing consent"
    );

    app.registry.set_granted(&HashMap::new());
    app.registry.set_acknowledged(&HashMap::new());
}

#[test]
fn non_matching_handle_returns_none() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let manifests = vec![manifest()];
    assert!(app
        .registry
        .match_in(&app, &manifests, &hid(0x9999, 0x0000, None, 0))
        .is_none());
}

#[test]
fn disabled_plugin_does_not_match() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0xAAAA, pid = 0xBBBB, vendor = "Acme", model = "K1" } },
        }
    "#;
    let manifests = vec![parse_manifest(src, Path::new("disabled_only_plugin.lua")).unwrap()];
    set_registry(&app.registry, manifests.clone());
    let handle = hid(0xAAAA, 0xBBBB, Some("S"), 0);
    app.registry
        .set_disabled(&["disabled_only_plugin".to_string()]);
    assert!(
        app.registry.match_in(&app, &manifests, &handle).is_none(),
        "disabled plugin must not shadow native"
    );
    app.registry.set_disabled(&[]);
    assert!(app.registry.match_in(&app, &manifests, &handle).is_some());
}

#[test]
fn plugin_with_ungranted_permission_does_not_match() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0xCCCC, pid = 0xDDDD, vendor = "Acme", model = "K2" } },
          permissions = { "network" },
        }
    "#;
    let manifests = vec![parse_manifest(src, Path::new("needs_network.lua")).unwrap()];
    set_registry(&app.registry, manifests.clone());
    let handle = hid(0xCCCC, 0xDDDD, Some("S"), 0);
    app.registry.set_disabled(&[]);
    acknowledge(&app.registry, &manifests);
    app.registry.set_granted(&HashMap::new());
    assert!(
        app.registry.match_in(&app, &manifests, &handle).is_none(),
        "declared-but-ungranted permission must keep the plugin inert"
    );

    let mut granted = HashMap::new();
    granted.insert("needs_network".to_string(), vec![Permission::Network]);
    app.registry.set_granted(&granted);
    assert!(
        app.registry.match_in(&app, &manifests, &handle).is_some(),
        "fully granted plugin activates"
    );
    app.registry.set_granted(&HashMap::new());
    app.registry.set_acknowledged(&HashMap::new());
}

#[test]
fn consent_satisfied_true_when_no_permissions_declared() {
    // A plugin that declares no permissions runs freely — it can only talk
    // to its matched device, the base trust every plugin has.
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        }
    "#;
    let m = parse_manifest(src, Path::new("no_perms.lua")).unwrap();
    app.registry.set_acknowledged(&HashMap::new());
    assert!(app.registry.consent_satisfied(&m));
}

#[test]
fn load_all_with_repos_discovers_a_repo_sourced_plugin_and_tags_its_source() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let _cfg_guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let config_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HALOD_CONFIG_DIR", config_dir.path()) };

    let local_dir = tempfile::tempdir().unwrap();
    let repos_root = crate::config::plugin_repos_dir();
    let repo_dir = repos_root.join("acme-repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(
        repo_dir.join("plugin.yaml"),
        "id: acme-repo\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
    )
    .unwrap();
    std::fs::write(repo_dir.join("main.lua"), "return {}").unwrap();

    app.registry
        .load_all_with_repos(local_dir.path(), std::slice::from_ref(&repo_dir));

    let secrets = FakeSecretStore::default();
    let infos = app.registry.list(&secrets);
    let p = infos
        .iter()
        .find(|p| p.id == "acme-repo")
        .expect("repo-sourced plugin discovered");
    assert_eq!(
        p.source,
        halod_shared::types::PluginSource::Repo {
            slug: "acme-repo".to_string()
        }
    );

    app.registry.load_all(Path::new("/nonexistent"));
    unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
}

#[test]
fn load_all_with_repos_discovers_sibling_packages_at_a_repos_root() {
    // The official repo's real layout: several packages as immediate sibling
    // directories under the repo root (no `plugins/` nesting, no
    // single-package-at-root manifest).
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let _cfg_guard = crate::test_support::HALOD_CONFIG_DIR_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let config_dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HALOD_CONFIG_DIR", config_dir.path()) };

    let local_dir = tempfile::tempdir().unwrap();
    let repos_root = crate::config::plugin_repos_dir();
    let repo_dir = repos_root.join("sibling-repo");
    let write_pkg = |id: &str| {
        let dir = repo_dir.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: {id}\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
    };
    write_pkg("pkg_a");
    write_pkg("pkg_b");
    // A README alongside them must not be mistaken for a package.
    std::fs::write(repo_dir.join("README.md"), "not a plugin").unwrap();

    app.registry
        .load_all_with_repos(local_dir.path(), std::slice::from_ref(&repo_dir));

    let secrets = FakeSecretStore::default();
    let infos = app.registry.list(&secrets);
    for id in ["pkg_a", "pkg_b"] {
        assert!(
            infos.iter().any(|p| p.id == id),
            "sibling package '{id}' at the repo root must be discovered"
        );
    }

    app.registry.load_all(Path::new("/nonexistent"));
    unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
}

#[test]
fn load_all_never_runs_a_dropped_in_scripts_side_effects() {
    // A malicious file dropped into the plugins dir tries to write a
    // sentinel at top level. `load_all` must treat the entry as inert source:
    // it neither compiles nor executes it before consent.
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("pwned.txt");
    let evil = format!(
        r#"local f = io.open([[{}]], "w"); f:write("x"); f:close()
           return {{}}"#,
        sentinel.display()
    );
    let plugin_dir = dir.path().join("evil");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("plugin.yaml"),
        "id: evil\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("main.lua"), evil).unwrap();

    app.registry.load_all(dir.path());

    assert!(
        !sentinel.exists(),
        "a dropped-in script's filesystem write must never execute at load time"
    );
    assert!(
        app.registry
            .snapshot()
            .manifests
            .iter()
            .any(|m| m.plugin_id == "evil"),
        "valid YAML must register without evaluating its entry Lua"
    );
    app.registry.load_all(Path::new("/nonexistent"));
}

#[test]
fn ungranted_in_reports_once_then_stays_silent() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
          identity = { name = "Needs Net" },
          permissions = { "network" },
        }
    "#;
    let m = parse_manifest(src, Path::new("needs_net2.lua")).unwrap();
    let manifests = vec![m];
    let mut notified = HashSet::new();

    let first = app.registry.ungranted_in(&manifests, &mut notified);
    assert_eq!(
        first,
        vec![("Needs Net".to_string(), UngrantedReason::NeedsPermission)]
    );

    // Same manifest, same notified set: already announced, not repeated.
    let second = app.registry.ungranted_in(&manifests, &mut notified);
    assert!(
        second.is_empty(),
        "must not repeat an already-notified plugin"
    );
}

#[test]
fn ungranted_in_flags_content_change_when_acknowledgment_is_stale() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
          identity = { name = "Approved Once" },
          permissions = { "network" },
        }
    "#;
    let m = parse_manifest(src, Path::new("approved_once.lua")).unwrap();
    // Grant the permission but pin acknowledgment to a hash that no longer
    // matches the manifest — i.e. its content changed since approval.
    app.registry.set_granted(&HashMap::from([(
        m.plugin_id.clone(),
        vec![Permission::Network],
    )]));
    app.registry.set_acknowledged(&HashMap::from([(
        m.plugin_id.clone(),
        "stale-hash".to_string(),
    )]));
    let mut notified = HashSet::new();

    let out = app.registry.ungranted_in(&[m], &mut notified);
    assert_eq!(
        out,
        vec![("Approved Once".to_string(), UngrantedReason::ContentChanged)]
    );

    app.registry.set_granted(&HashMap::new());
    app.registry.set_acknowledged(&HashMap::new());
}

#[test]
fn ungranted_in_skips_satisfied_manifests() {
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        }
    "#;
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let m = parse_manifest(src, Path::new("no_perms2.lua")).unwrap();
    let mut notified = HashSet::new();
    assert!(app.registry.ungranted_in(&[m], &mut notified).is_empty());
}

#[test]
fn official_repo_plugin_cannot_be_shadowed_by_a_later_source() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    let local_dir = tmp.path().join("plugins");
    let official_dir = tmp.path().join("official");
    std::fs::create_dir_all(&local_dir).unwrap();

    let write_dup = |dir: &Path, vendor: &str| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: dup\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ndevices:\n  - vendor: {vendor}\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
    };
    // A repo-root plugin's id must equal its directory name (the slug), so
    // the official repo provides "dup" from a `plugins/<id>/` subdir instead.
    write_dup(&official_dir.join("plugins").join("dup"), "Official");
    write_dup(&local_dir.join("dup"), "Community");

    app.registry.take_plugin_load_warnings(); // drain any stale state from a prior test
    app.registry
        .load_all_with_repos(&local_dir, std::slice::from_ref(&official_dir));

    let state = app.registry.snapshot();
    let dup: Vec<_> = state
        .manifests
        .iter()
        .filter(|m| m.plugin_id == "dup")
        .collect();
    assert_eq!(
        dup.len(),
        1,
        "the community copy must not join the official one"
    );
    assert_eq!(
        dup[0].devices[0].vendor, "Official",
        "the official repo loads first and owns the id"
    );
    drop(state);

    let warnings = app.registry.take_plugin_load_warnings();
    assert!(
        warnings.iter().any(|w| w.plugin_id == "dup"),
        "the shadow attempt must be surfaced, not silently dropped"
    );

    // The warning is also persisted as a per-plugin issue for the GUI page.
    let dup_issue = app
        .registry
        .list(app.secret_store.as_ref())
        .into_iter()
        .find(|p| p.id == "dup")
        .and_then(|p| p.issue)
        .expect("load warning surfaced as a plugin issue");
    assert_eq!(
        dup_issue.kind,
        halod_shared::types::PluginIssueKind::LoadWarning
    );

    app.registry.load_all(Path::new("/nonexistent"));
}

#[tokio::test]
async fn connect_error_toasts_once_per_episode_and_persists_issue() {
    use tokio::sync::mpsc;
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    set_registry(&app.registry, vec![manifest()]);
    let pid = manifest().plugin_id;

    let (tx, mut rx) = mpsc::channel::<Arc<Vec<u8>>>(16);
    app.clients.lock().await.push(crate::ipc::ClientHandle {
        id: 0,
        tx,
        subs: Arc::default(),
    });

    app.registry
        .report_connect_error(&app, &pid, "Acme K1", "127.0.0.1 blocked".into())
        .await;
    assert!(rx.try_recv().is_ok(), "first failure toasts");
    let info = app
        .registry
        .list(app.secret_store.as_ref())
        .into_iter()
        .find(|p| p.id == pid)
        .expect("plugin remains listed");
    assert!(
        info.issue.is_none(),
        "an integration connection failure is not a plugin package issue"
    );
    let issue = info.integration_issue.expect("integration issue persisted");
    assert_eq!(issue.kind, PluginIssueKind::ConnectFailed);

    // Same episode: deduped, no second toast.
    app.registry
        .report_connect_error(&app, &pid, "Acme K1", "still down".into())
        .await;
    assert!(rx.try_recv().is_err(), "same episode does not re-toast");

    // Recovery clears the dedup + issue; a later failure re-arms.
    app.registry.clear_operational_errors(&pid, &[]);
    assert!(app
        .registry
        .list(app.secret_store.as_ref())
        .into_iter()
        .find(|p| p.id == pid)
        .unwrap()
        .integration_issue
        .is_none());
    app.registry
        .report_connect_error(&app, &pid, "Acme K1", "down again".into())
        .await;
    assert!(rx.try_recv().is_ok(), "re-arms after recovery");

    // A connect attempt that completes after the integration was disabled is
    // stale: it must not recreate the cleared issue or toast.
    app.registry.clear_operational_errors(&pid, &[]);
    app.registry
        .set_integrations_disabled(std::slice::from_ref(&pid));
    app.registry
        .report_connect_error(&app, &pid, "Acme K1", "late failure".into())
        .await;
    assert!(rx.try_recv().is_err(), "late disabled failure is ignored");
    assert!(app
        .registry
        .list(app.secret_store.as_ref())
        .into_iter()
        .find(|p| p.id == pid)
        .unwrap()
        .integration_issue
        .is_none());
}

#[tokio::test]
async fn runtime_error_persists_issue_and_clears_on_success() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    set_registry(&app.registry, vec![manifest()]);
    let pid = manifest().plugin_id;

    app.registry
        .report_runtime_error(&app, &pid, "dev-1", "lua boom".into())
        .await;
    let issue = app
        .registry
        .list(app.secret_store.as_ref())
        .into_iter()
        .find(|p| p.id == pid)
        .and_then(|p| p.issue)
        .expect("runtime issue persisted");
    assert_eq!(issue.kind, PluginIssueKind::RuntimeError);

    app.registry
        .clear_operational_errors(&pid, &["dev-1".to_owned()]);
    assert!(app
        .registry
        .list(app.secret_store.as_ref())
        .into_iter()
        .find(|p| p.id == pid)
        .unwrap()
        .issue
        .is_none());

    // Likewise, a callback already running when the plugin was disabled must
    // not recreate its runtime issue.
    app.registry.set_disabled(std::slice::from_ref(&pid));
    app.registry
        .report_runtime_error(&app, &pid, "dev-1", "late lua boom".into())
        .await;
    assert!(app
        .registry
        .list(app.secret_store.as_ref())
        .into_iter()
        .find(|p| p.id == pid)
        .unwrap()
        .issue
        .is_none());
}

#[test]
fn invalid_plugin_lists_as_load_failed_and_malformed_is_skipped() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    let plugins_dir = tmp.path().join("plugins");

    let invalid = plugins_dir.join("tcpbad");
    std::fs::create_dir_all(&invalid).unwrap();
    let invalid_yaml =
        "id: tcpbad\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ntype: integration\ntransports:\n  tcp:\n    host_key: host\n    port_key: port\n";
    std::fs::write(invalid.join("plugin.yaml"), invalid_yaml).unwrap();
    std::fs::write(invalid.join("main.lua"), "return {}").unwrap();

    let broken = plugins_dir.join("broken");
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("plugin.yaml"), "a: b: c\n").unwrap();

    app.registry.load_all(&plugins_dir);

    let list = app.registry.list(app.secret_store.as_ref());
    let failed = list
        .iter()
        .find(|p| p.id == "tcpbad")
        .expect("invalid plugin is listed");
    assert!(!failed.enabled, "invalid plugin can't be enabled");
    assert_eq!(
        failed.issue.as_ref().map(|i| &i.kind),
        Some(&PluginIssueKind::LoadFailed)
    );

    assert!(
        list.iter().all(|p| p.id != "broken"),
        "malformed plugin is not listed as an entry"
    );
    let skipped = app.registry.skipped();
    assert!(
        skipped.iter().any(|s| s.path.contains("broken")),
        "malformed plugin is surfaced as skipped: {skipped:?}"
    );

    let fixed_yaml = "id: tcpbad\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ntype: integration\npermissions: [network]\ntransports:\n  tcp:\n    host_key: host\n    port_key: port\n";
    std::fs::write(invalid.join("plugin.yaml"), fixed_yaml).unwrap();
    app.registry.load_all(&plugins_dir);
    let fixed = app.registry.list(app.secret_store.as_ref());
    let fixed = fixed
        .iter()
        .find(|p| p.id == "tcpbad")
        .expect("still listed after fix");
    assert!(
        fixed.issue.is_none(),
        "fixing the manifest clears the issue"
    );
}

#[test]
fn permission_gate_is_inert_until_granted_then_satisfied_once_acknowledged() {
    // Demonstrates the consent gate uniformly: declared-but-ungranted is
    // unsatisfied; fully granted + acknowledged is satisfied. This applies
    // to every plugin now — no source is exempt.
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        sensor = {},
        permissions = { "os" },
    }"#;
    let m = parse_manifest(src, Path::new("permission_demo.lua")).unwrap();
    assert_eq!(m.permissions, vec![Permission::Os]);
    assert!(m.needs_worker());

    // Acknowledged, so this isolates the permission gate (not the consent gate).
    acknowledge(&app.registry, std::slice::from_ref(&m));
    app.registry.set_granted(&HashMap::new());
    assert!(
        !app.registry.consent_satisfied(&m),
        "ungranted os permission keeps it inert"
    );

    let mut granted = HashMap::new();
    granted.insert("permission_demo".to_string(), vec![Permission::Os]);
    app.registry.set_granted(&granted);
    assert!(app.registry.consent_satisfied(&m));
    app.registry.set_granted(&HashMap::new());
    app.registry.set_acknowledged(&HashMap::new());
}

#[test]
fn list_reports_enabled_only_once_permissions_are_granted() {
    // Invariant: a plugin can never be reported enabled while its permissions
    // are ungranted — enabling and granting are one atomic step, so an
    // un-consented plugin is never "on".
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        permissions = { "os" },
    }"#;
    let m = parse_manifest(src, Path::new("perm_plugin.lua")).unwrap();
    set_registry(&app.registry, vec![m.clone()]);
    app.registry.set_disabled(&[]);
    app.registry.set_granted(&HashMap::new());
    // Acknowledged so only the permission grant — not the content gate —
    // decides consent here.
    acknowledge(&app.registry, std::slice::from_ref(&m));

    let secrets = FakeSecretStore::default();
    let enabled_of = |secrets: &FakeSecretStore| {
        app.registry
            .list(secrets)
            .into_iter()
            .find(|p| p.id == "perm_plugin")
            .map(|p| (p.enabled, p.consented))
            .unwrap()
    };

    assert_eq!(
        enabled_of(&secrets),
        (false, false),
        "ungranted permissioned plugin must not report enabled"
    );

    let granted = HashMap::from([("perm_plugin".to_string(), vec![Permission::Os])]);
    app.registry.set_granted(&granted);
    assert_eq!(
        enabled_of(&secrets),
        (true, true),
        "granting its permission enables it"
    );

    // An explicitly disabled plugin stays off even when fully granted.
    app.registry.set_disabled(&["perm_plugin".to_string()]);
    assert!(!enabled_of(&secrets).0);

    app.registry.set_disabled(&[]);
    app.registry.set_granted(&HashMap::new());
    app.registry.set_acknowledged(&HashMap::new());
    set_registry(&app.registry, Vec::new());
}

#[test]
fn effect_entries_for_namespaces_ids_and_carries_kind() {
    let src = r#"return {
        type = "effect",
        identity = { name = "Effects" },
        effects = {
          { kind = "pixmap", id = "plasma", name = "Plasma" },
          { kind = "direct", id = "comet", name = "Comet" },
        },
    }"#;
    let m = parse_manifest(src, Path::new("fx.lua")).unwrap();
    let entries = effect_entries_for(&m);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].catalog_id, "fx:plasma");
    assert_eq!(entries[0].kind, EffectKind::Pixmap);
    assert_eq!(entries[0].descriptor.id, "fx:plasma");
    assert_eq!(entries[1].catalog_id, "fx:comet");
    assert_eq!(entries[1].kind, EffectKind::Direct);
}

#[test]
fn effect_entries_for_empty_for_a_device_only_plugin() {
    assert!(effect_entries_for(&manifest()).is_empty());
}

#[test]
fn logo_rejection_blocks_traversal_names_before_any_read() {
    // A `logo:` that escapes the assets dir is rejected on the name alone, so it
    // never reads (and leaks the size/existence of) an arbitrary root-readable file.
    let dir = Path::new("/nonexistent/plugin/dir");
    assert!(super::super::logo_rejection(dir, "../../../../etc/shadow").is_some());
    assert!(super::super::logo_rejection(dir, "/etc/shadow").is_some());
    assert!(super::super::logo_rejection(dir, "a/b.png").is_some());
    // A well-formed name whose file is simply absent is not a rejection.
    assert!(super::super::logo_rejection(dir, "logo.png").is_none());
}

#[test]
fn effect_activation_honours_the_consent_gate() {
    // An effect plugin with a permission must clear the same consent gate as a
    // device: granted *and* content-acknowledged. Editing the script (a stale
    // pin) hides its effects until re-acknowledged — matching the device path,
    // so effects can't spawn new code under an old grant.
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"return {
        type = "effect",
        identity = { name = "FX" },
        permissions = { "network" },
        effects = { { kind = "pixmap", id = "plasma", name = "Plasma" } },
    }"#;
    let manifests = vec![parse_manifest(src, Path::new("fx.lua")).unwrap()];
    set_registry(&app.registry, manifests.clone());
    app.registry.set_disabled(&[]);
    app.registry.set_granted(&HashMap::from([(
        "fx".to_string(),
        vec![Permission::Network],
    )]));

    // Granted but not acknowledged → effect hidden.
    app.registry.set_acknowledged(&HashMap::new());
    assert!(app.registry.effect_entry("fx:plasma").is_none());
    assert!(app.registry.pixmap_effect_descriptors().is_empty());

    // Acknowledged current content → visible.
    acknowledge(&app.registry, &manifests);
    assert!(app.registry.effect_entry("fx:plasma").is_some());
    assert_eq!(app.registry.pixmap_effect_descriptors().len(), 1);

    // Since-edited script (stale pin) → hidden again.
    app.registry
        .set_acknowledged(&HashMap::from([("fx".to_string(), "deadbeef".to_string())]));
    assert!(app.registry.effect_entry("fx:plasma").is_none());

    set_registry(&app.registry, Vec::new());
}

#[test]
fn capability_labels_reflect_manifest_sections() {
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 1, pid = 2, vendor = "V", model = "M" } },
          rgb = { zones = {} },
          sensor = {},
        }
    "#;
    let m = parse_manifest(src, Path::new("caps.lua")).unwrap();
    assert_eq!(m.capability_labels(), vec!["RGB", "Sensor"]);
}

/// An in-memory `SecretStore` for tests, so `list()` tests don't need a
/// real keyring or the encrypted-file backend.
#[derive(Default)]
struct FakeSecretStore {
    values: std::sync::Mutex<HashMap<(String, String), String>>,
}

impl crate::secrets::SecretStore for FakeSecretStore {
    fn set(&self, plugin_id: &str, key: &str, plaintext: &str) -> anyhow::Result<()> {
        self.values
            .lock()
            .unwrap()
            .insert((plugin_id.to_owned(), key.to_owned()), plaintext.to_owned());
        Ok(())
    }
    fn get(&self, plugin_id: &str, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .values
            .lock()
            .unwrap()
            .get(&(plugin_id.to_owned(), key.to_owned()))
            .cloned())
    }
    fn delete(&self, plugin_id: &str, key: &str) -> anyhow::Result<()> {
        self.values
            .lock()
            .unwrap()
            .remove(&(plugin_id.to_owned(), key.to_owned()));
        Ok(())
    }
}

#[test]
fn config_for_defaults_unset_fields_and_overrides_set_ones() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        config = { fields = {
          { key = "host", label = "Host", default = "127.0.0.1" },
          { key = "port", label = "Port", default = "6742" },
          { key = "token", label = "Token", secure = true, default = "unused" },
        } },
    }"#;
    set_registry(
        &app.registry,
        vec![parse_manifest(src, Path::new("cfgfor.lua")).unwrap()],
    );
    let mut stored = HashMap::new();
    stored.insert(
        "cfgfor".to_string(),
        HashMap::from([("port".to_string(), "9999".to_string())]),
    );
    app.registry.set_config_values(&stored);

    let resolved = app.registry.config_for("cfgfor");
    assert_eq!(resolved.get("host"), Some(&"127.0.0.1".to_string()));
    assert_eq!(resolved.get("port"), Some(&"9999".to_string()));
    assert!(
        !resolved.contains_key("token"),
        "secure fields must never appear in the non-secure config map"
    );

    app.registry.set_config_values(&HashMap::new());
    set_registry(&app.registry, Vec::new());
}

#[test]
fn config_for_unknown_plugin_is_empty() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    assert!(app.registry.config_for("does-not-exist").is_empty());
}

#[test]
fn integration_manifests_filters_by_type_disabled_and_permissions() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let integ_src = r#"return {
        type = "integration",
    }"#;
    let device_src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
    }"#;
    let needs_perm_src = r#"return {
        type = "integration",
        permissions = { "network" },
    }"#;
    set_registry(
        &app.registry,
        vec![
            parse_manifest(integ_src, Path::new("integ_ok.lua")).unwrap(),
            parse_manifest(device_src, Path::new("device_only.lua")).unwrap(),
            parse_manifest(needs_perm_src, Path::new("integ_needs_perm.lua")).unwrap(),
        ],
    );
    app.registry.set_disabled(&[]);
    app.registry.set_granted(&HashMap::new());

    let ids: Vec<String> = app
        .registry
        .integration_manifests()
        .into_iter()
        .map(|m| m.plugin_id)
        .collect();
    assert_eq!(ids, vec!["integ_ok"]);

    app.registry.set_disabled(&["integ_ok".to_string()]);
    assert!(app.registry.integration_manifests().is_empty());
    app.registry.set_disabled(&[]);

    // `integrations_disabled` is a second, independent gate: it must
    // exclude the integration even though the plugin itself is enabled.
    app.registry
        .set_integrations_disabled(&["integ_ok".to_string()]);
    assert!(app.registry.integration_manifests().is_empty());
    assert!(app.registry.integration_manifest("integ_ok").is_none());
    app.registry.set_integrations_disabled(&[]);

    assert_eq!(
        app.registry
            .integration_manifest("integ_ok")
            .map(|m| m.plugin_id),
        Some("integ_ok".to_string())
    );

    set_registry(&app.registry, Vec::new());
}

#[test]
fn secure_config_keys_for_returns_declared_secure_keys() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        config = { fields = {
          { key = "host", label = "Host" },
          { key = "token", label = "Token", secure = true },
        } },
    }"#;
    set_registry(
        &app.registry,
        vec![parse_manifest(src, Path::new("securekeys.lua")).unwrap()],
    );

    assert_eq!(
        app.registry.secure_config_keys_for("securekeys"),
        vec!["token"]
    );
    assert!(app
        .registry
        .secure_config_keys_for("does-not-exist")
        .is_empty());

    set_registry(&app.registry, Vec::new());
}

#[test]
fn list_reports_config_fields_values_and_secret_set() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        config = { fields = {
          { key = "host", label = "Host", default = "127.0.0.1" },
          { key = "token", label = "Token", secure = true },
        } },
    }"#;
    set_registry(
        &app.registry,
        vec![parse_manifest(src, Path::new("listcfg.lua")).unwrap()],
    );

    let secrets = FakeSecretStore::default();
    secrets.set("listcfg", "token", "s3cr3t").unwrap();

    let infos = app.registry.list(&secrets);
    let info = infos.iter().find(|p| p.id == "listcfg").expect("present");
    assert_eq!(info.config_fields.len(), 2);
    assert_eq!(
        info.config_values.get("host"),
        Some(&"127.0.0.1".to_string())
    );
    assert!(
        !info.config_values.contains_key("token"),
        "secret value must never appear in config_values"
    );
    assert_eq!(info.secret_set.get("token"), Some(&true));

    set_registry(&app.registry, Vec::new());
}

// ── Assets ────────────────────────────────────────────────────────

fn write_asset_plugin(root: &Path, id: &str, logo_bytes: Option<&[u8]>) -> std::path::PathBuf {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.yaml"),
        format!(
            "id: {id}\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n\
             logo: logo.png\neffects:\n  - kind: pixmap\n    id: rainbow\n    name: Rainbow\n\
             effect_assets:\n  - id: rainbow\n    thumbnail: rainbow.png\n"
        ),
    )
    .unwrap();
    std::fs::write(dir.join("main.lua"), "return {}").unwrap();
    if let Some(bytes) = logo_bytes {
        let assets = dir.join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(assets.join("logo.png"), bytes).unwrap();
    }
    dir
}

#[test]
fn list_surfaces_declared_logo_and_effect_thumbnails() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "assetplug", None);
    app.registry.load_all(tmp.path());

    let secrets = FakeSecretStore::default();
    let infos = app.registry.list(&secrets);
    let info = infos.iter().find(|p| p.id == "assetplug").expect("present");
    assert_eq!(info.logo.as_deref(), Some("logo.png"));
    assert_eq!(info.effect_thumbnails.len(), 1);
    assert_eq!(info.effect_thumbnails[0].id, "rainbow");
    assert_eq!(info.effect_thumbnails[0].thumbnail, "rainbow.png");
    assert_eq!(info.source, halod_shared::types::PluginSource::Local);

    app.registry.load_all(Path::new("/nonexistent"));
}

#[test]
fn list_leaves_logo_and_thumbnails_empty_when_undeclared() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("noassets");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.yaml"),
        "id: noassets\ncompatibility:\n  halod: '>=0.2.0'\n  plugin_api: 1\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.lua"), "return {}").unwrap();
    app.registry.load_all(tmp.path());

    let secrets = FakeSecretStore::default();
    let infos = app.registry.list(&secrets);
    let info = infos.iter().find(|p| p.id == "noassets").expect("present");
    assert!(info.logo.is_none());
    assert!(info.effect_thumbnails.is_empty());

    app.registry.load_all(Path::new("/nonexistent"));
}

#[test]
fn read_asset_returns_bytes_for_a_declared_logo() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    let png = png_of(32, 32);
    write_asset_plugin(tmp.path(), "readable", Some(&png));
    app.registry.load_all(tmp.path());

    let bytes = app.registry.read_asset("readable", "logo.png").unwrap();
    assert_eq!(bytes, png);

    app.registry.load_all(Path::new("/nonexistent"));
}

#[test]
fn read_asset_rejects_missing_file_and_unknown_plugin() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "nofile", None);
    app.registry.load_all(tmp.path());

    assert!(
        app.registry.read_asset("nofile", "logo.png").is_err(),
        "declared asset with no file on disk must error"
    );
    assert!(
        app.registry
            .read_asset("does-not-exist", "logo.png")
            .is_err(),
        "unknown plugin id must error"
    );

    app.registry.load_all(Path::new("/nonexistent"));
}

/// A PNG of `w`×`h` opaque pixels, for exercising the logo dimension/aspect bounds.
fn png_of(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

#[test]
fn load_keeps_valid_square_logo() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "goodlogo", Some(&png_of(64, 64)));
    app.registry.load_all(tmp.path());

    let info = app.registry.list(&FakeSecretStore::default());
    let p = info.iter().find(|p| p.id == "goodlogo").expect("present");
    assert_eq!(p.logo.as_deref(), Some("logo.png"));
    assert!(app.registry.take_plugin_load_warnings().is_empty());

    app.registry.load_all(Path::new("/nonexistent"));
}

#[test]
fn load_drops_oversized_and_banner_logos() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    for bad in [
        png_of(MAX_LOGO_DIM_PLUS, MAX_LOGO_DIM_PLUS),
        png_of(500, 50),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        write_asset_plugin(tmp.path(), "badlogo", Some(&bad));
        app.registry.load_all(tmp.path());

        let info = app.registry.list(&FakeSecretStore::default());
        let p = info.iter().find(|p| p.id == "badlogo").expect("present");
        assert!(p.logo.is_none(), "an out-of-bounds logo must be dropped");
        let warnings = app.registry.take_plugin_load_warnings();
        assert!(
            warnings.iter().any(|w| w.plugin_id == "badlogo"),
            "dropping a logo must surface a load warning"
        );
    }
    app.registry.load_all(Path::new("/nonexistent"));
}

const MAX_LOGO_DIM_PLUS: u32 = halod_shared::types::MAX_PLUGIN_LOGO_DIM + 1;

#[test]
fn read_asset_rejects_oversized_file() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    let huge = vec![0u8; (halod_shared::types::MAX_PLUGIN_ASSET_BYTES + 1) as usize];
    write_asset_plugin(tmp.path(), "huge", Some(&huge));
    app.registry.load_all(tmp.path());

    assert!(
        app.registry.read_asset("huge", "logo.png").is_err(),
        "an asset over the byte limit must be refused"
    );

    app.registry.load_all(Path::new("/nonexistent"));
}

#[test]
fn read_asset_rejects_path_traversal_and_bad_extensions() {
    let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "traversal", Some(b"PNGDATA"));
    app.registry.load_all(tmp.path());

    for bad in [
        "../plugin.yaml",
        "../../etc/passwd",
        "logo",
        "logo.txt",
        "/etc/passwd",
    ] {
        assert!(
            app.registry.read_asset("traversal", bad).is_err(),
            "'{bad}' must be rejected"
        );
    }

    app.registry.load_all(Path::new("/nonexistent"));
}
