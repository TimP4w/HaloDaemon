// SPDX-License-Identifier: GPL-3.0-or-later
use halod_shared::commands::DaemonCommand;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

use crate::cooling;
use crate::input;
use crate::ipc::ClientHandle;
use crate::lcd;
use crate::lighting;
use crate::profiles;
use crate::registry;
use crate::state::AppState;

const LCD_PREVIEW_LEASE: Duration =
    Duration::from_secs(halod_shared::types::LCD_PREVIEW_LEASE_SECS);

/// Lease-gated LCD preview forwarder; drops the broadcast receiver when idle.
async fn lcd_preview_forward_loop(app: Arc<AppState>, client: ClientHandle) {
    let mut keepalive = client.lcd_keepalive_rx();
    loop {
        // Paused: no receiver held. Wait for a keepalive or client teardown.
        if keepalive.borrow_and_update().elapsed() >= LCD_PREVIEW_LEASE {
            tokio::select! {
                _ = client.tx.closed() => return,
                c = keepalive.changed() => { if c.is_err() { return; } continue; }
            }
        }
        // Engine may not be set yet at startup — same readiness dance as
        // engine_subscribe_loop, plus the closed() escape.
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
            let deadline = *keepalive.borrow_and_update() + LCD_PREVIEW_LEASE;
            tokio::select! {
                _ = client.tx.closed() => return,
                _ = tokio::time::sleep_until(deadline) => break,
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

async fn engine_subscribe_loop<T, F>(
    app: Arc<AppState>,
    client: ClientHandle,
    get_receiver: F,
    frame_type: &'static str,
) where
    T: Serialize + Send + Sync + 'static,
    F: Fn(&AppState) -> Option<tokio::sync::broadcast::Receiver<Arc<T>>>,
{
    // The domain engine is set once during startup, possibly after this client
    // subscribes. Wake on the readiness signal rather than polling; re-check
    // each round so a flip that races the subscribe is never missed.
    let mut ready = app.engines_ready.subscribe();
    let mut rx = loop {
        if let Some(rx) = get_receiver(&app) {
            break rx;
        }
        if ready.changed().await.is_err() {
            return; // daemon shutting down
        }
    };
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if client.tx.is_closed() {
                    break;
                }
                client.send_json(&json!({"type": frame_type, "data": *frame}));
            }
            Err(RecvError::Lagged(n)) => log::debug!("{frame_type} subscriber lagged {n} frame(s)"),
            Err(RecvError::Closed) => break,
        }
    }
}

pub async fn handle_message(msg: Value, client: ClientHandle, app: Arc<AppState>) {
    let Some(cmd) = msg["type"].as_str() else {
        log::warn!("IPC: frame missing string 'type' field");
        client
            .send_json(&json!({"type": "error", "message": "missing or non-string 'type' field"}));
        return;
    };

    if cmd == "ping" {
        client.send_json(&json!({"type": "pong"}));
        return;
    }

    let req_id = msg["request_id"].as_str();

    let typed = match DaemonCommand::deserialize(&msg) {
        Ok(typed) => typed,
        Err(e) => {
            reply_error(
                &client,
                cmd,
                req_id,
                anyhow::anyhow!("failed to parse: {e}"),
            );
            return;
        }
    };

    // Device-scoped commands run off the reader on a per-device lock: commands
    // to the same device stay ordered and never overlap, while commands to
    // other devices run concurrently. This keeps one slow / timing-out device
    // (e.g. an unreachable wireless controller) from stalling every other
    // command. Global commands run inline in read order.
    match command_target(&typed) {
        Some(id) => {
            let id = id.to_string();
            let cmd = cmd.to_string();
            let req_id = req_id.map(str::to_string);
            tokio::spawn(async move {
                let lock = app.device_lock(&id).await;
                let _guard = lock.lock().await;
                if let Err(e) = dispatch(typed, client.clone(), Arc::clone(&app)).await {
                    reply_error(&client, &cmd, req_id.as_deref(), e);
                }
            });
        }
        None => {
            if let Err(e) = dispatch(typed, client.clone(), app).await {
                reply_error(&client, cmd, req_id, e);
            }
        }
    }
}

/// Send an `error` reply frame for a failed command, echoing `request_id` when
/// the client supplied one so it can correlate the failure.
fn reply_error(client: &ClientHandle, cmd: &str, req_id: Option<&str>, e: anyhow::Error) {
    log::warn!("command '{cmd}' failed: {e}");
    let mut reply = json!({"type": "error", "message": e.to_string()});
    if let Some(req_id) = req_id {
        reply["request_id"] = req_id.into();
    }
    client.send_json(&reply);
}

/// The device id a command targets, or `None` for global commands (profiles,
/// settings, canvas-wide effects, …). Determines the [`AppState::device_lock`]
/// used to serialize per-device work; see [`handle_message`].
fn command_target(cmd: &DaemonCommand) -> Option<&str> {
    use DaemonCommand::*;
    match cmd {
        SetChoice { id, .. }
        | SetRange { id, .. }
        | SetBoolean { id, .. }
        | TriggerAction { id, .. }
        | ResetAllButtonMappings { id, .. }
        | ResetButtonMapping { id, .. }
        | SetEqPreset { id, .. }
        | SetEqBands { id, .. }
        | SetDpiSteps { id, .. }
        | SetFanSpeed { id, .. }
        | RgbApply { id, .. }
        | RgbSetZoneTransform { id, .. }
        | RgbChainAddLink { id, .. }
        | RgbChainRemoveLink { id, .. }
        | RgbChainReorderLink { id, .. }
        | RgbChainDetectChannel { id, .. }
        | SetButtonMapping { id, .. }
        | SetSoftwareDpiSteps { id, .. }
        | OnboardProfileSwitch { id, .. }
        | OnboardProfileRestore { id, .. }
        | OnboardProfileSetEnabled { id, .. }
        | SetScreenImage { id, .. }
        | SetScreenImageFromLibrary { id, .. }
        | SetScreenRotation { id, .. }
        | SetScreenBrightness { id, .. }
        | SetScreenDefault { id, .. }
        | SetScreenRawStreaming { id, .. }
        | SetScreenVideo { id, .. }
        | ReceiverStartPairing { id, .. }
        | ReceiverStopPairing { id, .. }
        | ReceiverUnpair { id, .. } => Some(id),
        SetFanCurvePoints { fan_id, .. }
        | SetFanCurvePreset { fan_id, .. }
        | RemoveFanCurve { fan_id } => Some(fan_id),
        SetDeviceVisibility { device_id, .. }
        | SetDeviceName { device_id, .. }
        | CanvasPlaceZone { device_id, .. }
        | CanvasRemoveZone { device_id, .. }
        | CanvasMoveZone { device_id, .. }
        | LcdEngineSetTemplate { device_id, .. }
        | LcdEngineDeactivate { device_id, .. }
        | RenderLcdEditor { device_id, .. } => Some(device_id),
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
        DaemonCommand::SetRange { id, key, value } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::Range { key, value },
                app,
            )
            .await
        }
        DaemonCommand::SetChoice { id, key, selected } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::Choice { key, selected },
                app,
            )
            .await
        }
        DaemonCommand::SetBoolean { id, key, value } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::Boolean { key, value },
                app,
            )
            .await
        }
        DaemonCommand::TriggerAction { id, key } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::Action { key },
                app,
            )
            .await
        }
        DaemonCommand::AddProfile { name } => {
            profiles::usecases::profiles::add_profile(name, app).await
        }
        DaemonCommand::RenameProfile { old_name, new_name } => {
            profiles::usecases::profiles::rename_profile(old_name, new_name, app).await
        }
        DaemonCommand::RemoveProfile { name } => {
            profiles::usecases::profiles::remove_profile(name, app).await
        }
        DaemonCommand::SwitchProfile { name } => {
            profiles::usecases::profiles::switch_profile(name, app).await
        }
        DaemonCommand::RemoveProfileOverride { target } => {
            profiles::usecases::profile_override::remove_profile_override(target, app).await
        }
        DaemonCommand::SetLightingTargets { device_ids, zones } => {
            profiles::usecases::profiles::set_lighting_targets(device_ids, zones, app).await
        }
        DaemonCommand::AddAppRule {
            process_names,
            profile,
            enabled,
        } => profiles::usecases::app_rules::add(process_names, profile, enabled, app).await,
        DaemonCommand::UpdateAppRule {
            index,
            process_names,
            profile,
            enabled,
        } => {
            profiles::usecases::app_rules::update(index, process_names, profile, enabled, app).await
        }
        DaemonCommand::RemoveAppRule { index } => {
            profiles::usecases::app_rules::remove(index, app).await
        }
        DaemonCommand::Rediscover => registry::usecases::settings::rediscover(app).await,
        DaemonCommand::SetPluginEnabled { id, enabled } => {
            registry::usecases::plugins::set_enabled(id, enabled, app).await
        }
        DaemonCommand::SetLogLevel { level } => {
            registry::usecases::settings::set_log_level(level, app).await
        }
        DaemonCommand::SetLanguage { lang } => {
            registry::usecases::settings::set_language(lang, app).await
        }
        DaemonCommand::SetUiConfig {
            close_to_tray,
            suppress_dependency_warning,
            hide_window_controls,
        } => {
            registry::usecases::settings::set_ui_config(
                close_to_tray,
                suppress_dependency_warning,
                hide_window_controls,
                app,
            )
            .await
        }
        DaemonCommand::MarkTourSeen { tour } => {
            registry::usecases::settings::mark_tour_seen(tour, app).await
        }
        DaemonCommand::ResetToursSeen => registry::usecases::settings::reset_tours_seen(app).await,
        DaemonCommand::SetFanFailsafeDuty { duty } => {
            cooling::usecases::failsafe::set_fan_failsafe_duty(duty, app).await
        }
        DaemonCommand::ResetAllButtonMappings { id } => {
            input::usecases::key_remap::reset_all_button_mappings(id, app).await
        }
        DaemonCommand::ResetButtonMapping { id, cid } => {
            input::usecases::key_remap::reset_button_mapping(id, cid, app).await
        }
        DaemonCommand::SetEqPreset { id, preset_index } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::EqPreset { preset_index },
                app,
            )
            .await
        }
        DaemonCommand::SetEqBands { id, values } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::EqBands { values },
                app,
            )
            .await
        }
        DaemonCommand::SetDpiSteps { id, steps } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::DpiSteps { steps },
                app,
            )
            .await
        }
        DaemonCommand::SetDeviceVisibility { device_id, state } => {
            registry::usecases::visibility::set_device_visibility(device_id, state, app).await
        }
        DaemonCommand::SetSensorVisibility { sensor_id, state } => {
            registry::usecases::visibility::set_sensor_visibility(sensor_id, state, app).await
        }
        DaemonCommand::SetDeviceName { device_id, name } => {
            registry::usecases::rename::set_device_name(device_id, name, app).await
        }

        DaemonCommand::SetFanSpeed { id, duty } => {
            registry::usecases::capability::set_capability_param(
                id,
                registry::usecases::capability::CapabilityParam::FanDuty { duty },
                app,
            )
            .await
        }
        DaemonCommand::SetFanCurvePoints {
            fan_id,
            points,
            sensor_id,
        } => {
            cooling::usecases::fan_curve::set_fan_curve_points(fan_id, points, sensor_id, app).await
        }
        DaemonCommand::SetFanCurvePreset {
            fan_id,
            preset,
            sensor_id,
        } => {
            cooling::usecases::fan_curve::set_fan_curve_preset(fan_id, preset, sensor_id, app).await
        }
        DaemonCommand::RemoveFanCurve { fan_id } => {
            cooling::usecases::fan_curve::remove_fan_curve(fan_id, app).await
        }

        DaemonCommand::RgbApply { id, state } => {
            lighting::usecases::rgb::rgb_apply(id, state, app).await
        }
        DaemonCommand::RgbSetZoneTransform {
            id,
            zone_id,
            transform,
        } => lighting::usecases::rgb::set_zone_transform(id, zone_id, transform, app).await,
        DaemonCommand::RgbChainAddLink {
            id,
            channel_id,
            name,
            led_count,
            topology,
            kind,
        } => {
            registry::usecases::chain::rgb_chain_add_link(
                id, channel_id, name, led_count, topology, kind, app,
            )
            .await
        }
        DaemonCommand::RgbChainRemoveLink {
            id,
            channel_id,
            child_device_id,
        } => {
            registry::usecases::chain::rgb_chain_remove_link(id, channel_id, child_device_id, app)
                .await
        }
        DaemonCommand::RgbChainReorderLink {
            id,
            channel_id,
            child_device_id,
            new_index,
        } => {
            registry::usecases::chain::rgb_chain_reorder_link(
                id,
                channel_id,
                child_device_id,
                new_index,
                app,
            )
            .await
        }
        DaemonCommand::RgbChainDetectChannel { id, channel_id } => {
            registry::usecases::chain::rgb_chain_detect_channel(id, channel_id, app).await
        }

        DaemonCommand::SetButtonMapping { id, mapping } => {
            input::usecases::key_remap::set_button_mapping(id, mapping, app).await
        }
        DaemonCommand::SetSoftwareDpiSteps { id, steps } => {
            input::usecases::key_remap::set_software_dpi_steps(id, steps, app).await
        }
        DaemonCommand::PlayMacro { steps } => {
            input::usecases::key_remap::play_macro(steps, app).await
        }

        DaemonCommand::OnboardProfileSwitch { id, slot } => {
            input::usecases::onboard_profiles::switch_onboard_profile(id, slot, app).await
        }
        DaemonCommand::OnboardProfileRestore { id, slot } => {
            input::usecases::onboard_profiles::restore_onboard_profile(id, slot, app).await
        }
        DaemonCommand::OnboardProfileSetEnabled { id, slot, enabled } => {
            input::usecases::onboard_profiles::set_onboard_profile_enabled(id, slot, enabled, app)
                .await
        }

        DaemonCommand::SetScreenImage {
            id,
            data_b64,
            request_id,
        } => lcd::usecases::lcd::set_screen_image(id, data_b64, request_id, app, client).await,
        DaemonCommand::SetScreenImageFromLibrary {
            id,
            filename,
            request_id,
        } => {
            lcd::usecases::lcd::set_screen_image_from_library(id, filename, request_id, app, client)
                .await
        }
        DaemonCommand::SetScreenRotation { id, rotation } => {
            lcd::usecases::lcd::set_screen_rotation(id, rotation, app).await
        }
        DaemonCommand::SetScreenBrightness { id, brightness } => {
            lcd::usecases::lcd::set_screen_brightness(id, brightness, app).await
        }
        DaemonCommand::SetScreenDefault { id } => {
            lcd::usecases::lcd::set_screen_default(id, app).await
        }
        DaemonCommand::SetScreenRawStreaming { id, enabled } => {
            lcd::usecases::lcd::set_screen_raw_streaming(id, enabled, app).await
        }
        DaemonCommand::SetScreenVideo { id, path } => {
            lcd::usecases::lcd::set_screen_video(id, path, app).await
        }
        DaemonCommand::ListLcdImages => lcd::usecases::lcd::list_lcd_images(client).await,
        DaemonCommand::DeleteLcdImage { filename } => {
            lcd::usecases::lcd::delete_lcd_image(filename, app).await
        }

        DaemonCommand::CanvasUpsertEffect { instance_id, def } => {
            lighting::usecases::canvas::upsert_effect(instance_id, def, app).await
        }
        DaemonCommand::CanvasRemoveEffect { instance_id } => {
            lighting::usecases::canvas::remove_effect(instance_id, app).await
        }
        DaemonCommand::CanvasSetDefaultEffect { instance_id } => {
            lighting::usecases::canvas::set_default_effect(instance_id, app).await
        }
        DaemonCommand::CanvasPlaceZone {
            device_id,
            zone_id,
            x,
            y,
            w,
            h,
            rotation,
            effect,
            sampling_mode,
        } => {
            lighting::usecases::canvas::place_zone(
                device_id,
                zone_id,
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
        DaemonCommand::CanvasRemoveZone { device_id, zone_id } => {
            lighting::usecases::canvas::remove_zone(device_id, zone_id, app).await
        }
        DaemonCommand::CanvasMoveZone {
            device_id,
            zone_id,
            x,
            y,
            w,
            h,
            rotation,
            effect,
            sampling_mode,
        } => {
            lighting::usecases::canvas::move_zone(
                device_id,
                zone_id,
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
        DaemonCommand::CanvasSetSampleRadius { radius } => {
            lighting::usecases::canvas::set_sample_radius(radius, app).await
        }
        DaemonCommand::CanvasStop => lighting::usecases::canvas::stop(app).await,
        DaemonCommand::CanvasSubscribe => {
            if client.try_subscribe_canvas() {
                let c2 = client.clone();
                tokio::spawn(engine_subscribe_loop(
                    app,
                    c2,
                    |a| a.lighting.engine().map(|e| e.subscribe()),
                    "canvas_frame",
                ));
            }
            Ok(())
        }

        DaemonCommand::LcdEngineSetTemplate {
            device_id,
            template_id,
            params,
        } => lcd::usecases::engine::set_template(device_id, template_id, params, app).await,
        DaemonCommand::LcdEngineDeactivate { device_id } => {
            lcd::usecases::engine::deactivate(device_id, app).await
        }
        DaemonCommand::LcdEngineSubscribe => {
            client.touch_lcd_preview();
            if client.try_subscribe_lcd() {
                tokio::spawn(lcd_preview_forward_loop(app, client.clone()));
            }
            Ok(())
        }
        DaemonCommand::SaveLcdTemplate { name, def } => {
            lcd::usecases::templates::save_template(name, def, app).await
        }
        DaemonCommand::LoadLcdTemplate { name } => {
            lcd::usecases::templates::load_template(name, client).await
        }
        DaemonCommand::DeleteLcdTemplate { name } => {
            lcd::usecases::templates::delete_template(name, app).await
        }
        DaemonCommand::RenderLcdEditor {
            device_id,
            def,
            known,
        } => lcd::usecases::editor::render(device_id, def, known, app, client).await,

        DaemonCommand::SaveCustomEffect { name, params } => {
            lighting::usecases::custom_effects::save_custom_effect(name, params, app).await
        }
        DaemonCommand::DeleteCustomEffect { name } => {
            lighting::usecases::custom_effects::delete_custom_effect(name, app).await
        }

        DaemonCommand::ReceiverStartPairing { id, timeout_secs } => {
            registry::usecases::receiver::start_pairing(id, timeout_secs, app).await
        }
        DaemonCommand::ReceiverStopPairing { id } => {
            registry::usecases::receiver::stop_pairing(id, app).await
        }
        DaemonCommand::ReceiverUnpair { id, slot } => {
            registry::usecases::receiver::unpair(id, slot, app).await
        }

        DaemonCommand::ListRunningApps => profiles::running_apps::list(client).await,
        DaemonCommand::GetDebugInfo => registry::usecases::debug::get_debug_info(client, app).await,
        DaemonCommand::SetEngineConfig {
            engine,
            enabled,
            tick_ms,
            fps,
            failsafe_duty,
        } => {
            registry::usecases::settings::set_engine_config(
                engine,
                enabled,
                tick_ms,
                fps,
                failsafe_duty,
                app,
            )
            .await
        }
        DaemonCommand::Ping => Ok(()),
        DaemonCommand::Shutdown => {
            log::info!("Shutdown requested by a client");
            request_shutdown(&app);
            Ok(())
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
/// Also used by the idle-shutdown watcher ([`crate::lifecycle`]) — a client
/// `Shutdown` command and "no client has been connected for a while" both
/// route through the same stop path.
pub(crate) fn request_shutdown(app: &Arc<AppState>) {
    if app.is_service_worker {
        #[cfg(windows)]
        match crate::platform::service::request_stop() {
            Ok(()) => {
                log::info!("asked the SCM to stop the HalodDaemon service");
                return;
            }
            Err(e) => log::error!("failed to stop service ({e}); exiting worker directly"),
        }
    }
    app.shutdown.notify_one();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ipc::ClientHandle;
    use std::time::Duration;
    use tokio::sync::mpsc;

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
            app,
        )
        .await;

        let frame = rx.recv().await.expect("an error frame");
        let reply: Value = serde_json::from_slice(&frame[5..]).unwrap();
        assert_eq!(reply["type"], "error");
        assert_eq!(reply["request_id"], "req-42");
    }

    #[test]
    fn command_target_routes_device_commands_and_leaves_globals_unlocked() {
        use halod_shared::types::{RgbColor, RgbState};
        // Device-scoped commands map to their device id (by whichever field
        // names it) so same-device work serializes on one lock.
        assert_eq!(
            command_target(&DaemonCommand::RgbApply {
                id: "kbd".into(),
                state: RgbState::Static {
                    color: RgbColor { r: 0, g: 0, b: 0 }
                },
            }),
            Some("kbd")
        );
        assert_eq!(
            command_target(&DaemonCommand::RemoveFanCurve {
                fan_id: "fan0".into()
            }),
            Some("fan0")
        );
        assert_eq!(
            command_target(&DaemonCommand::CanvasMoveZone {
                device_id: "ram".into(),
                zone_id: "leds".into(),
                x: 0.0,
                y: 0.0,
                w: None,
                h: None,
                rotation: None,
                effect: None,
                sampling_mode: None,
            }),
            Some("ram")
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
        assert!(!app.is_service_worker, "default daemon is not a worker");

        request_shutdown(&app);

        tokio::time::timeout(Duration::from_millis(200), app.shutdown.notified())
            .await
            .expect("shutdown should have been signalled");
    }

    /// Test helper: `AppState` with a live `LcdEngine`.
    fn app_with_lcd_engine() -> (Arc<AppState>, Arc<crate::lcd::engine::LcdEngine>) {
        use crate::lcd::engine::{video::VideoEngine, LcdEngine};
        use crate::state::EngineRunConfig;

        let app = Arc::new(AppState::new(Config::default()));
        let engine = LcdEngine::new(Arc::clone(&app));
        app.lcd.set_engine(
            Arc::clone(&engine),
            VideoEngine::new(Arc::clone(&app), engine.frame_sender()),
            tokio::sync::watch::channel(EngineRunConfig::lcd(
                &crate::config::GlobalConfig::default(),
            ))
            .0,
        );
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

    #[tokio::test]
    async fn lcd_preview_forwarder_exits_when_client_disconnects_while_paused() {
        let (app, _engine) = app_with_lcd_engine();
        let (tx, rx) = mpsc::channel::<std::sync::Arc<Vec<u8>>>(8);
        let client = ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        };
        // Lease already stale (default keepalive is `Instant::now()` at
        // construction, so advance past the lease before dropping the reader).
        tokio::time::sleep(LCD_PREVIEW_LEASE + Duration::from_millis(50)).await;
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

        let frame = crate::lcd::engine::encode_wire_frame("lcd0", 1, "").expect("wire encode");
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
