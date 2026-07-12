// SPDX-License-Identifier: GPL-3.0-or-later
//! Tests for the plugin registry itself (matching, consent, config
//! resolution, discovery notifications) — as opposed to the
//! per-plugin equivalence tests in the sibling modules.

use super::super::*;
use crate::secrets::SecretStore as _;
use std::path::Path;

use super::super::TEST_GLOBALS_LOCK as GLOBALS_LOCK;

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
/// entries) for a test — the snapshot equivalent of the old direct
/// `*PLUGIN_REGISTRY.write()`. Callers hold `GLOBALS_LOCK`.
fn set_registry(manifests: Vec<PluginManifest>) {
    let effects: Vec<PluginEffectEntry> = manifests.iter().flat_map(effect_entries_for).collect();
    update(|s| {
        s.manifests = manifests;
        s.effects = effects;
    });
}

/// Acknowledge every manifest's current content, as user consent would, so
/// `consent_satisfied` treats them as consented. Callers hold `GLOBALS_LOCK`.
fn acknowledge(manifests: &[PluginManifest]) {
    let map = manifests
        .iter()
        .map(|m| (m.plugin_id.clone(), m.content_hash()))
        .collect();
    set_acknowledged(&map);
}

#[test]
fn matching_handle_builds_device_with_identity() {
    let manifests = vec![manifest()];
    let dev = match_in(&manifests, &hid(0x1234, 0x5678, Some("SER"), 0)).expect("matches");
    assert_eq!(dev.vendor(), "Acme");
    assert_eq!(dev.name(), "Acme K1");
    assert_eq!(dev.id(), "acme_k1-SER");
}

#[test]
fn device_id_falls_back_to_index_without_serial() {
    let manifests = vec![manifest()];
    let dev = match_in(&manifests, &hid(0x1234, 0x5678, None, 3)).expect("matches");
    assert_eq!(dev.id(), "acme_k1-3");
}

#[test]
fn granted_permission_is_pinned_to_script_content() {
    // A permissioned plugin activates only while its granted content pin
    // matches the current script; editing the script revokes consent.
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0xCCCC, pid = 0xDDDD, vendor = "Acme", model = "K2" } },
          permissions = { "network" },
        }
    "#;
    let manifests = vec![parse_manifest(src, Path::new("needs_network.lua")).unwrap()];
    let handle = hid(0xCCCC, 0xDDDD, Some("S"), 0);
    set_disabled(&[]);
    set_granted(&HashMap::from([(
        "needs_network".to_string(),
        vec![Permission::Network],
    )]));

    // Granted but not pinned to content → inert.
    set_acknowledged(&HashMap::new());
    assert!(
        match_in(&manifests, &handle).is_none(),
        "a grant with no content pin must not activate"
    );

    // Pinned to the current content → active.
    acknowledge(&manifests);
    assert!(
        match_in(&manifests, &handle).is_some(),
        "grant pinned to the current script activates"
    );

    // Content changed (stale pin) → inert again.
    set_acknowledged(&HashMap::from([(
        "needs_network".to_string(),
        "deadbeef".to_string(),
    )]));
    assert!(
        match_in(&manifests, &handle).is_none(),
        "a since-modified script must revert to needing consent"
    );

    set_granted(&HashMap::new());
    set_acknowledged(&HashMap::new());
}

#[test]
fn non_matching_handle_returns_none() {
    let manifests = vec![manifest()];
    assert!(match_in(&manifests, &hid(0x9999, 0x0000, None, 0)).is_none());
}

#[test]
fn disabled_plugin_does_not_match() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0xAAAA, pid = 0xBBBB, vendor = "Acme", model = "K1" } },
        }
    "#;
    let manifests = vec![parse_manifest(src, Path::new("disabled_only_plugin.lua")).unwrap()];
    let handle = hid(0xAAAA, 0xBBBB, Some("S"), 0);
    set_disabled(&["disabled_only_plugin".to_string()]);
    assert!(
        match_in(&manifests, &handle).is_none(),
        "disabled plugin must not shadow native"
    );
    set_disabled(&[]);
    assert!(match_in(&manifests, &handle).is_some());
}

#[test]
fn plugin_with_ungranted_permission_does_not_match() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 0xCCCC, pid = 0xDDDD, vendor = "Acme", model = "K2" } },
          permissions = { "network" },
        }
    "#;
    let manifests = vec![parse_manifest(src, Path::new("needs_network.lua")).unwrap()];
    let handle = hid(0xCCCC, 0xDDDD, Some("S"), 0);
    set_disabled(&[]);
    acknowledge(&manifests);
    set_granted(&HashMap::new());
    assert!(
        match_in(&manifests, &handle).is_none(),
        "declared-but-ungranted permission must keep the plugin inert"
    );

    let mut granted = HashMap::new();
    granted.insert("needs_network".to_string(), vec![Permission::Network]);
    set_granted(&granted);
    assert!(
        match_in(&manifests, &handle).is_some(),
        "fully granted plugin activates"
    );
    set_granted(&HashMap::new());
    set_acknowledged(&HashMap::new());
}

#[test]
fn consent_satisfied_true_when_no_permissions_declared() {
    // A plugin that declares no permissions runs freely — it can only talk
    // to its matched device, the base trust every plugin has.
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        }
    "#;
    let m = parse_manifest(src, Path::new("no_perms.lua")).unwrap();
    set_acknowledged(&HashMap::new());
    assert!(consent_satisfied(&m));
}

#[test]
fn load_all_with_repos_discovers_a_repo_sourced_plugin_and_tags_its_source() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        "id: acme-repo\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
    )
    .unwrap();
    std::fs::write(repo_dir.join("main.lua"), "return {}").unwrap();

    load_all_with_repos(local_dir.path(), std::slice::from_ref(&repo_dir));

    let secrets = FakeSecretStore::default();
    let infos = list(&secrets);
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

    load_all(Path::new("/nonexistent"));
    unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
}

#[test]
fn load_all_with_repos_discovers_sibling_packages_at_a_repos_root() {
    // The official repo's real layout: several packages as immediate sibling
    // directories under the repo root (no `plugins/` nesting, no
    // single-package-at-root manifest).
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
                "id: {id}\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
    };
    write_pkg("pkg_a");
    write_pkg("pkg_b");
    // A README alongside them must not be mistaken for a package.
    std::fs::write(repo_dir.join("README.md"), "not a plugin").unwrap();

    load_all_with_repos(local_dir.path(), std::slice::from_ref(&repo_dir));

    let secrets = FakeSecretStore::default();
    let infos = list(&secrets);
    for id in ["pkg_a", "pkg_b"] {
        assert!(
            infos.iter().any(|p| p.id == id),
            "sibling package '{id}' at the repo root must be discovered"
        );
    }

    load_all(Path::new("/nonexistent"));
    unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
}

#[test]
fn load_all_never_runs_a_dropped_in_scripts_side_effects() {
    // A malicious file dropped into the plugins dir tries to write a
    // sentinel at top level. `load_all` evaluates its manifest, but the
    // sandbox strips `io`/`os`, so the write never happens and the plugin
    // is skipped — dropping a file can't run code before consent.
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        "id: evil\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
    )
    .unwrap();
    std::fs::write(plugin_dir.join("main.lua"), evil).unwrap();

    load_all(dir.path());

    assert!(
        !sentinel.exists(),
        "a dropped-in script's filesystem write must never execute at load time"
    );
    assert!(
        !snapshot().manifests.iter().any(|m| m.plugin_id == "evil"),
        "a script that errors under the sandbox must be skipped, not registered"
    );
    load_all(Path::new("/nonexistent"));
}

#[test]
fn ungranted_in_reports_once_then_stays_silent() {
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

    let first = ungranted_in(&manifests, &mut notified);
    assert_eq!(
        first,
        vec![("Needs Net".to_string(), UngrantedReason::NeedsPermission)]
    );

    // Same manifest, same notified set: already announced, not repeated.
    let second = ungranted_in(&manifests, &mut notified);
    assert!(
        second.is_empty(),
        "must not repeat an already-notified plugin"
    );
}

#[test]
fn ungranted_in_flags_content_change_when_acknowledgment_is_stale() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    set_granted(&HashMap::from([(
        m.plugin_id.clone(),
        vec![Permission::Network],
    )]));
    set_acknowledged(&HashMap::from([(
        m.plugin_id.clone(),
        "stale-hash".to_string(),
    )]));
    let mut notified = HashSet::new();

    let out = ungranted_in(&[m], &mut notified);
    assert_eq!(
        out,
        vec![("Approved Once".to_string(), UngrantedReason::ContentChanged)]
    );

    set_granted(&HashMap::new());
    set_acknowledged(&HashMap::new());
}

#[test]
fn ungranted_in_skips_satisfied_manifests() {
    let src = r#"
        return {
          devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        }
    "#;
    let m = parse_manifest(src, Path::new("no_perms2.lua")).unwrap();
    let mut notified = HashSet::new();
    assert!(ungranted_in(&[m], &mut notified).is_empty());
}

#[test]
fn official_repo_plugin_cannot_be_shadowed_by_a_later_source() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let local_dir = tmp.path().join("plugins");
    let official_dir = tmp.path().join("official");
    std::fs::create_dir_all(&local_dir).unwrap();

    let write_dup = |dir: &Path, vendor: &str| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: dup\ndevices:\n  - vendor: {vendor}\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
    };
    // A repo-root plugin's id must equal its directory name (the slug), so
    // the official repo provides "dup" from a `plugins/<id>/` subdir instead.
    write_dup(&official_dir.join("plugins").join("dup"), "Official");
    write_dup(&local_dir.join("dup"), "Community");

    take_plugin_load_warnings(); // drain any stale state from a prior test
    load_all_with_repos(&local_dir, std::slice::from_ref(&official_dir));

    let state = snapshot();
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

    let warnings = take_plugin_load_warnings();
    assert!(
        warnings.iter().any(|w| w.plugin_id == "dup"),
        "the shadow attempt must be surfaced, not silently dropped"
    );

    load_all(Path::new("/nonexistent"));
}

#[test]
fn permission_gate_is_inert_until_granted_then_satisfied_once_acknowledged() {
    // Demonstrates the consent gate uniformly: declared-but-ungranted is
    // unsatisfied; fully granted + acknowledged is satisfied. This applies
    // to every plugin now — no source is exempt.
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        sensor = {},
        permissions = { "os" },
    }"#;
    let m = parse_manifest(src, Path::new("permission_demo.lua")).unwrap();
    assert_eq!(m.permissions, vec![Permission::Os]);
    assert!(m.needs_worker());

    // Acknowledged, so this isolates the permission gate (not the consent gate).
    acknowledge(std::slice::from_ref(&m));
    set_granted(&HashMap::new());
    assert!(
        !consent_satisfied(&m),
        "ungranted os permission keeps it inert"
    );

    let mut granted = HashMap::new();
    granted.insert("permission_demo".to_string(), vec![Permission::Os]);
    set_granted(&granted);
    assert!(consent_satisfied(&m));
    set_granted(&HashMap::new());
    set_acknowledged(&HashMap::new());
}

#[test]
fn list_reports_enabled_only_once_permissions_are_granted() {
    // Invariant: a plugin can never be reported enabled while its permissions
    // are ungranted — enabling and granting are one atomic step, so an
    // un-consented plugin is never "on".
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        permissions = { "os" },
    }"#;
    let m = parse_manifest(src, Path::new("perm_plugin.lua")).unwrap();
    set_registry(vec![m.clone()]);
    set_disabled(&[]);
    set_granted(&HashMap::new());
    // Acknowledged so only the permission grant — not the content gate —
    // decides consent here.
    acknowledge(std::slice::from_ref(&m));

    let secrets = FakeSecretStore::default();
    let enabled_of = |secrets: &FakeSecretStore| {
        list(secrets)
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
    set_granted(&granted);
    assert_eq!(
        enabled_of(&secrets),
        (true, true),
        "granting its permission enables it"
    );

    // An explicitly disabled plugin stays off even when fully granted.
    set_disabled(&["perm_plugin".to_string()]);
    assert!(!enabled_of(&secrets).0);

    set_disabled(&[]);
    set_granted(&HashMap::new());
    set_acknowledged(&HashMap::new());
    set_registry(Vec::new());
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
    fn backend_name(&self) -> &'static str {
        "fake"
    }
}

#[test]
fn config_for_defaults_unset_fields_and_overrides_set_ones() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        config = { fields = {
          { key = "host", label = "Host", default = "127.0.0.1" },
          { key = "port", label = "Port", default = "6742" },
          { key = "token", label = "Token", secure = true, default = "unused" },
        } },
    }"#;
    set_registry(vec![parse_manifest(src, Path::new("cfgfor.lua")).unwrap()]);
    let mut stored = HashMap::new();
    stored.insert(
        "cfgfor".to_string(),
        HashMap::from([("port".to_string(), "9999".to_string())]),
    );
    set_config_values(&stored);

    let resolved = config_for("cfgfor");
    assert_eq!(resolved.get("host"), Some(&"127.0.0.1".to_string()));
    assert_eq!(resolved.get("port"), Some(&"9999".to_string()));
    assert!(
        !resolved.contains_key("token"),
        "secure fields must never appear in the non-secure config map"
    );

    set_config_values(&HashMap::new());
    set_registry(Vec::new());
}

#[test]
fn config_for_unknown_plugin_is_empty() {
    assert!(config_for("does-not-exist").is_empty());
}

#[test]
fn integration_manifests_filters_by_type_disabled_and_permissions() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    set_registry(vec![
        parse_manifest(integ_src, Path::new("integ_ok.lua")).unwrap(),
        parse_manifest(device_src, Path::new("device_only.lua")).unwrap(),
        parse_manifest(needs_perm_src, Path::new("integ_needs_perm.lua")).unwrap(),
    ]);
    set_disabled(&[]);
    set_granted(&HashMap::new());

    let ids: Vec<String> = integration_manifests()
        .into_iter()
        .map(|m| m.plugin_id)
        .collect();
    assert_eq!(ids, vec!["integ_ok"]);

    set_disabled(&["integ_ok".to_string()]);
    assert!(integration_manifests().is_empty());
    set_disabled(&[]);

    // `integrations_disabled` is a second, independent gate: it must
    // exclude the integration even though the plugin itself is enabled.
    set_integrations_disabled(&["integ_ok".to_string()]);
    assert!(integration_manifests().is_empty());
    assert!(integration_manifest("integ_ok").is_none());
    set_integrations_disabled(&[]);

    assert_eq!(
        integration_manifest("integ_ok").map(|m| m.plugin_id),
        Some("integ_ok".to_string())
    );

    set_registry(Vec::new());
}

#[test]
fn secure_config_keys_for_returns_declared_secure_keys() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        config = { fields = {
          { key = "host", label = "Host" },
          { key = "token", label = "Token", secure = true },
        } },
    }"#;
    set_registry(vec![
        parse_manifest(src, Path::new("securekeys.lua")).unwrap()
    ]);

    assert_eq!(secure_config_keys_for("securekeys"), vec!["token"]);
    assert!(secure_config_keys_for("does-not-exist").is_empty());

    set_registry(Vec::new());
}

#[test]
fn list_reports_config_fields_values_and_secret_set() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = r#"return {
        devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        config = { fields = {
          { key = "host", label = "Host", default = "127.0.0.1" },
          { key = "token", label = "Token", secure = true },
        } },
    }"#;
    set_registry(vec![parse_manifest(src, Path::new("listcfg.lua")).unwrap()]);

    let secrets = FakeSecretStore::default();
    secrets.set("listcfg", "token", "s3cr3t").unwrap();

    let infos = list(&secrets);
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

    set_registry(Vec::new());
}

// ── Assets ────────────────────────────────────────────────────────

fn write_asset_plugin(root: &Path, id: &str, logo_bytes: Option<&[u8]>) -> std::path::PathBuf {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.yaml"),
        format!(
            "id: {id}\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n\
             logo: logo.png\neffects:\n  - id: rainbow\n    thumbnail: rainbow.png\n"
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
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "assetplug", None);
    load_all(tmp.path());

    let secrets = FakeSecretStore::default();
    let infos = list(&secrets);
    let info = infos.iter().find(|p| p.id == "assetplug").expect("present");
    assert_eq!(info.logo.as_deref(), Some("logo.png"));
    assert_eq!(info.effect_thumbnails.len(), 1);
    assert_eq!(info.effect_thumbnails[0].id, "rainbow");
    assert_eq!(info.effect_thumbnails[0].thumbnail, "rainbow.png");
    assert_eq!(info.source, halod_shared::types::PluginSource::Local);

    load_all(Path::new("/nonexistent"));
}

#[test]
fn list_leaves_logo_and_thumbnails_empty_when_undeclared() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("noassets");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.yaml"),
        "id: noassets\ndevices:\n  - vendor: x\n    model: y\n    transport: hid\n    vid: 1\n    pid: 2\n",
    )
    .unwrap();
    std::fs::write(dir.join("main.lua"), "return {}").unwrap();
    load_all(tmp.path());

    let secrets = FakeSecretStore::default();
    let infos = list(&secrets);
    let info = infos.iter().find(|p| p.id == "noassets").expect("present");
    assert!(info.logo.is_none());
    assert!(info.effect_thumbnails.is_empty());

    load_all(Path::new("/nonexistent"));
}

#[test]
fn read_asset_returns_bytes_for_a_declared_logo() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "readable", Some(b"PNGDATA"));
    load_all(tmp.path());

    let bytes = read_asset("readable", "logo.png").unwrap();
    assert_eq!(bytes, b"PNGDATA");

    load_all(Path::new("/nonexistent"));
}

#[test]
fn read_asset_rejects_missing_file_and_unknown_plugin() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "nofile", None);
    load_all(tmp.path());

    assert!(
        read_asset("nofile", "logo.png").is_err(),
        "declared asset with no file on disk must error"
    );
    assert!(
        read_asset("does-not-exist", "logo.png").is_err(),
        "unknown plugin id must error"
    );

    load_all(Path::new("/nonexistent"));
}

#[test]
fn read_asset_rejects_path_traversal_and_bad_extensions() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    write_asset_plugin(tmp.path(), "traversal", Some(b"PNGDATA"));
    load_all(tmp.path());

    for bad in [
        "../plugin.yaml",
        "../../etc/passwd",
        "logo",
        "logo.txt",
        "/etc/passwd",
    ] {
        assert!(
            read_asset("traversal", bad).is_err(),
            "'{bad}' must be rejected"
        );
    }

    load_all(Path::new("/nonexistent"));
}
