use halod_protocol::commands::DaemonCommand;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;

use crate::ipc::ClientHandle;
use crate::state::AppState;
use crate::usecases;

async fn engine_subscribe_loop<T, F>(
    app: Arc<AppState>,
    client: ClientHandle,
    get_receiver: F,
    frame_type: &'static str,
) where
    T: Serialize + Send + Sync + 'static,
    F: Fn(&crate::state::Engines) -> tokio::sync::broadcast::Receiver<Arc<T>>,
{
    let mut rx = loop {
        if let Some(engines) = app.engines.get() {
            break get_receiver(engines);
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
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
    let cmd = msg["type"].as_str().unwrap_or("").to_string();

    // `ping` is a connection-level ack handled before command dispatch.
    if cmd == "ping" {
        client.send_json(&json!({"type": "pong"}));
        return;
    }

    let result = match serde_json::from_value::<DaemonCommand>(msg.clone()) {
        Ok(typed) => dispatch(typed, msg, client.clone(), app).await,
        Err(_) => Err(anyhow::anyhow!("unknown command: {cmd}")),
    };

    if let Err(e) = result {
        log::warn!("command '{cmd}' failed: {e}");
        client.send_json(&json!({"type": "error", "message": e.to_string()}));
    }
}

/// Dispatch a command that has been successfully deserialised into a `DaemonCommand`.
///
/// The original raw `msg` Value is threaded through because the use-case fns still
/// parse their arguments directly from the JSON; the typed dispatch guarantees the
/// wire format is valid before they run.
async fn dispatch(
    cmd: DaemonCommand,
    msg: Value,
    client: ClientHandle,
    app: Arc<AppState>,
) -> anyhow::Result<()> {
    match cmd {
        DaemonCommand::SetRange { .. } => usecases::range::set_range(msg, app).await,
        DaemonCommand::SetChoice { .. } => usecases::choice::set_choice(msg, app).await,
        DaemonCommand::SetBoolean { .. } => usecases::boolean::set_boolean(msg, app).await,
        DaemonCommand::TriggerAction { .. } => usecases::action::trigger_action(msg, app).await,
        DaemonCommand::AddProfile { .. } => usecases::profiles::add_profile(msg, app).await,
        DaemonCommand::RenameProfile { .. } => usecases::profiles::rename_profile(msg, app).await,
        DaemonCommand::RemoveProfile { .. } => usecases::profiles::remove_profile(msg, app).await,
        DaemonCommand::SwitchProfile { .. } => usecases::profiles::switch_profile(msg, app).await,
        DaemonCommand::AddAppRule { .. } => usecases::app_rules::add(msg, app).await,
        DaemonCommand::UpdateAppRule { .. } => usecases::app_rules::update(msg, app).await,
        DaemonCommand::RemoveAppRule { .. } => usecases::app_rules::remove(msg, app).await,
        DaemonCommand::Rediscover => usecases::settings::rediscover(app).await,
        DaemonCommand::SetLogLevel { .. } => usecases::settings::set_log_level(msg, app).await,
        DaemonCommand::SetUiConfig { .. } => usecases::settings::set_ui_config(msg, app).await,
        DaemonCommand::SetFanFailsafeDuty { .. } => {
            usecases::settings::set_fan_failsafe_duty(msg, app).await
        }
        DaemonCommand::ResetAllButtonMappings { .. } => {
            usecases::key_remap::reset_all_button_mappings(msg, app).await
        }
        DaemonCommand::ResetButtonMapping { .. } => {
            usecases::key_remap::reset_button_mapping(msg, app).await
        }
        DaemonCommand::SetEqPreset { .. } => usecases::equalizer::set_eq_preset(msg, app).await,
        DaemonCommand::SetEqBands { .. } => usecases::equalizer::set_eq_bands(msg, app).await,
        DaemonCommand::SetDpiSteps { .. } => usecases::dpi::set_dpi_steps(msg, app).await,
        DaemonCommand::SetDeviceVisibility { device_id, state } => {
            usecases::visibility::set_device_visibility(device_id, state, app).await
        }
        DaemonCommand::SetSensorVisibility { sensor_id, state } => {
            usecases::visibility::set_sensor_visibility(sensor_id, state, app).await
        }
        DaemonCommand::SetDeviceName { device_id, name } => {
            usecases::rename::set_device_name(device_id, name, app).await
        }

        DaemonCommand::SetFanSpeed { .. } => usecases::fan::set_fan_speed(msg, app).await,
        DaemonCommand::SetFanCurvePoints { .. } => {
            usecases::fan_curve::set_fan_curve_points(msg, app).await
        }
        DaemonCommand::SetFanCurvePreset { .. } => {
            usecases::fan_curve::set_fan_curve_preset(msg, app).await
        }
        DaemonCommand::RemoveFanCurve { .. } => {
            usecases::fan_curve::remove_fan_curve(msg, app).await
        }

        DaemonCommand::RgbApply { .. } => usecases::rgb::rgb_apply(msg, app).await,
        DaemonCommand::RgbSetZoneTransform { .. } => {
            usecases::rgb::set_zone_transform(msg, app).await
        }
        DaemonCommand::RgbChainAddLink { .. } => {
            usecases::chain::rgb_chain_add_link(msg, app).await
        }
        DaemonCommand::RgbChainRemoveLink { .. } => {
            usecases::chain::rgb_chain_remove_link(msg, app).await
        }
        DaemonCommand::RgbChainReorderLink { .. } => {
            usecases::chain::rgb_chain_reorder_link(msg, app).await
        }
        DaemonCommand::RgbChainDetectChannel { .. } => {
            usecases::chain::rgb_chain_detect_channel(msg, app).await
        }

        DaemonCommand::SetButtonMapping { .. } => {
            usecases::key_remap::set_button_mapping(msg, app).await
        }
        DaemonCommand::SetSoftwareDpiSteps { .. } => {
            usecases::key_remap::set_software_dpi_steps(msg, app).await
        }

        DaemonCommand::OnboardProfileSwitch { .. } => {
            usecases::onboard_profiles::switch_onboard_profile(msg, app).await
        }
        DaemonCommand::OnboardProfileRestore { .. } => {
            usecases::onboard_profiles::restore_onboard_profile(msg, app).await
        }
        DaemonCommand::OnboardProfileSetEnabled { .. } => {
            usecases::onboard_profiles::set_onboard_profile_enabled(msg, app).await
        }

        DaemonCommand::SetScreenImage { .. } => {
            usecases::lcd::set_screen_image(msg, app, client).await
        }
        DaemonCommand::SetScreenImageFromLibrary { .. } => {
            usecases::lcd::set_screen_image_from_library(msg, app, client).await
        }
        DaemonCommand::SetScreenRotation { .. } => {
            usecases::lcd::set_screen_rotation(msg, app).await
        }
        DaemonCommand::SetScreenBrightness { .. } => {
            usecases::lcd::set_screen_brightness(msg, app).await
        }
        DaemonCommand::SetScreenDefault { .. } => usecases::lcd::set_screen_default(msg, app).await,
        DaemonCommand::ListLcdImages => usecases::lcd::list_lcd_images(msg, client).await,
        DaemonCommand::DeleteLcdImage { .. } => usecases::lcd::delete_lcd_image(msg).await,

        DaemonCommand::CanvasSetEffect { .. } => usecases::canvas::set_effect(msg, app).await,
        DaemonCommand::CanvasPlaceZone { .. } => usecases::canvas::place_zone(msg, app).await,
        DaemonCommand::CanvasRemoveZone { .. } => usecases::canvas::remove_zone(msg, app).await,
        DaemonCommand::CanvasMoveZone { .. } => usecases::canvas::move_zone(msg, app).await,
        DaemonCommand::CanvasSetSampleRadius { .. } => {
            usecases::canvas::set_sample_radius(msg, app).await
        }
        DaemonCommand::CanvasSubscribe => {
            tokio::spawn(engine_subscribe_loop(
                app,
                client,
                |e| e.canvas.subscribe(),
                "canvas_frame",
            ));
            Ok(())
        }

        DaemonCommand::LcdEngineSetTemplate { .. } => {
            usecases::lcd_engine::set_template(msg, app).await
        }
        DaemonCommand::LcdEngineDeactivate { .. } => {
            usecases::lcd_engine::deactivate(msg, app).await
        }
        DaemonCommand::LcdEngineSubscribe => {
            tokio::spawn(engine_subscribe_loop(
                app,
                client,
                |e| e.lcd.subscribe(),
                "lcd_engine_frame",
            ));
            Ok(())
        }

        DaemonCommand::ListRunningApps => usecases::running_apps::list(msg, client, app).await,
        DaemonCommand::GetDebugInfo => usecases::debug::get_debug_info(msg, client, app).await,
        DaemonCommand::SetEngineConfig { .. } => {
            usecases::settings::set_engine_config(msg, app).await
        }
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
fn request_shutdown(app: &Arc<AppState>) {
    if app.is_service_worker {
        #[cfg(windows)]
        match crate::service::request_stop() {
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
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_request_signals_a_plain_daemon() {
        // A non-service (dev / plain) daemon must shut down gracefully: an IPC
        // `shutdown` must fire the `shutdown` Notify the run loop awaits.
        let app = Arc::new(AppState::new(Config::default()));
        assert!(!app.is_service_worker, "default daemon is not a worker");

        request_shutdown(&app);

        tokio::time::timeout(Duration::from_millis(200), app.shutdown.notified())
            .await
            .expect("shutdown should have been signalled");
    }
}
