// SPDX-License-Identifier: GPL-3.0-or-later
//! Tests for the plugin registry itself (matching, consent, config
//! resolution, discovery notifications) — as opposed to the
//! per-plugin equivalence tests in the sibling modules.

use super::super::*;
use crate::secrets::SecretStore as _;
use halod_shared::types::DeviceType;
use std::path::Path;

use super::super::TEST_GLOBALS_LOCK as GLOBALS_LOCK;

fn manifest() -> PluginManifest {
    let src = r#"
        return {
          match = { transport = "hid", vid = 0x1234, pid = 0x5678 },
          identity = { vendor = "Acme", model = "K1", name = "Acme K1" },
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
          match = { transport = "hid", vid = 0xCCCC, pid = 0xDDDD },
          identity = { vendor = "Acme", model = "K2" },
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
          match = { transport = "hid", vid = 0xAAAA, pid = 0xBBBB },
          identity = { vendor = "Acme", model = "K1" },
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
          match = { transport = "hid", vid = 0xCCCC, pid = 0xDDDD },
          identity = { vendor = "Acme", model = "K2" },
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
          match = { transport = "hid", vid = 1, pid = 2 },
          identity = { vendor = "x", model = "y" },
        }
    "#;
    let m = parse_manifest(src, Path::new("no_perms.lua")).unwrap();
    set_acknowledged(&HashMap::new());
    assert!(consent_satisfied(&m));
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
           return {{ match = {{ transport = "hid", vid = 1, pid = 2 }},
                     identity = {{ vendor = "x", model = "y" }} }}"#,
        sentinel.display()
    );
    std::fs::write(dir.path().join("evil.lua"), evil).unwrap();

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
          match = { transport = "hid", vid = 1, pid = 2 },
          identity = { vendor = "x", model = "y", name = "Needs Net" },
          permissions = { "network" },
        }
    "#;
    let m = parse_manifest(src, Path::new("needs_net2.lua")).unwrap();
    let manifests = vec![m];
    let mut notified = HashSet::new();

    let first = ungranted_in(&manifests, &mut notified);
    assert_eq!(first, vec!["Needs Net".to_string()]);

    // Same manifest, same notified set: already announced, not repeated.
    let second = ungranted_in(&manifests, &mut notified);
    assert!(
        second.is_empty(),
        "must not repeat an already-notified plugin"
    );
}

#[test]
fn ungranted_in_skips_satisfied_manifests() {
    let src = r#"
        return {
          match = { transport = "hid", vid = 1, pid = 2 },
          identity = { vendor = "x", model = "y" },
        }
    "#;
    let m = parse_manifest(src, Path::new("no_perms2.lua")).unwrap();
    let mut notified = HashSet::new();
    assert!(ungranted_in(&[m], &mut notified).is_empty());
}

#[test]
fn ene_smbus_is_builtin_others_are_not() {
    assert!(is_builtin("ene_smbus"));
    assert!(!is_builtin("wled_udp"));
    assert!(!is_builtin("ene_smbus.lua")); // stem only, not the file name
}

#[test]
fn disk_plugin_cannot_impersonate_a_builtin_id() {
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // A file dropped as `openrgb.lua` claims a built-in id; if loaded it
    // would be consent-exempt and auto-granted the built-in's permissions.
    let evil = r#"
        return {
          match = { transport = "hid", vid = 1, pid = 2 },
          identity = { vendor = "EVIL", model = "y" },
          permissions = { "network", "os" },
        }
    "#;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("openrgb.lua"), evil).unwrap();
    load_all(dir.path());

    let state = snapshot();
    let openrgb: Vec<_> = state
        .manifests
        .iter()
        .filter(|m| m.plugin_id == "openrgb")
        .collect();
    assert_eq!(openrgb.len(), 1, "disk shadow must not join the built-in");
    assert_ne!(
        openrgb[0].identity.vendor, "EVIL",
        "the surviving 'openrgb' must be the compiled-in built-in, not the disk file"
    );
    drop(state);
    load_all(Path::new("/nonexistent"));
}

#[test]
fn shipped_example_plugin_parses() {
    // Guards the documented example against drift with the manifest schema.
    let src = include_str!("../../../../../../plugins/examples/example_device.lua");
    let m = parse_manifest(src, Path::new("example_device.lua")).unwrap();
    assert_eq!(m.identity.vendor, "Example");
    assert_eq!(m.capability_labels(), vec!["RGB", "Fan", "Sensor"]);
    assert!(m.needs_worker());
    assert_eq!(m.poll.as_ref().map(|p| p.interval_ms), Some(500));
}

#[test]
fn shipped_nzxt_kraken_plugin_parses_per_pid_identity() {
    // Guards the Z/Elite family plugin's per-PID name/device_type fix
    // (regression: every matched PID used to show as "Kraken Z" and
    // categorize as "unknown" instead of AIO).
    let src = include_str!("../builtins/nzxt_kraken.lua");
    let m = parse_manifest(src, Path::new("nzxt_kraken.lua")).unwrap();
    assert_eq!(m.match_specs.len(), 5);
    for spec in &m.match_specs {
        assert_eq!(spec.device_type, Some(DeviceType::AIO));
        assert!(spec.name.is_some(), "every PID needs its own display name");
    }
    let elite_v2 = m
        .match_specs
        .iter()
        .find(|s| s.pid == Some(0x3012))
        .expect("0x3012 (Elite V2) must be matched");
    assert_eq!(elite_v2.name.as_deref(), Some("Kraken Elite V2"));
    assert!(m.fan.is_some());
    assert!(m.lcd.is_some());
}

#[test]
fn shipped_nzxt_kraken_x3_plugin_parses() {
    // X53/X63/X73: distinct wire family — ring+logo RGB only, no
    // software pump/fan control, no LCD.
    let src = include_str!("../builtins/nzxt_kraken_x3.lua");
    let m = parse_manifest(src, Path::new("nzxt_kraken_x3.lua")).unwrap();
    assert_eq!(m.match_specs.len(), 2);
    for spec in &m.match_specs {
        assert_eq!(spec.device_type, Some(DeviceType::AIO));
        assert_eq!(spec.name.as_deref(), Some("Kraken X53/X63/X73"));
    }
    assert!(m.match_specs.iter().any(|s| s.pid == Some(0x2007)));
    assert!(m.match_specs.iter().any(|s| s.pid == Some(0x2014)));
    assert!(m.fan.is_none(), "X3 has no software pump/fan control");
    assert!(m.lcd.is_none(), "X3 has no LCD");
    let zones = &m.rgb.as_ref().unwrap().zones;
    assert_eq!(zones.len(), 2);
    assert_eq!(zones[0].leds.len(), 8);
    assert_eq!(zones[1].leds.len(), 1);
}

#[test]
fn shipped_nzxt_control_hub_plugin_parses() {
    let src = include_str!("../builtins/nzxt_control_hub.lua");
    let m = parse_manifest(src, Path::new("nzxt_control_hub.lua")).unwrap();
    assert_eq!(m.match_specs.len(), 1);
    assert_eq!(m.match_specs[0].pid, Some(0x2022));
    assert_eq!(m.match_specs[0].device_type, Some(DeviceType::Hub));
    assert!(m.rgb.is_none(), "hub has no LEDs of its own");
    assert!(m.fan.is_none(), "hub has no fan of its own");
    assert!(m.sensor.is_none());
    let chain = m.chain.as_ref().unwrap();
    assert_eq!(chain.channels.len(), 5);
    assert!(chain.accessories.iter().all(|a| a.fan));
}

#[test]
fn shipped_philips_evnia_plugin_parses_merged_capabilities() {
    // The merged monitor + Ambiglow plugin: one match on the DDC chip, a
    // bundled Ambiglow control endpoint, and every capability of both native
    // devices (RGB + range/choice/boolean/action).
    let src = include_str!("../builtins/philips_evnia.lua");
    let m = parse_manifest(src, Path::new("philips_evnia.lua")).unwrap();
    assert_eq!(m.match_specs.len(), 1);
    assert_eq!(m.match_specs[0].transport, "usb_control");
    assert_eq!(m.match_specs[0].vid, Some(0x2109));
    assert_eq!(m.match_specs[0].pid, Some(0x8884));
    assert_eq!(m.match_specs[0].device_type, Some(DeviceType::Monitor));

    // The Ambiglow chip is bundled as a secondary control endpoint.
    let uc = m
        .transports
        .usb_control
        .as_ref()
        .expect("declares a usb_control transport");
    assert_eq!(uc.endpoints.len(), 1);
    assert_eq!(uc.endpoints[0].id, "ambiglow");
    assert_eq!(uc.endpoints[0].vid, 0x0CF2);
    assert_eq!(uc.endpoints[0].pid, 0xB201);

    // Both chips' capabilities present on one device.
    let labels = m.capability_labels();
    assert!(labels.contains(&"RGB".to_owned()));
    assert!(labels.contains(&"Settings".to_owned())); // choice
    assert!(labels.contains(&"Controls".to_owned())); // range/boolean/action
    let rgb = m.rgb.as_ref().unwrap();
    assert_eq!(rgb.zones[0].leds.len(), 44);
    assert!(rgb.native_effects.iter().any(|e| e.id == "monitor"));
    assert_eq!(m.range.as_ref().unwrap().ranges.len(), 12);
    assert_eq!(m.choice.as_ref().unwrap().choices.len(), 14);
    assert_eq!(m.boolean.as_ref().unwrap().booleans.len(), 10);
    assert_eq!(m.action.as_ref().unwrap().actions.len(), 1);
}

#[test]
fn shipped_example_effects_plugin_parses() {
    // Guards the documented effects example against drift with the schema.
    let src = include_str!("../builtins/halo_effects.lua");
    let m = parse_manifest(src, Path::new("halo_effects.lua")).unwrap();
    assert!(
        m.match_specs.is_empty(),
        "effect-only plugin needs no match"
    );
    assert_eq!(m.plugin_type, PluginKind::Effect);
    assert!(!m.needs_worker());
    assert!(
        m.capability_labels().is_empty(),
        "effects aren't a capability"
    );
    assert_eq!(m.effects.len(), 10);
    let entries = effect_entries_for(&m);
    assert_eq!(entries[0].catalog_id, "halo_effects:plasma");
    assert_eq!(entries[0].kind, EffectKind::Pixmap);
    assert!(entries
        .iter()
        .any(|e| e.catalog_id == "halo_effects:comet" && e.kind == EffectKind::Direct));
}

#[test]
fn shipped_openrgb_plugin_parses() {
    // Guards the built-in OpenRGB integration against drift with the schema.
    let src = include_str!("../builtins/openrgb.lua");
    let m = parse_manifest(src, Path::new("openrgb.lua")).unwrap();
    assert!(
        m.match_specs.is_empty(),
        "integration plugin needs no match"
    );
    assert_eq!(m.plugin_type, PluginKind::Integration);
    assert!(m.needs_worker());
    assert_eq!(m.permissions, vec![Permission::Network, Permission::Os]);
    let tcp = m.transports.tcp.as_ref().expect("declares a tcp transport");
    assert_eq!(tcp.host_key, "host");
    assert_eq!(tcp.port_key, "port");
    let fields = m.config_fields();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].key, "host");
    assert_eq!(fields[0].default, "127.0.0.1");
    assert_eq!(fields[1].key, "port");
    assert_eq!(fields[1].default, "6742");
}

#[test]
fn builtin_plugins_are_permission_satisfied_without_a_grant() {
    // openrgb.lua declares `network`; being built-in (shipped with the
    // trusted daemon binary) must be enough — no manual consent step.
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_granted(&HashMap::new());
    let src = include_str!("../builtins/openrgb.lua");
    let m = parse_manifest(src, Path::new("openrgb.lua")).unwrap();
    assert!(consent_satisfied(&m));
}

#[test]
fn granted_for_auto_grants_a_builtins_own_declared_permissions() {
    // Regression: `permissions_satisfied` (discovery gating) bypassing
    // consent for built-ins isn't enough on its own — `granted_for` feeds
    // the *sandbox's* actual permission list (e.g. whether `os.clock()`
    // is reinjected), and previously had no built-in bypass at all, so a
    // built-in's own declared permissions were never actually granted at
    // the Lua level despite `permissions_satisfied` letting it through.
    // Uses the real `openrgb.lua` (declares `network` + `os`) since the
    // lookup is against the built-in sources, not `PLUGIN_REGISTRY` —
    // this must hold even before `load_all` has ever populated it.
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_granted(&HashMap::new());
    set_registry(Vec::new());

    let granted = granted_for("openrgb");
    assert!(granted.contains(&Permission::Network));
    assert!(granted.contains(&Permission::Os));

    // A non-builtin with the same declared permissions must NOT be
    // auto-granted — only an explicit user grant satisfies it.
    let src = r#"return {
        identity = { vendor = "x", model = "y" },
        type = "integration",
        permissions = { "network", "os" },
    }"#;
    set_registry(vec![parse_manifest(
        src,
        Path::new("some_other_plugin.lua"),
    )
    .unwrap()]);
    assert!(granted_for("some_other_plugin").is_empty());

    set_registry(Vec::new());
}

#[test]
fn effect_entries_for_namespaces_ids_and_carries_kind() {
    let src = r#"return {
        identity = { vendor = "x", model = "Effects" },
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
fn shipped_permission_demo_plugin_parses_and_is_inert_until_granted() {
    // Guards the permission-demo example against drift, and demonstrates
    // the gate: declared-but-ungranted is unsatisfied; fully granted is
    // satisfied. (Not routed through `match_in` — this plugin declares a
    // `sensor` capability, so `needs_worker()` is true and full device
    // construction would need a real HID transport, not just a runtime.)
    let _guard = GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let src = include_str!("../../../../../../plugins/examples/permission_demo.lua");
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
fn capability_labels_reflect_manifest_sections() {
    let src = r#"
        return {
          match = { transport = "hid", vid = 1, pid = 2 },
          identity = { vendor = "V", model = "M" },
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
        match = { transport = "hid", vid = 1, pid = 2 },
        identity = { vendor = "x", model = "y" },
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
        identity = { vendor = "x", model = "y" },
        type = "integration",
    }"#;
    let device_src = r#"return {
        identity = { vendor = "x", model = "y" },
        match = { transport = "hid", vid = 1, pid = 2 },
    }"#;
    let needs_perm_src = r#"return {
        identity = { vendor = "x", model = "y" },
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
        match = { transport = "hid", vid = 1, pid = 2 },
        identity = { vendor = "x", model = "y" },
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
        match = { transport = "hid", vid = 1, pid = 2 },
        identity = { vendor = "x", model = "y" },
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
