// SPDX-License-Identifier: GPL-3.0-or-later
use halod_shared::commands::{DaemonCommand, DeviceOp, Request};
use halod_shared::messages::{DaemonMessage, IpcError};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

use super::{ClientHandle, LeaseState};
use crate::application::state::AppState;
use crate::domain::profiles;

const LCD_PREVIEW_LEASE: Duration =
    Duration::from_secs(halod_shared::types::LCD_PREVIEW_LEASE_SECS);

const IPC_MAX_DEPTH: usize = 64;
const IPC_MAX_NODES: usize = 200_000;

/// Reject a command whose JSON nests too deep or holds too many collection nodes,
/// so a small frame can't expand into excessive nested work.
fn check_ipc_bounds(v: &Value) -> anyhow::Result<()> {
    crate::util::json::check_bounds(v, IPC_MAX_DEPTH, IPC_MAX_NODES)
}

/// Lease-gated LCD preview forwarder; drops the broadcast receiver once the lease goes `Expired`.
async fn lcd_preview_forward_loop(app: Arc<AppState>, client: ClientHandle) {
    let mut lease = client.lcd_lease_rx();
    loop {
        // Paused: no active lease. Wait for a keepalive (re-subscribe) or client teardown.
        let is_live = matches!(
            *lease.borrow_and_update(),
            LeaseState::Active { last_keepalive } if last_keepalive.elapsed() < LCD_PREVIEW_LEASE
        );
        if !is_live {
            tokio::select! {
                _ = client.tx.closed() => return,
                c = lease.changed() => { if c.is_err() { return; } continue; }
            }
        }
        // Engine may not be set yet at startup — same readiness dance as
        // canvas_forward_loop, plus the closed() escape.
        let mut ready = app.engines_ready.subscribe();
        let mut rx = loop {
            if let Some(e) = app.lcd.engine() {
                break e.subscribe();
            }
            tokio::select! {
                _ = client.tx.closed() => return,
                r = ready.changed() => { if r.is_err() { return; } }
            }
        };
        // Active: forward frames until the lease expires, then drop rx.
        loop {
            let deadline = match *lease.borrow_and_update() {
                LeaseState::Active { last_keepalive } => last_keepalive + LCD_PREVIEW_LEASE,
                LeaseState::Unsubscribed | LeaseState::Expired => break,
            };
            tokio::select! {
                _ = client.tx.closed() => return,
                _ = tokio::time::sleep_until(deadline) => { client.expire_lcd_lease(); break; }
                res = rx.recv() => match res {
                    Ok(frame) => client.send_lcd_preview(frame.wire.clone()),
                    Err(RecvError::Lagged(n)) =>
                        log::debug!("lcd_engine_frame subscriber lagged {n} frame(s)"),
                    Err(RecvError::Closed) => return,
                },
            }
        }
    }
}

async fn canvas_forward_loop(app: Arc<AppState>, client: ClientHandle) {
    let mut ready = app.engines_ready.subscribe();
    let mut rx = loop {
        if let Some(engine) = app.lighting.engine() {
            break engine.subscribe();
        }
        if ready.changed().await.is_err() {
            return;
        }
    };
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if client.tx.is_closed() {
                    break;
                }
                client.send(&DaemonMessage::CanvasFrame { data: frame });
            }
            Err(RecvError::Lagged(n)) => log::debug!("canvas_frame subscriber lagged {n} frame(s)"),
            Err(RecvError::Closed) => break,
        }
    }
}

pub async fn handle_message(msg: Value, client: ClientHandle, app: Arc<AppState>) {
    let cmd = msg["type"].as_str().unwrap_or("<untyped>").to_string();
    let req_id = msg["request_id"].as_str().map(str::to_string);

    if let Err(e) = check_ipc_bounds(&msg) {
        reply_error(
            &client,
            &cmd,
            &anyhow::anyhow!("frame rejected: {e}"),
            req_id,
        );
        return;
    }

    let Request {
        request_id,
        command,
    } = match Request::deserialize(&msg) {
        Ok(request) => request,
        Err(e) => {
            reply_error(
                &client,
                &cmd,
                &anyhow::anyhow!("failed to parse: {e}"),
                req_id,
            );
            return;
        }
    };

    // Device-scoped commands run off the reader on a per-device lock: commands
    // to the same device stay ordered and never overlap, while commands to
    // other devices run concurrently. This keeps one slow / timing-out device
    // (e.g. an unreachable wireless controller) from stalling every other
    // command. Global commands run inline in read order.
    match command_target(&command) {
        Some(id) => {
            let writes_device = command_writes_device(&command);
            let id = id.to_string();
            tokio::spawn(async move {
                let lock = app.device_lock(&id).await;
                let _guard = lock.lock().await;
                if let Err(e) = dispatch(command, client.clone(), Arc::clone(&app)).await {
                    log::warn!("command '{cmd}' failed: {e:#}");
                    let mut error = ipc_error(&e, request_id);
                    if writes_device && !error.handled {
                        let device = {
                            let devices = app.device_registry.read().await;
                            devices
                                .iter()
                                .find(|device| device.id() == id)
                                .map(|device| device.name().to_string())
                                .unwrap_or_else(|| id.clone())
                        };
                        crate::application::notifications::send(
                            &app,
                            halod_shared::types::NotificationCode::DeviceWriteFailed {
                                device,
                                detail: format!("{e:#}"),
                            },
                        )
                        .await;
                    }
                    error.handled |= writes_device;
                    client.send(&DaemonMessage::Error(error));
                }
            });
        }
        None => {
            if let Err(e) = dispatch(command, client.clone(), app).await {
                reply_error(&client, &cmd, &e, request_id);
            }
        }
    }
}

fn catalog_error_code(
    error: &halod_plugin_signing::CatalogError,
) -> halod_shared::types::CatalogErrorCode {
    use halod_plugin_signing::CatalogError as E;
    use halod_shared::types::CatalogErrorCode as C;
    match error {
        E::Malformed(_) => C::Malformed,
        E::Unsigned => C::Unsigned,
        E::UnknownKey(_) => C::UntrustedKey,
        E::SignatureMismatch => C::SignatureMismatch,
        E::Stale { .. } => C::Stale,
        E::HashMismatch { .. } => C::HashMismatch,
        E::ApiIncompatible { .. } => C::ApiIncompatible,
    }
}

fn ipc_error(e: &anyhow::Error, request_id: Option<String>) -> IpcError {
    IpcError {
        message: e.to_string(),
        code: e
            .downcast_ref::<halod_plugin_signing::CatalogError>()
            .map(catalog_error_code),
        handled: e
            .downcast_ref::<crate::domain::plugin::SurfacedPluginError>()
            .is_some(),
        request_id,
    }
}

fn reply_error(client: &ClientHandle, cmd: &str, e: &anyhow::Error, request_id: Option<String>) {
    log::warn!("command '{cmd}' failed: {e:#}");
    client.send(&DaemonMessage::Error(ipc_error(e, request_id)));
}

fn command_writes_device(cmd: &DaemonCommand) -> bool {
    matches!(cmd, DaemonCommand::Device { op, .. } if op.writes_device())
}

/// Serializes device-scoped work under [`AppState::device_lock`].
fn command_target(cmd: &DaemonCommand) -> Option<&str> {
    match cmd {
        DaemonCommand::Device { device_id, .. } => Some(device_id),
        _ => None,
    }
}

/// Dispatch a typed `DaemonCommand` to the matching use-case.
async fn dispatch(
    cmd: DaemonCommand,
    client: ClientHandle,
    app: Arc<AppState>,
) -> anyhow::Result<()> {
    match cmd {
        DaemonCommand::Device { device_id, op } => {
            dispatch_device(device_id, op, client, app).await
        }
        DaemonCommand::AddProfile { name } => {
            crate::application::usecases::profiles::lifecycle::add_profile(name, app).await
        }
        DaemonCommand::RenameProfile { old_name, new_name } => {
            crate::application::usecases::profiles::lifecycle::rename_profile(
                old_name, new_name, app,
            )
            .await
        }
        DaemonCommand::RemoveProfile { name } => {
            crate::application::usecases::profiles::lifecycle::remove_profile(name, app).await
        }
        DaemonCommand::SwitchProfile { name } => {
            crate::application::usecases::profiles::lifecycle::switch_profile(name, app).await
        }
        DaemonCommand::RemoveProfileOverride { profile, target } => {
            crate::application::usecases::profiles::profile_override::remove_profile_override(
                profile, target, app,
            )
            .await
        }
        DaemonCommand::SetLightingTargets {
            device_ids,
            channels,
        } => {
            crate::application::usecases::profiles::lifecycle::set_lighting_targets(
                device_ids, channels, app,
            )
            .await
        }
        DaemonCommand::AddAppRule {
            process_names,
            profile,
            enabled,
        } => {
            crate::application::usecases::profiles::app_rules::add(
                process_names,
                profile,
                enabled,
                app,
            )
            .await
        }
        DaemonCommand::UpdateAppRule {
            index,
            process_names,
            profile,
            enabled,
        } => {
            crate::application::usecases::profiles::app_rules::update(
                index,
                process_names,
                profile,
                enabled,
                app,
            )
            .await
        }
        DaemonCommand::RemoveAppRule { index } => {
            crate::application::usecases::profiles::app_rules::remove(index, app).await
        }
        DaemonCommand::Rediscover => {
            crate::application::usecases::registry::settings::rediscover(app).await
        }
        DaemonCommand::GetUdevRulesStatus => {
            crate::application::usecases::plugin::plugins::get_udev_rules_status(client, app).await
        }
        DaemonCommand::SetPluginEnabled { id, enabled } => {
            crate::application::usecases::plugin::plugins::set_enabled(id, enabled, app).await
        }
        DaemonCommand::SetPluginConfig { id, values } => {
            crate::application::usecases::plugin::plugins::set_config(id, values, app).await
        }
        DaemonCommand::AddPluginRepo { source } => {
            crate::application::usecases::plugin::repos::add_repo(source, app).await
        }
        DaemonCommand::RemovePluginRepo { slug } => {
            crate::application::usecases::plugin::repos::remove_repo(slug, app).await
        }
        DaemonCommand::RefreshPluginRepos => {
            crate::application::usecases::plugin::repos::refresh_repos(app).await
        }
        DaemonCommand::InstallPlugin { repo, id } => {
            crate::application::usecases::plugin::repos::install_plugin(repo, id, app).await
        }
        DaemonCommand::UninstallPlugin { id } => {
            crate::application::usecases::plugin::repos::uninstall_plugin(id, app).await
        }
        DaemonCommand::UpdateAllPlugins => {
            crate::application::usecases::plugin::repos::update_all_plugins(app).await
        }
        DaemonCommand::SetIntegrationEnabled { id, enabled } => {
            crate::application::usecases::plugin::integrations::set_integration_enabled(
                id, enabled, app,
            )
            .await
        }
        DaemonCommand::SetIntegrationConfig { id, values } => {
            crate::application::usecases::plugin::integrations::set_integration_config(
                id, values, app,
            )
            .await
        }
        DaemonCommand::SetLogLevel { level } => {
            crate::application::usecases::registry::settings::set_log_level(level, app).await
        }
        DaemonCommand::SetLanguage { lang } => {
            crate::application::usecases::registry::settings::set_language(lang, app).await
        }
        DaemonCommand::SetHomeWidgets { widgets } => {
            crate::application::usecases::registry::settings::set_home_widgets(widgets, app).await
        }
        DaemonCommand::SetCoolingWidgets { widgets } => {
            crate::application::usecases::registry::settings::set_cooling_widgets(widgets, app)
                .await
        }
        DaemonCommand::SetUiConfig {
            close_to_tray,
            suppress_dependency_warning,
            hide_window_controls,
            low_battery_notifications,
        } => {
            crate::application::usecases::registry::settings::set_ui_config(
                close_to_tray,
                suppress_dependency_warning,
                hide_window_controls,
                low_battery_notifications,
                app,
            )
            .await
        }
        DaemonCommand::MarkTourSeen { tour } => {
            crate::application::usecases::registry::settings::mark_tour_seen(tour, app).await
        }
        DaemonCommand::ResetToursSeen => {
            crate::application::usecases::registry::settings::reset_tours_seen(app).await
        }
        DaemonCommand::SetFanFailsafeDuty { duty } => {
            crate::application::usecases::cooling::failsafe::set_fan_failsafe_duty(duty, app).await
        }
        DaemonCommand::SetChannelVisibility {
            device_id,
            kind,
            channel_id,
            state,
        } => {
            crate::application::usecases::device::visibility::set_channel_visibility(
                device_id, kind, channel_id, state, app,
            )
            .await
        }

        DaemonCommand::PlayMacro { steps } => {
            crate::application::usecases::input::key_remap::play_macro(steps, app).await
        }

        DaemonCommand::ListLcdImages => {
            crate::application::usecases::lcd::controls::list_lcd_images(client).await
        }
        DaemonCommand::DeleteLcdImage { filename } => {
            crate::application::usecases::lcd::controls::delete_lcd_image(filename, app).await
        }
        DaemonCommand::GetPluginAsset { plugin_id, name } => {
            crate::application::usecases::plugin::plugins::get_asset(plugin_id, name, client, app)
                .await
        }
        DaemonCommand::GetLcdWidgetIcon { catalog_id } => {
            crate::application::usecases::plugin::plugins::get_lcd_widget_icon(
                catalog_id, client, app,
            )
            .await
        }
        DaemonCommand::GetLcdPluginPreset { catalog_id } => {
            crate::application::usecases::plugin::plugins::get_lcd_preset(catalog_id, client, app)
                .await
        }
        DaemonCommand::ListSerialPorts => {
            crate::application::usecases::plugin::plugins::list_serial_ports(client).await
        }

        DaemonCommand::CanvasUpsertEffect { instance_id, def } => {
            crate::application::usecases::lighting::canvas::upsert_effect(instance_id, def, app)
                .await
        }
        DaemonCommand::CanvasRemoveEffect { instance_id } => {
            crate::application::usecases::lighting::canvas::remove_effect(instance_id, app).await
        }
        DaemonCommand::CanvasSetDefaultEffect { instance_id } => {
            crate::application::usecases::lighting::canvas::set_default_effect(instance_id, app)
                .await
        }
        DaemonCommand::CanvasSetSampleRadius { radius } => {
            crate::application::usecases::lighting::canvas::set_sample_radius(radius, app).await
        }
        DaemonCommand::CanvasStop => {
            crate::application::usecases::lighting::canvas::stop(app).await
        }
        DaemonCommand::CanvasSubscribe => {
            if client.try_subscribe_canvas() {
                let task = tokio::spawn(canvas_forward_loop(app, client.clone()));
                client.track_subscription(task);
            }
            Ok(())
        }

        DaemonCommand::LcdEngineSubscribe => {
            client.touch_lcd_preview();
            if client.try_subscribe_lcd() {
                let task = tokio::spawn(lcd_preview_forward_loop(app, client.clone()));
                client.track_subscription(task);
            }
            Ok(())
        }
        DaemonCommand::SaveLcdTemplate { name, def } => {
            crate::application::usecases::lcd::templates::save_template(name, def, app).await
        }
        DaemonCommand::LoadLcdTemplate { name } => {
            crate::application::usecases::lcd::templates::load_template(name, client).await
        }
        DaemonCommand::DeleteLcdTemplate { name } => {
            crate::application::usecases::lcd::templates::delete_template(name, app).await
        }

        DaemonCommand::SaveCustomEffect { name, params } => {
            crate::application::usecases::lighting::custom_effects::save_custom_effect(
                name, params, app,
            )
            .await
        }
        DaemonCommand::DeleteCustomEffect { name } => {
            crate::application::usecases::lighting::custom_effects::delete_custom_effect(name, app)
                .await
        }

        DaemonCommand::ListRunningApps => profiles::observers::running_apps::list(client).await,
        DaemonCommand::GetDebugInfo => {
            crate::application::usecases::registry::debug::get_debug_info(client, app).await
        }
        DaemonCommand::SetEngineConfig {
            engine,
            enabled,
            tick_ms,
            fps,
            failsafe_duty,
        } => {
            crate::application::usecases::registry::settings::set_engine_config(
                engine,
                enabled,
                tick_ms,
                fps,
                failsafe_duty,
                app,
            )
            .await
        }
        DaemonCommand::Ping => {
            client.send(&DaemonMessage::Pong);
            Ok(())
        }
        DaemonCommand::Shutdown => {
            log::info!("Shutdown requested by a client");
            request_shutdown(&app);
            Ok(())
        }
    }
}

async fn dispatch_device(
    device_id: String,
    op: DeviceOp,
    client: ClientHandle,
    app: Arc<AppState>,
) -> anyhow::Result<()> {
    match op {
        DeviceOp::SetRange { key, value } => {
            crate::application::usecases::device::capability::set_capability_param(
                device_id,
                crate::application::usecases::device::capability::CapabilityParam::Range {
                    key,
                    value,
                },
                app,
            )
            .await
        }
        DeviceOp::SetChoice { key, selected } => {
            crate::application::usecases::device::capability::set_capability_param(
                device_id,
                crate::application::usecases::device::capability::CapabilityParam::Choice {
                    key,
                    selected,
                },
                app,
            )
            .await
        }
        DeviceOp::SetBoolean { key, value } => {
            crate::application::usecases::device::capability::set_capability_param(
                device_id,
                crate::application::usecases::device::capability::CapabilityParam::Boolean {
                    key,
                    value,
                },
                app,
            )
            .await
        }
        DeviceOp::TriggerAction { key } => {
            crate::application::usecases::device::capability::set_capability_param(
                device_id,
                crate::application::usecases::device::capability::CapabilityParam::Action { key },
                app,
            )
            .await
        }
        DeviceOp::BeginIntegrationSetup => {
            crate::application::usecases::plugin::integrations::begin_setup(device_id, app).await
        }
        DeviceOp::ResetIntegrationSetup => {
            crate::application::usecases::plugin::integrations::reset_setup(device_id, app).await
        }
        DeviceOp::SelectIntegrationSetupMode { mode } => {
            crate::application::usecases::plugin::integrations::select_setup_mode(
                device_id, mode, app,
            )
            .await
        }
        DeviceOp::SubmitIntegrationSetup {
            candidate_id,
            values,
        } => {
            crate::application::usecases::plugin::integrations::submit_setup(
                device_id,
                candidate_id,
                values,
                app,
            )
            .await
        }
        DeviceOp::RetryIntegrationPairing => {
            crate::application::usecases::plugin::integrations::retry_pairing(device_id, app).await
        }
        DeviceOp::CancelIntegrationSetup => {
            crate::application::usecases::plugin::integrations::cancel_setup(device_id, app).await
        }
        DeviceOp::ResetAllButtonMappings => {
            crate::application::usecases::input::key_remap::reset_all_button_mappings(
                device_id, app,
            )
            .await
        }
        DeviceOp::ResetButtonMapping { cid } => {
            crate::application::usecases::input::key_remap::reset_button_mapping(
                device_id, cid, app,
            )
            .await
        }
        DeviceOp::SetEqPreset { preset_index } => {
            crate::application::usecases::device::capability::set_capability_param(
                device_id,
                crate::application::usecases::device::capability::CapabilityParam::EqPreset {
                    preset_index,
                },
                app,
            )
            .await
        }
        DeviceOp::SetEqBands { values } => {
            crate::application::usecases::device::capability::set_capability_param(
                device_id,
                crate::application::usecases::device::capability::CapabilityParam::EqBands {
                    values,
                },
                app,
            )
            .await
        }
        DeviceOp::SetDpiSteps { steps } => {
            crate::application::usecases::device::capability::set_capability_param(
                device_id,
                crate::application::usecases::device::capability::CapabilityParam::DpiSteps {
                    steps,
                },
                app,
            )
            .await
        }
        DeviceOp::SetDeviceVisibility { state } => {
            crate::application::usecases::device::visibility::set_device_visibility(
                device_id, state, app,
            )
            .await
        }
        DeviceOp::SetKeyboardLayout { selection } => {
            crate::application::usecases::device::keyboard_layout::set_keyboard_layout(
                device_id, selection, app,
            )
            .await
        }
        DeviceOp::SetDeviceName { name } => {
            crate::application::usecases::device::rename::set_device_name(device_id, name, app)
                .await
        }
        DeviceOp::SetCoolingCurvePoints {
            channel_id,
            points,
            sensor_id,
        } => {
            crate::application::usecases::cooling::fan_curve::set_cooling_curve_points(
                device_id, channel_id, points, sensor_id, app,
            )
            .await
        }
        DeviceOp::SetCoolingCurvePreset {
            channel_id,
            preset,
            sensor_id,
        } => {
            crate::application::usecases::cooling::fan_curve::set_cooling_curve_preset(
                device_id, channel_id, preset, sensor_id, app,
            )
            .await
        }
        DeviceOp::RemoveCoolingCurve { channel_id } => {
            crate::application::usecases::cooling::fan_curve::remove_cooling_curve(
                device_id, channel_id, app,
            )
            .await
        }
        DeviceOp::LightingApply { state } => {
            crate::application::usecases::lighting::rgb::lighting_apply(device_id, state, app).await
        }
        DeviceOp::LightingSetChannelTransform {
            channel_id,
            transform,
        } => {
            crate::application::usecases::lighting::rgb::set_channel_transform(
                device_id, channel_id, transform, app,
            )
            .await
        }
        DeviceOp::LightingAddSegment {
            channel_id,
            name,
            led_count,
            topology,
        } => {
            crate::application::usecases::registry::chain::lighting_add_segment(
                device_id, channel_id, name, led_count, topology, app,
            )
            .await
        }
        DeviceOp::LightingRemoveSegment {
            channel_id,
            segment_id,
        } => {
            crate::application::usecases::registry::chain::lighting_remove_segment(
                device_id, channel_id, segment_id, app,
            )
            .await
        }
        DeviceOp::LightingReorderSegment {
            channel_id,
            segment_id,
            new_index,
        } => {
            crate::application::usecases::registry::chain::lighting_reorder_segment(
                device_id, channel_id, segment_id, new_index, app,
            )
            .await
        }
        DeviceOp::LightingDetectSegments { channel_id } => {
            crate::application::usecases::registry::chain::lighting_detect_segments(
                device_id, channel_id, app,
            )
            .await
        }
        DeviceOp::SetButtonMapping { mapping } => {
            crate::application::usecases::input::key_remap::set_button_mapping(
                device_id, mapping, app,
            )
            .await
        }
        DeviceOp::SetSoftwareDpiSteps { steps } => {
            crate::application::usecases::input::key_remap::set_software_dpi_steps(
                device_id, steps, app,
            )
            .await
        }
        DeviceOp::OnboardProfileSwitch { slot } => {
            crate::application::usecases::input::onboard_profiles::switch_onboard_profile(
                device_id, slot, app,
            )
            .await
        }
        DeviceOp::OnboardProfileRestore { slot } => {
            crate::application::usecases::input::onboard_profiles::restore_onboard_profile(
                device_id, slot, app,
            )
            .await
        }
        DeviceOp::OnboardProfileSetEnabled { slot, enabled } => {
            crate::application::usecases::input::onboard_profiles::set_onboard_profile_enabled(
                device_id, slot, enabled, app,
            )
            .await
        }
        DeviceOp::SetScreenImage { data_b64 } => {
            crate::application::usecases::lcd::controls::set_screen_image(
                device_id, data_b64, app, client,
            )
            .await
        }
        DeviceOp::SetScreenImageFromLibrary { filename } => {
            crate::application::usecases::lcd::controls::set_screen_image_from_library(
                device_id, filename, app, client,
            )
            .await
        }
        DeviceOp::SetScreenRotation { rotation } => {
            crate::application::usecases::lcd::controls::set_screen_rotation(
                device_id, rotation, app,
            )
            .await
        }
        DeviceOp::SetScreenBrightness { brightness } => {
            crate::application::usecases::lcd::controls::set_screen_brightness(
                device_id, brightness, app,
            )
            .await
        }
        DeviceOp::SetScreenDefault => {
            crate::application::usecases::lcd::controls::set_screen_default(device_id, app).await
        }
        DeviceOp::SetScreenRawStreaming { enabled } => {
            crate::application::usecases::lcd::controls::set_screen_raw_streaming(
                device_id, enabled, app,
            )
            .await
        }
        DeviceOp::SetScreenVideo { path } => {
            crate::application::usecases::lcd::controls::set_screen_video(device_id, path, app)
                .await
        }
        DeviceOp::CanvasPlaceZone {
            channel_id,
            x,
            y,
            w,
            h,
            rotation,
            effect,
            sampling_mode,
        } => {
            crate::application::usecases::lighting::canvas::place_zone(
                device_id,
                channel_id,
                x,
                y,
                w,
                h,
                rotation,
                effect,
                sampling_mode,
                app,
            )
            .await
        }
        DeviceOp::CanvasRemoveZone { channel_id } => {
            crate::application::usecases::lighting::canvas::remove_zone(device_id, channel_id, app)
                .await
        }
        DeviceOp::CanvasMoveZone {
            channel_id,
            x,
            y,
            w,
            h,
            rotation,
            effect,
            sampling_mode,
        } => {
            crate::application::usecases::lighting::canvas::move_zone(
                device_id,
                channel_id,
                x,
                y,
                w,
                h,
                rotation,
                effect,
                sampling_mode,
                app,
            )
            .await
        }
        DeviceOp::LcdEngineSetTemplate {
            template_id,
            params,
        } => {
            crate::application::usecases::lcd::engine::set_template(
                device_id,
                template_id,
                params,
                app,
            )
            .await
        }
        DeviceOp::LcdEngineDeactivate => {
            crate::application::usecases::lcd::engine::deactivate(device_id, app).await
        }
        DeviceOp::RenderLcdEditor { def, known } => {
            crate::application::usecases::lcd::editor::render(device_id, def, known, app, client)
                .await
        }
        DeviceOp::ReceiverStartPairing { timeout_secs } => {
            crate::application::usecases::registry::receiver::start_pairing(
                device_id,
                timeout_secs,
                app,
            )
            .await
        }
        DeviceOp::ReceiverStopPairing => {
            crate::application::usecases::registry::receiver::stop_pairing(device_id, app).await
        }
        DeviceOp::ReceiverUnpair { slot } => {
            crate::application::usecases::registry::receiver::unpair(device_id, slot, app).await
        }
    }
}

/// Handle an IPC `shutdown` command (the tray's "Quit").
///
/// In the Windows service `--worker`, the worker asks the SCM to stop the whole
/// `HalodDaemon` service — letting the worker merely exit would have the
/// supervisor relaunch it. The worker carries the elevated token, so it has the
/// rights to do so; the supervisor's stop handler then terminates this process.
///
/// In a dev/plain run there is no service, so just trigger a graceful shutdown.
///
/// Also used by the idle-shutdown watcher ([`crate::application::lifecycle`]) — a client
/// `Shutdown` command and "no client has been connected for a while" both
/// route through the same stop path.
pub(crate) fn request_shutdown(app: &Arc<AppState>) {
    // The daemon is a plain user process launched by the GUI — nothing
    // relaunches it, so exiting is the whole shutdown. (The on-demand broker
    // service self-stops once this worker's register-bus connections drop.)
    app.shutdown.notify_one();
}

#[cfg(test)]
mod tests {
    use super::ClientHandle;
    use super::*;
    use crate::config::Config;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn ipc_bounds_reject_deep_nesting() {
        let mut v = json!(0);
        for _ in 0..IPC_MAX_DEPTH + 2 {
            v = json!([v]);
        }
        assert!(check_ipc_bounds(&v).is_err());
    }

    #[test]
    fn ipc_bounds_reject_too_many_nodes() {
        let v = json!(vec![0u8; IPC_MAX_NODES + 1]);
        assert!(check_ipc_bounds(&v).is_err());
    }

    #[test]
    fn ipc_bounds_allow_large_string_single_node() {
        let v = json!({ "data": "x".repeat(5_000_000) });
        assert!(check_ipc_bounds(&v).is_ok());
    }

    #[tokio::test]
    async fn error_reply_echoes_request_id() {
        let (tx, mut rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(8);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        };
        let app = Arc::new(AppState::new(Config::default()));

        handle_message(
            json!({"type": "definitely_not_a_command", "request_id": "req-42"}),
            client,
            app.clone(),
        )
        .await;

        let frame = rx.recv().await.expect("an error frame");
        let reply: Value = serde_json::from_slice(&frame[5..]).unwrap();
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["request_id"], "req-42");
    }

    #[tokio::test]
    async fn signature_failure_reply_has_localizable_code_and_raw_log_message() {
        let (tx, mut rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(1);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        };
        reply_error(
            &client,
            "add_plugin_repo",
            &halod_plugin_signing::CatalogError::SignatureMismatch.into(),
            None,
        );

        let frame = rx.recv().await.unwrap();
        let reply: Value = serde_json::from_slice(&frame[5..]).unwrap();
        assert_eq!(reply["code"], "signature_mismatch");
    }

    #[tokio::test]
    async fn failed_device_write_sends_detail_notification_and_handled_error() {
        let (tx, mut rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(8);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: std::sync::Arc::default(),
        };
        let app = Arc::new(AppState::new(Config::default()));
        app.clients.lock().await.push(client.clone());

        handle_message(
            json!({
                "type": "device",
                "device_id": "missing-device",
                "op": {
                    "type": "lighting_apply",
                    "state": {"mode": "static", "color": {"r": 1, "g": 2, "b": 3}}
                },
                "request_id": "write-1"
            }),
            client,
            app.clone(),
        )
        .await;

        let frame = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("response timed out")
            .expect("response channel closed");
        let error: Value = serde_json::from_slice(&frame[5..]).unwrap();
        assert_eq!(error["request_id"], "write-1");
        assert_eq!(error["handled"], true);

        let replay = app.data_bus.replay_events(None);
        let notification = replay.events.last().expect("device-write event");
        let halod_shared::bus::BusEventPayload::Notification(notification) = &notification.payload;
        assert!(matches!(
            &notification.code,
            halod_shared::types::NotificationCode::DeviceWriteFailed { device, detail }
                if device == "missing-device" && detail.contains("missing-device")
        ));
    }

    #[test]
    fn only_hardware_commands_are_classified_as_device_writes() {
        use halod_shared::types::{LightingState, RgbColor};
        assert!(command_writes_device(&DaemonCommand::Device {
            device_id: "kbd".into(),
            op: DeviceOp::LightingApply {
                state: LightingState::Static {
                    color: RgbColor { r: 1, g: 2, b: 3 },
                }
            }
        }));
        assert!(!command_writes_device(&DaemonCommand::Device {
            device_id: "kbd".into(),
            op: DeviceOp::SetDeviceName {
                name: "Desk keyboard".into()
            }
        }));
    }

    #[test]
    fn command_target_routes_device_commands_and_leaves_globals_unlocked() {
        use halod_shared::types::{LightingState, RgbColor};
        assert_eq!(
            command_target(&DaemonCommand::Device {
                device_id: "kbd".into(),
                op: DeviceOp::LightingApply {
                    state: LightingState::Static {
                        color: RgbColor { r: 0, g: 0, b: 0 }
                    }
                }
            }),
            Some("kbd")
        );
        assert_eq!(
            command_target(&DaemonCommand::Device {
                device_id: "fan0".into(),
                op: DeviceOp::RemoveCoolingCurve {
                    channel_id: "fan".into()
                }
            }),
            Some("fan0")
        );
        assert_eq!(
            command_target(&DaemonCommand::Device {
                device_id: "ram".into(),
                op: DeviceOp::CanvasMoveZone {
                    channel_id: "leds".into(),
                    x: 0.0,
                    y: 0.0,
                    w: None,
                    h: None,
                    rotation: None,
                    effect: None,
                    sampling_mode: None
                }
            }),
            Some("ram")
        );
        assert_eq!(
            command_target(&DaemonCommand::Device {
                device_id: "hue".into(),
                op: DeviceOp::RetryIntegrationPairing
            }),
            Some("hue")
        );
        // Global commands run inline (no device lock) so a stuck device can't
        // stall them.
        assert_eq!(command_target(&DaemonCommand::CanvasStop), None);
        assert_eq!(command_target(&DaemonCommand::Rediscover), None);
        assert_eq!(
            command_target(&DaemonCommand::SwitchProfile {
                name: "gaming".into()
            }),
            None
        );
    }

    #[tokio::test]
    async fn shutdown_request_signals_a_plain_daemon() {
        let app = Arc::new(AppState::new(Config::default()));

        request_shutdown(&app);

        tokio::time::timeout(Duration::from_millis(200), app.shutdown.notified())
            .await
            .expect("shutdown should have been signalled");
    }

    /// Test helper: `AppState` with a live `LcdEngine`.
    fn app_with_lcd_engine() -> (Arc<AppState>, Arc<crate::domain::lcd::engine::LcdEngine>) {
        let app = Arc::new(AppState::new(Config::default()));
        let engine = crate::test_support::install_lcd_engine(&app);
        let _ = app.engines_ready.send(true);
        (app, engine)
    }

    #[tokio::test(start_paused = true)]
    async fn lcd_preview_forwarder_holds_receiver_only_while_leased() {
        let (app, engine) = app_with_lcd_engine();
        let (tx, _rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(8);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        client.touch_lcd_preview();

        let handle = tokio::spawn(lcd_preview_forward_loop(app, client.clone()));
        // Let the forwarder subscribe.
        for _ in 0..50 {
            if engine.frame_sender().receiver_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(engine.frame_sender().receiver_count(), 1);

        // Advance past the lease with no renewal: the forwarder drops rx.
        tokio::time::advance(LCD_PREVIEW_LEASE + Duration::from_millis(50)).await;
        for _ in 0..50 {
            if engine.frame_sender().receiver_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(engine.frame_sender().receiver_count(), 0);

        // Renew: the forwarder re-subscribes.
        client.touch_lcd_preview();
        for _ in 0..50 {
            if engine.frame_sender().receiver_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(engine.frame_sender().receiver_count(), 1);

        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn lcd_lease_state_is_observably_expired_after_the_deadline() {
        let (app, engine) = app_with_lcd_engine();
        let (tx, _rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(8);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        client.touch_lcd_preview();
        assert!(matches!(
            *client.lcd_lease_rx().borrow(),
            LeaseState::Active { .. }
        ));

        let handle = tokio::spawn(lcd_preview_forward_loop(app, client.clone()));
        for _ in 0..50 {
            if engine.frame_sender().receiver_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }

        tokio::time::advance(LCD_PREVIEW_LEASE + Duration::from_millis(50)).await;
        for _ in 0..50 {
            if *client.lcd_lease_rx().borrow() == LeaseState::Expired {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*client.lcd_lease_rx().borrow(), LeaseState::Expired);

        handle.abort();
    }

    #[tokio::test]
    async fn lcd_preview_forwarder_exits_when_client_disconnects_while_paused() {
        let (app, _engine) = app_with_lcd_engine();
        let (tx, rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(8);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        // A default lease is `Unsubscribed` (never entered the active branch), so the loop is already paused.
        drop(rx);

        let handle = tokio::spawn(lcd_preview_forward_loop(app, client));
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("forwarder must exit once the client disconnects")
            .expect("task must not panic");
    }

    #[tokio::test]
    async fn lcd_preview_forwarder_delivers_a_broadcast_frame_while_leased() {
        let (app, engine) = app_with_lcd_engine();
        let (tx, _rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(8);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        client.touch_lcd_preview();
        let mut preview_rx = client.subs.lcd_preview.subscribe();

        let handle = tokio::spawn(lcd_preview_forward_loop(app, client.clone()));
        for _ in 0..50 {
            if engine.frame_sender().receiver_count() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }

        let frame =
            crate::domain::lcd::engine::encode_wire_frame("lcd0", 1, "").expect("wire encode");
        let _ = engine.frame_sender().send(frame);

        tokio::time::timeout(Duration::from_secs(2), preview_rx.changed())
            .await
            .expect("a frame must reach the preview slot")
            .expect("slot must stay open");
        let wire = preview_rx.borrow().clone().expect("frame present");
        let reply: Value = serde_json::from_slice(&wire[5..]).unwrap();
        assert_eq!(reply["type"], "lcd_engine_frame");
        assert_eq!(reply["data"]["device_id"], "lcd0");

        handle.abort();
    }
}
