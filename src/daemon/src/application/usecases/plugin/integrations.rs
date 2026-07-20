// SPDX-License-Identifier: GPL-3.0-or-later
//! Per-integration enable/disable and config, independent of the generic
//! plugin toggle (which only governs whether the Lua may run at all — see
//! `usecases::plugins`). Unlike plugin edits, these apply immediately and are
//! scoped to the one integration: only its root device and the children it
//! exposes are torn down and rebuilt, never the whole device set.

use crate::domain::events::ChangeSink as _;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use anyhow::{bail, Context};
use base64::Engine as _;
use halod_shared::types::{
    IntegrationAuthKind, IntegrationSetupMode, IntegrationSetupPhase, IntegrationSetupStatus,
};
use rand::Rng;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::application::state::AppState;
use crate::application::usecases::registry::registration::unregister_device_and_children;
use crate::domain::plugin::observers::integration_scan;

async fn resolved_config(
    app: &Arc<AppState>,
    id: &str,
    granted: &[halod_shared::types::Permission],
) -> Result<crate::domain::plugin::ResolvedConfig> {
    let app = Arc::clone(app);
    let id = id.to_owned();
    let granted = granted.to_vec();
    tokio::task::spawn_blocking(move || {
        app.registry
            .resolved_config_for(app.secret_store.as_ref(), &id, &granted)
    })
    .await
    .context("secret-store task failed")
}

async fn setup_worker(
    app: &Arc<AppState>,
    manifest: &crate::domain::plugin::manifest::PluginManifest,
) -> Result<crate::domain::plugin::engine::worker::PluginHandle> {
    let granted = app.registry.granted_for(&manifest.plugin_id);
    let config = resolved_config(app, &manifest.plugin_id, &granted).await?;
    let http = crate::domain::plugin::engine::worker::http_runtime_for(manifest, &granted, &config);
    let udp = crate::domain::plugin::engine::worker::udp_runtime_for(manifest, &granted, &config);
    Ok(
        crate::domain::plugin::engine::worker::PluginHandle::spawn_with_data(
            manifest.script_source.clone(),
            manifest.module_sources.clone(),
            crate::domain::plugin::engine::transport::PluginIo::None,
            crate::domain::plugin::engine::worker::DevMatch {
                transport: "setup".into(),
                ..Default::default()
            },
            granted,
            config,
            tokio::runtime::Handle::current(),
            vec![],
            Default::default(),
            Default::default(),
            http,
            udp,
        ),
    )
}

async fn config_context(app: &Arc<AppState>, id: &str) -> Result<serde_json::Value> {
    let granted = app.registry.granted_for(id);
    let values = resolved_config(app, id, &granted)
        .await?
        .into_iter()
        .map(|(key, value)| (key, value.to_config_string()))
        .collect::<HashMap<_, _>>();
    Ok(json!({ "config": values }))
}

async fn publish_setup(app: &Arc<AppState>, id: &str, status: IntegrationSetupStatus) {
    app.registry
        .set_integration_setup_status(id.to_owned(), status);
    app.record_change(crate::domain::events::Change::PluginTopology)
        .await;
}

pub async fn begin_setup(id: String, app: Arc<AppState>) -> Result<()> {
    let manifest = app
        .registry
        .setup_integration_manifest(&id)
        .context("unknown, disabled, or unconsented integration")?;
    let setup = manifest
        .setup
        .as_ref()
        .context("integration does not require an interactive setup flow")?;
    let locale = app.config.read().await.gui.language.clone();
    let (title, instructions) = match &setup.auth {
        crate::domain::plugin::manifest::IntegrationAuthConfig::Button {
            title,
            instructions,
        } => (
            Some(
                manifest
                    .translate(&locale, "setup.auth.title", title)
                    .to_owned(),
            ),
            instructions
                .iter()
                .enumerate()
                .map(|(index, instruction)| {
                    manifest
                        .translate(
                            &locale,
                            &format!("setup.auth.instructions.{index}"),
                            instruction,
                        )
                        .to_owned()
                })
                .collect(),
        ),
        _ => (None, vec![]),
    };
    publish_setup(
        &app,
        &id,
        IntegrationSetupStatus {
            modes: setup.modes.clone(),
            auth: setup.auth.kind(),
            title,
            instructions,
            ..Default::default()
        },
    )
    .await;
    Ok(())
}

/// Return an integration to its pre-setup state, removing both plaintext
/// connection values and host-stored credentials, then open a fresh flow.
pub async fn reset_setup(id: String, app: Arc<AppState>) -> Result<()> {
    let manifest = app
        .registry
        .setup_integration_manifest(&id)
        .context("unknown, disabled, or unconsented integration")?;
    if manifest.setup.is_none() {
        bail!("integration does not have an interactive setup flow");
    }

    disable_one(&app, &id).await;
    app.registry.clear_integration_operational_errors(&id);
    app.registry.clear_integration_setup_status(&id);

    let keys = app.registry.secure_config_keys_for(&id);
    let store = Arc::clone(&app.secret_store);
    let integration_id = id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for key in keys {
            store.delete(&integration_id, &key).with_context(|| {
                format!("deleting secret '{key}' for integration '{integration_id}'")
            })?;
        }
        Ok(())
    })
    .await
    .context("secret-store cleanup task failed")??;
    {
        let mut cfg = app.config.write().await;
        cfg.plugins.config.remove(&id);
        cfg.plugins
            .integrations_configured
            .retain(|configured| configured != &id);
        cfg.plugins.integration_devices.remove(&id);
        app.registry.replace_policy(&cfg.plugins);
    }
    app.request_config_save();
    begin_setup(id, app).await
}

pub async fn select_setup_mode(
    id: String,
    mode: IntegrationSetupMode,
    app: Arc<AppState>,
) -> Result<()> {
    let manifest = app
        .registry
        .setup_integration_manifest(&id)
        .context("unknown, disabled, or unconsented integration")?;
    let setup = manifest
        .setup
        .as_ref()
        .context("integration has no setup")?;
    if !setup.modes.contains(&mode) {
        bail!("integration does not support the selected setup mode");
    }
    let mut status = app
        .registry
        .integration_setup_status(&id)
        .context("integration setup has not started")?;
    status.selected_mode = Some(mode);
    status.error = None;
    if mode == IntegrationSetupMode::Manual {
        status.phase = IntegrationSetupPhase::Init;
        publish_setup(&app, &id, status).await;
        return Ok(());
    }
    status.phase = IntegrationSetupPhase::Discovering;
    status.message = Some("integrations.setup_searching".into());
    status.candidates.clear();
    publish_setup(&app, &id, status.clone()).await;
    let candidates = match async {
        let raw =
            crate::infrastructure::integration_discovery::scan(setup.discovery.clone()).await?;
        let candidates = setup_worker(&app, &manifest)
            .await?
            .setup_discover(raw)
            .await?;
        validate_candidates(&manifest, &candidates)?;
        Ok::<_, anyhow::Error>(candidates)
    }
    .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            status.message = None;
            status.error = Some(error.to_string());
            publish_setup(&app, &id, status).await;
            return Ok(());
        }
    };
    if app.registry.integration_setup_status(&id).is_none() {
        return Ok(());
    }
    status.candidates = candidates;
    status.message = None;
    status.error = None;
    publish_setup(&app, &id, status).await;
    Ok(())
}

fn validate_candidates(
    manifest: &crate::domain::plugin::manifest::PluginManifest,
    candidates: &[halod_shared::types::IntegrationSetupCandidate],
) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for candidate in candidates {
        if candidate.id.trim().is_empty() || candidate.name.trim().is_empty() {
            bail!("discover() returned a candidate without an id or name");
        }
        if !ids.insert(&candidate.id) {
            bail!(
                "discover() returned duplicate candidate id '{}'",
                candidate.id
            );
        }
        for key in candidate.values.keys() {
            let declared = manifest
                .config_fields()
                .iter()
                .any(|field| &field.key == key && !field.secure);
            if !declared {
                bail!("discover() returned undeclared config key '{key}'");
            }
        }
    }
    Ok(())
}

fn validate_pair_result_values(
    manifest: &crate::domain::plugin::manifest::PluginManifest,
    values: &HashMap<String, String>,
) -> Result<()> {
    for (key, value) in values {
        if value.is_empty()
            && manifest
                .config_fields()
                .iter()
                .any(|field| field.secure && &field.key == key)
        {
            bail!("pair() returned an empty credential for secure field '{key}'");
        }
    }
    Ok(())
}

pub async fn submit_setup(
    id: String,
    candidate_id: Option<String>,
    mut values: HashMap<String, String>,
    app: Arc<AppState>,
) -> Result<()> {
    let manifest = app
        .registry
        .setup_integration_manifest(&id)
        .context("unknown, disabled, or unconsented integration")?;
    let mut status = app
        .registry
        .integration_setup_status(&id)
        .context("integration setup has not started")?;
    if let Some(candidate_id) = candidate_id {
        let candidate = status
            .candidates
            .iter()
            .find(|candidate| candidate.id == candidate_id)
            .context("unknown integration discovery candidate")?;
        for (key, value) in &candidate.values {
            values.entry(key.clone()).or_insert_with(|| value.clone());
        }
        status.selected_candidate = Some(candidate_id);
    }
    super::plugins::persist_config_values(&id, &values, &app).await?;
    app.request_config_save();
    match manifest.setup.as_ref().map(|setup| setup.auth.kind()) {
        Some(IntegrationAuthKind::Button) => {
            status.phase = IntegrationSetupPhase::Pairing;
            status.error = None;
            publish_setup(&app, &id, status).await;
        }
        Some(IntegrationAuthKind::Oauth2Pkce) => {
            start_oauth(manifest, status, app).await?;
        }
        _ => finish_setup(&id, &manifest, status, app).await?,
    }
    Ok(())
}

pub async fn retry_pairing(id: String, app: Arc<AppState>) -> Result<()> {
    let manifest = app
        .registry
        .setup_integration_manifest(&id)
        .context("unknown, disabled, or unconsented integration")?;
    let mut status = app
        .registry
        .integration_setup_status(&id)
        .context("integration setup has not started")?;
    if status.auth != IntegrationAuthKind::Button {
        bail!("integration does not use button pairing");
    }
    status.message = Some("integrations.setup_pairing_fallback".into());
    status.error = None;
    publish_setup(&app, &id, status.clone()).await;
    let result = match setup_worker(&app, &manifest)
        .await?
        .setup_pair(config_context(&app, &id).await?)
        .await
    {
        Ok(result) => result,
        Err(error) => {
            if app.registry.integration_setup_status(&id).is_none() {
                return Ok(());
            }
            status.message = None;
            status.error = Some(error.to_string());
            publish_setup(&app, &id, status).await;
            return Ok(());
        }
    };
    if result.pending || !result.ok {
        if app.registry.integration_setup_status(&id).is_none() {
            return Ok(());
        }
        status.message = None;
        status.error = result.reason;
        publish_setup(&app, &id, status).await;
        return Ok(());
    }
    if app.registry.integration_setup_status(&id).is_none() {
        return Ok(());
    }
    if let Err(error) = validate_pair_result_values(&manifest, &result.values) {
        status.message = None;
        status.error = Some(error.to_string());
        publish_setup(&app, &id, status).await;
        return Ok(());
    }
    super::plugins::persist_config_values(&id, &result.values, &app).await?;
    app.request_config_save();
    finish_setup(&id, &manifest, status, app).await
}

async fn finish_setup(
    id: &str,
    manifest: &crate::domain::plugin::manifest::PluginManifest,
    mut status: IntegrationSetupStatus,
    app: Arc<AppState>,
) -> Result<()> {
    let validation = match setup_worker(&app, manifest)
        .await?
        .setup_validate(config_context(&app, id).await?)
        .await
    {
        Ok(validation) => validation,
        Err(error) => crate::domain::plugin::engine::worker::SetupHookResult {
            ok: false,
            pending: false,
            reason: Some(error.to_string()),
            values: HashMap::new(),
        },
    };
    status.phase = IntegrationSetupPhase::Done;
    status.message = None;
    status.success = validation.ok;
    status.error = (!validation.ok).then(|| {
        validation
            .reason
            .unwrap_or_else(|| "Validation failed".into())
    });
    if validation.ok {
        {
            let mut cfg = app.config.write().await;
            if !cfg
                .plugins
                .integrations_configured
                .iter()
                .any(|item| item == id)
            {
                cfg.plugins.integrations_configured.push(id.to_owned());
            }
            if !cfg
                .plugins
                .integrations_enabled
                .iter()
                .any(|item| item == id)
            {
                cfg.plugins.integrations_enabled.push(id.to_owned());
            }
            if let Some(candidate) = &status.selected_candidate {
                cfg.plugins
                    .integration_devices
                    .insert(id.to_owned(), candidate.clone());
            }
            app.registry.replace_policy(&cfg.plugins);
        }
        app.request_config_save();
        enable_one(&app, id).await;
    }
    publish_setup(&app, id, status).await;
    Ok(())
}

async fn start_oauth(
    manifest: crate::domain::plugin::manifest::PluginManifest,
    mut status: IntegrationSetupStatus,
    app: Arc<AppState>,
) -> Result<()> {
    let crate::domain::plugin::manifest::IntegrationAuthConfig::Oauth2Pkce {
        authorization_url,
        token_url,
        client_id,
        scopes,
        access_token_key,
        refresh_token_key,
    } = manifest
        .setup
        .as_ref()
        .context("integration has no setup")?
        .auth
        .clone()
    else {
        bail!("integration does not use OAuth2 PKCE");
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("binding OAuth2 loopback callback")?;
    let redirect_uri = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let mut random = [0u8; 32];
    rand::rng().fill_bytes(&mut random);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let csrf = uuid::Uuid::new_v4().to_string();
    let mut authorization = url::Url::parse(&authorization_url)?;
    authorization
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &csrf);
    if !scopes.is_empty() {
        authorization
            .query_pairs_mut()
            .append_pair("scope", &scopes.join(" "));
    }
    status.phase = IntegrationSetupPhase::Pairing;
    status.external_url = Some(authorization.into());
    status.message = Some("integrations.setup_authorize_sub".into());
    let id = manifest.plugin_id.clone();
    publish_setup(&app, &id, status.clone()).await;
    tokio::spawn(async move {
        let result = async {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(300), listener.accept())
                .await
                .context("OAuth2 authorization timed out")??;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = vec![0u8; 8192];
            let length = stream.read(&mut request).await?;
            let first_line = std::str::from_utf8(&request[..length])?
                .lines()
                .next()
                .context("OAuth2 callback was empty")?;
            let path = first_line
                .split_whitespace()
                .nth(1)
                .context("OAuth2 callback request was malformed")?;
            let callback = url::Url::parse(&format!("http://localhost{path}"))?;
            let params: HashMap<String, String> = callback.query_pairs().into_owned().collect();
            if params.get("state") != Some(&csrf) {
                bail!("OAuth2 callback state did not match");
            }
            let code = params
                .get("code")
                .context("OAuth2 callback did not contain a code")?;
            if app.registry.integration_setup_status(&id).is_none() {
                bail!("OAuth2 setup was cancelled");
            }
            const CALLBACK_BODY: &str =
                "Authorization complete. You can return to HaloDaemon.";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{CALLBACK_BODY}",
                CALLBACK_BODY.len()
            );
            stream.write_all(response.as_bytes()).await?;

            let token = url::Url::parse(&token_url)?;
            let origin = format!(
                "{}://{}{}",
                token.scheme(),
                token.host_str().context("OAuth2 token URL has no host")?,
                token.port().map(|port| format!(":{port}")).unwrap_or_default()
            );
            let body = {
                let mut serializer = url::form_urlencoded::Serializer::new(String::new());
                serializer
                    .append_pair("grant_type", "authorization_code")
                    .append_pair("code", code)
                    .append_pair("client_id", &client_id)
                    .append_pair("redirect_uri", &redirect_uri)
                    .append_pair("code_verifier", &verifier);
                serializer.finish().into_bytes()
            };
            let policy = crate::domain::plugin::manifest::HttpConfig {
                origins: vec![origin.clone()],
                host_key: None,
                port_key: None,
                methods: vec!["POST".into()],
                max_request_bytes: 64 * 1024,
                max_response_bytes: 1024 * 1024,
                max_timeout_ms: 30_000,
                max_concurrency: 1,
                allow_private: false,
                tls: None,
            };
            let runtime = crate::infrastructure::http::HttpRuntime::from_config(&policy, None, None, None)?;
            let response = runtime.request(crate::infrastructure::http::HttpRequest {
                method: "POST".into(),
                origin,
                path: match token.query() {
                    Some(query) => format!("{}?{query}", token.path()),
                    None => token.path().to_owned(),
                },
                headers: vec![(
                    "Content-Type".into(),
                    "application/x-www-form-urlencoded".into(),
                )],
                body,
                timeout: Duration::from_secs(30),
            })?;
            if !(200..300).contains(&response.status) {
                bail!("OAuth2 token exchange returned HTTP {}", response.status);
            }
            let token: serde_json::Value = serde_json::from_slice(&response.body)?;
            let access_token = token
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .context("OAuth2 response did not contain an access token")?;
            let mut values = HashMap::from([(access_token_key, access_token.to_owned())]);
            if let (Some(key), Some(value)) = (
                refresh_token_key,
                token.get("refresh_token").and_then(serde_json::Value::as_str),
            ) {
                values.insert(key, value.to_owned());
            }
            if app.registry.integration_setup_status(&id).is_none() {
                bail!("OAuth2 setup was cancelled");
            }
            super::plugins::persist_config_values(&id, &values, &app).await?;
            app.request_config_save();
            finish_setup(&id, &manifest, status, app.clone()).await
        }
        .await;
        if let Err(error) = result {
            let Some(mut failed) = app.registry.integration_setup_status(&id) else {
                return;
            };
            failed.phase = IntegrationSetupPhase::Done;
            failed.success = false;
            failed.message = None;
            failed.external_url = None;
            failed.error = Some(error.to_string());
            publish_setup(&app, &id, failed).await;
        }
    });
    Ok(())
}

pub async fn cancel_setup(id: String, app: Arc<AppState>) -> Result<()> {
    app.registry.clear_integration_setup_status(&id);
    app.record_change(crate::domain::events::Change::PluginTopology)
        .await;
    Ok(())
}

/// Close and drop `id`'s integration root (and the children it exposes) from
/// `app.device_registry`, if currently registered. No-op otherwise.
async fn disable_one(app: &Arc<AppState>, id: &str) -> Option<String> {
    // Integration enablement is an independent lifecycle gate. Tear down its
    // shared snapshots at the same boundary as its devices so consumers never
    // observe a still-fresh value from a stopped producer.
    app.data_bus.invalidate_owner(id);
    let root_id = {
        let devices = app.device_registry.read().await;
        devices
            .iter()
            .find(|d| d.integration_id().as_deref() == Some(id))
            .map(|d| d.id().to_owned())
    };
    if let Some(root_id) = root_id {
        unregister_device_and_children(app, &root_id).await;
        Some(root_id)
    } else {
        None
    }
}

/// Connect and register `id`'s integration root (and its children), if it's
/// currently enabled and permission-satisfied. No-op otherwise.
async fn enable_one(app: &Arc<AppState>, id: &str) {
    let _ = integration_scan::discover_one(app, id).await;
}

/// Enable or disable a single integration, independent of the generic plugin
/// toggle. Applies immediately and only touches this one integration's root
/// + exposed devices — no global rediscovery.
pub async fn set_integration_enabled(id: String, enabled: bool, app: Arc<AppState>) -> Result<()> {
    if enabled {
        let manifest = app
            .registry
            .setup_integration_manifest(&id)
            .context("unknown, disabled, or unconsented integration")?;
        if !app.registry.integration_is_configured(&id) {
            bail!("integration must be configured before it can be enabled");
        }
        let validation = setup_worker(&app, &manifest)
            .await?
            .setup_validate(config_context(&app, &id).await?)
            .await?;
        if !validation.ok {
            bail!(
                "{}",
                validation
                    .reason
                    .unwrap_or_else(|| "integration validation failed".into())
            );
        }
    }
    {
        let mut cfg = app.config.write().await;
        cfg.plugins.integrations_enabled.retain(|x| x != &id);
        if enabled {
            cfg.plugins.integrations_enabled.push(id.clone());
        }
        app.registry.replace_policy(&cfg.plugins);
    }
    app.request_config_save();

    if enabled {
        enable_one(&app, &id).await;
    } else {
        disable_one(&app, &id).await;
        app.registry.clear_integration_operational_errors(&id);
    }
    app.record_change(crate::domain::events::Change::PluginTopology)
        .await;
    Ok(())
}

/// Replace a single integration's user-editable config values and reconnect
/// just that integration (e.g. a changed host/port takes effect immediately)
/// — every other device is left untouched.
pub async fn set_integration_config(
    id: String,
    values: HashMap<String, String>,
    app: Arc<AppState>,
) -> Result<()> {
    super::plugins::persist_config_values(&id, &values, &app).await?;
    app.request_config_save();

    disable_one(&app, &id).await;
    app.registry.clear_integration_operational_errors(&id);
    enable_one(&app, &id).await;
    app.record_change(crate::domain::events::Change::PluginTopology)
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MockDevice;
    use std::sync::atomic::Ordering;

    /// An integration-type plugin declaring the one secure config field the
    /// secret-store test exercises. `timeout_ms` is tiny so a scoped reconnect
    /// attempt (nothing is actually listening) fails fast rather than
    /// stalling the test.
    const INTEGRATION_CONFIG_TEST_PLUGIN: &str = "return {}";

    /// Loads `INTEGRATION_CONFIG_TEST_PLUGIN` into `app`'s plugin registry for
    /// the duration of `f`.
    async fn with_integration_config_test_plugin<F, Fut>(app: &Arc<AppState>, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("inttest");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.yaml"),
            "id: inttest\ntype: integration\npermissions: [network, secure_storage]\ntransports:\n  tcp:\n    host_key: host\n    port_key: port\n    timeout_ms: 50\nconfig:\n  fields:\n    - key: host\n      label: Host\n      kind: host\n      default: 127.0.0.1\n    - key: port\n      label: Port\n      kind: port\n      default: '12345'\n    - key: token\n      label: Token\n      secure: true\n",
        )
        .unwrap();
        std::fs::write(plugin_dir.join("main.lua"), INTEGRATION_CONFIG_TEST_PLUGIN).unwrap();
        app.registry.load_all(dir.path());
        f().await;
        app.registry.load_all(std::path::Path::new("/nonexistent"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn resolving_secure_config_does_not_block_the_async_runtime() {
        struct BlockingSecrets {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        }

        impl crate::infrastructure::secrets::SecretStore for BlockingSecrets {
            fn set(&self, _plugin_id: &str, _key: &str, _plaintext: &str) -> Result<()> {
                Ok(())
            }

            fn get(&self, _plugin_id: &str, _key: &str) -> Result<Option<String>> {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(Some("secret".into()))
            }

            fn delete(&self, _plugin_id: &str, _key: &str) -> Result<()> {
                Ok(())
            }
        }

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (entered_async_tx, entered_async_rx) = tokio::sync::oneshot::channel();
        let (completed_tx, completed_rx) = std::sync::mpsc::channel();
        let fallback_release = release_tx.clone();
        let watchdog = std::thread::spawn(move || {
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let _ = entered_async_tx.send(());
            if completed_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .is_err()
            {
                let _ = fallback_release.send(());
            }
        });

        let app = Arc::new(
            AppState::new(crate::config::Config::default()).with_secret_store(Arc::new(
                BlockingSecrets {
                    entered: entered_tx,
                    release: std::sync::Mutex::new(release_rx),
                },
            )),
        );
        with_integration_config_test_plugin(&app, || async {
            let started = std::time::Instant::now();
            let task = tokio::spawn({
                let app = app.clone();
                async move {
                    resolved_config(
                        &app,
                        "inttest",
                        &[halod_shared::types::Permission::SecureStorage],
                    )
                    .await
                }
            });

            entered_async_rx.await.unwrap();
            let responsive_after = started.elapsed();
            release_tx.send(()).unwrap();
            completed_tx.send(()).unwrap();
            task.await.unwrap().unwrap();
            assert!(
                responsive_after < std::time::Duration::from_millis(500),
                "secret-store access blocked the sole async worker for {responsive_after:?}"
            );
        })
        .await;
        watchdog.join().unwrap();
    }

    #[tokio::test]
    async fn set_integration_enabled_persists_the_policy() {
        crate::test_support::with_tmp_config(|app| async move {
            let root = Arc::new(MockDevice::new("openrgb-root").with_integration_id("openrgb"));
            let child = Arc::new(MockDevice::new("openrgb-root_ctrl_0"));
            let unrelated = Arc::new(MockDevice::new("other-device"));
            {
                let mut devices = app.device_registry.write().await;
                devices.push(root.clone());
                devices.push(child.clone());
                devices.push(unrelated.clone());
            }
            app.data_bus
                .publish(
                    "openrgb",
                    "openrgb.status",
                    crate::application::bus::data_bus::DataValue::Bool(true),
                    crate::application::bus::data_bus::DataPolicy {
                        stale_after: std::time::Duration::from_secs(60),
                        min_notify_interval: std::time::Duration::from_millis(16),
                    },
                )
                .unwrap();

            set_integration_enabled("openrgb".into(), false, app.clone())
                .await
                .unwrap();

            assert!(!app
                .config
                .read()
                .await
                .plugins
                .integrations_enabled
                .contains(&"openrgb".to_string()));
            let remaining = app.device_registry.read().await;
            assert_eq!(
                remaining.len(),
                1,
                "only the integration's subtree is torn down"
            );
            assert_eq!(remaining[0].id(), "other-device");
            drop(remaining);

            assert!(root.closed.load(Ordering::SeqCst));
            assert!(child.closed.load(Ordering::SeqCst));
            assert!(!unrelated.closed.load(Ordering::SeqCst));
            assert_eq!(
                app.data_bus.read("openrgb.status").status,
                crate::application::bus::data_bus::SnapshotStatus::Unavailable
            );
            assert!(app.data_bus.statuses_for_owner("openrgb").is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn set_integration_config_splits_secure_values_into_the_secret_store() {
        crate::test_support::with_tmp_config(|app| async move {
            with_integration_config_test_plugin(&app, || async {
                let mut values = HashMap::new();
                values.insert("host".to_string(), "127.0.0.1".to_string());
                values.insert("token".to_string(), "s3cr3t".to_string());
                set_integration_config("inttest".into(), values, app.clone())
                    .await
                    .unwrap();

                let cfg = app.config.read().await;
                assert_eq!(
                    cfg.plugins
                        .config
                        .get("inttest")
                        .and_then(|m| m.get("host")),
                    Some(&"127.0.0.1".to_string())
                );
                assert!(
                    !cfg.plugins
                        .config
                        .get("inttest")
                        .is_some_and(|m| m.contains_key("token")),
                    "a secure value must never land in the plaintext config map"
                );
                drop(cfg);
                assert_eq!(
                    app.secret_store.get("inttest", "token").unwrap(),
                    Some("s3cr3t".to_string())
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn disable_one_is_a_no_op_when_the_integration_is_not_registered() {
        crate::test_support::with_tmp_config(|app| async move {
            let unrelated = Arc::new(MockDevice::new("other-device"));
            app.device_registry.write().await.push(unrelated.clone());

            disable_one(&app, "does-not-exist").await;

            assert_eq!(app.device_registry.read().await.len(), 1);
            assert!(!unrelated.closed.load(Ordering::SeqCst));
        })
        .await;
    }

    #[tokio::test]
    async fn pairing_rejects_an_empty_secure_credential() {
        crate::test_support::with_tmp_config(|app| async move {
            let dir = tempfile::tempdir().unwrap();
            let plugin_dir = dir.path().join("empty_pair");
            std::fs::create_dir_all(&plugin_dir).unwrap();
            std::fs::write(
                plugin_dir.join("plugin.yaml"),
                "id: empty_pair\ntype: integration\npermissions: [network, secure_storage]\ntranslations:\n  it:\n    setup.auth.title: Associa\n    setup.auth.instructions.0: Premi il pulsante\ntransports:\n  http:\n    origins: [https://api.example.com]\n    methods: [POST]\nsetup:\n  modes: [manual]\n  auth:\n    kind: button\n    title: Pair\n    instructions: [Press the button]\nconfig:\n  fields:\n    - { key: host, label: Host, default: api.example.com }\n    - { key: token, label: Token, secure: true }\n",
            )
            .unwrap();
            std::fs::write(
                plugin_dir.join("main.lua"),
                "return {\n  pair = function(_) return { ok = true, values = { token = '' } } end,\n  validate = function(_) return { ok = true } end,\n}",
            )
            .unwrap();
            app.registry.load_all(dir.path());

            let authority = app
                .registry
                .list(app.secret_store.as_ref())
                .into_iter()
                .find(|plugin| plugin.id == "empty_pair")
                .unwrap()
                .authority;
            {
                // Keep the temporary registry loaded; the production enable
                // helper reconciles from configured repository paths.
                let mut cfg = app.config.write().await;
                cfg.plugins
                    .accepted_authorities
                    .insert("empty_pair".into(), authority.normalized());
                cfg.plugins.enabled.push("empty_pair".into());
                cfg.gui.language = "it".into();
                app.registry.replace_policy(&cfg.plugins);
            }

            begin_setup("empty_pair".into(), app.clone()).await.unwrap();
            let localized = app
                .registry
                .integration_setup_status("empty_pair")
                .unwrap();
            assert_eq!(localized.title.as_deref(), Some("Associa"));
            assert_eq!(localized.instructions, ["Premi il pulsante"]);
            select_setup_mode(
                "empty_pair".into(),
                IntegrationSetupMode::Manual,
                app.clone(),
            )
            .await
            .unwrap();
            submit_setup("empty_pair".into(), None, HashMap::new(), app.clone())
                .await
                .unwrap();
            retry_pairing("empty_pair".into(), app.clone())
                .await
                .unwrap();

            let status = app
                .registry
                .integration_setup_status("empty_pair")
                .unwrap();
            assert_eq!(status.phase, IntegrationSetupPhase::Pairing);
            assert!(status
                .error
                .as_deref()
                .is_some_and(|error| error.contains("empty credential")));
            assert_eq!(app.secret_store.get("empty_pair", "token").unwrap(), None);
            assert!(!app.registry.integration_is_configured("empty_pair"));
        })
        .await;
    }

    #[tokio::test]
    async fn reset_setup_removes_config_and_secrets_then_reopens_the_flow() {
        crate::test_support::with_tmp_config(|app| async move {
            let dir = tempfile::tempdir().unwrap();
            let plugin_dir = dir.path().join("resettable");
            std::fs::create_dir_all(&plugin_dir).unwrap();
            std::fs::write(
                plugin_dir.join("plugin.yaml"),
                "id: resettable\ntype: integration\npermissions: [network, secure_storage]\ntransports:\n  http:\n    origins: [https://api.example.com]\nsetup:\n  modes: [manual]\n  auth: { kind: none }\nconfig:\n  fields:\n    - { key: host, label: Host, kind: host, default: controller.local }\n    - { key: token, label: Token, secure: true }\n",
            )
            .unwrap();
            std::fs::write(
                plugin_dir.join("main.lua"),
                "return { validate = function(_) return { ok = true } end }",
            )
            .unwrap();
            app.registry.load_all(dir.path());

            let authority = app
                .registry
                .list(app.secret_store.as_ref())
                .into_iter()
                .find(|plugin| plugin.id == "resettable")
                .unwrap()
                .authority;
            {
                let mut cfg = app.config.write().await;
                cfg.plugins
                    .accepted_authorities
                    .insert("resettable".into(), authority);
                cfg.plugins.enabled.push("resettable".into());
                app.registry.replace_policy(&cfg.plugins);
            }
            crate::application::usecases::plugin::plugins::persist_config_values(
                "resettable",
                &HashMap::from([
                    ("host".into(), "192.0.2.10".into()),
                    ("token".into(), "secret-token".into()),
                ]),
                &app,
            )
            .await
            .unwrap();
            {
                let mut cfg = app.config.write().await;
                cfg.plugins.integrations_configured.push("resettable".into());
                cfg.plugins.integrations_enabled.push("resettable".into());
                cfg.plugins
                    .integration_devices
                    .insert("resettable".into(), "device-1".into());
                app.registry.replace_policy(&cfg.plugins);
            }

            reset_setup("resettable".into(), app.clone()).await.unwrap();

            let cfg = app.config.read().await;
            assert!(!cfg.plugins.config.contains_key("resettable"));
            assert!(!cfg
                .plugins
                .integrations_configured
                .contains(&"resettable".to_owned()));
            assert!(!cfg.plugins.integration_devices.contains_key("resettable"));
            drop(cfg);
            assert_eq!(
                app.secret_store.get("resettable", "token").unwrap(),
                None
            );
            assert_eq!(
                app.registry
                    .integration_setup_status("resettable")
                    .unwrap()
                    .phase,
                IntegrationSetupPhase::Init
            );
        })
        .await;
    }
}
