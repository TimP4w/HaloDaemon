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
)
where
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
                if client.tx.is_closed() { break; }
                client.send_json(&json!({"type": frame_type, "data": *frame}));
            }
            Err(RecvError::Lagged(n)) => log::debug!("{frame_type} subscriber lagged {n} frame(s)"),
            Err(RecvError::Closed) => break,
        }
    }
}

pub async fn dispatch(msg: Value, client: ClientHandle, app: Arc<AppState>) {
    let cmd = msg["type"].as_str().unwrap_or("").to_string();

    match cmd.as_str() {
        "ping" => {
            client.send_json(&json!({"type": "pong"}));
            return;
        }
        _ => {}
    }

    let result = route(&cmd, msg.clone(), client.clone(), app.clone()).await;

    if let Err(e) = result {
        log::warn!("command '{cmd}' failed: {e}");
        client.send_json(&json!({"type": "error", "message": e.to_string()}));
    }
}

/// Dispatch a command that has been successfully deserialised into a `DaemonCommand`.
///
/// The original raw `msg` Value is threaded through for use-cases that still
/// parse their arguments directly from the JSON (the use-case fns take `Value`).
/// That allows incremental migration: typed dispatch ensures the wire format is
/// valid; the use-case itself can be updated to accept a typed arg later.
async fn dispatch_typed(
    cmd: DaemonCommand,
    msg: Value,
    _client: ClientHandle,
    app: Arc<AppState>,
) -> anyhow::Result<()> {
    match cmd {
        DaemonCommand::SetRange { .. }    => usecases::range::set_range(msg, app).await,
        DaemonCommand::SetChoice { .. }   => usecases::choice::set_choice(msg, app).await,
        DaemonCommand::SetBoolean { .. }  => usecases::boolean::set_boolean(msg, app).await,
        DaemonCommand::TriggerAction { .. } => usecases::action::trigger_action(msg, app).await,
        DaemonCommand::AddProfile { .. }    => usecases::profiles::add_profile(msg, app).await,
        DaemonCommand::RenameProfile { .. } => usecases::profiles::rename_profile(msg, app).await,
        DaemonCommand::RemoveProfile { .. } => usecases::profiles::remove_profile(msg, app).await,
        DaemonCommand::SwitchProfile { .. } => usecases::profiles::switch_profile(msg, app).await,
        DaemonCommand::AddAppRule { .. }    => usecases::app_rules::add(msg, app).await,
        DaemonCommand::UpdateAppRule { .. } => usecases::app_rules::update(msg, app).await,
        DaemonCommand::RemoveAppRule { .. } => usecases::app_rules::remove(msg, app).await,
        DaemonCommand::Rediscover           => usecases::settings::rediscover(app).await,
        DaemonCommand::SetLogLevel { .. }   => usecases::settings::set_log_level(msg, app).await,
        DaemonCommand::SetUiConfig { .. }   => usecases::settings::set_ui_config(msg, app).await,
        DaemonCommand::SetFanFailsafeDuty { .. } => usecases::settings::set_fan_failsafe_duty(msg, app).await,
        DaemonCommand::ResetAllButtonMappings { .. } => usecases::key_remap::reset_all_button_mappings(msg, app).await,
        DaemonCommand::ResetButtonMapping { .. }     => usecases::key_remap::reset_button_mapping(msg, app).await,
        DaemonCommand::SetEqPreset { .. }  => usecases::equalizer::set_eq_preset(msg, app).await,
        DaemonCommand::SetEqBands { .. }   => usecases::equalizer::set_eq_bands(msg, app).await,
        DaemonCommand::SetDpiSteps { .. }  => usecases::dpi::set_dpi_steps(msg, app).await,
        DaemonCommand::SetDeviceVisibility { .. } => usecases::visibility::set_device_visibility(msg, app).await,
        DaemonCommand::SetSensorVisibility { .. } => usecases::visibility::set_sensor_visibility(msg, app).await,
        DaemonCommand::SetDeviceName { .. }        => usecases::rename::set_device_name(msg, app).await,
    }
}

async fn route(
    cmd: &str,
    msg: Value,
    client: ClientHandle,
    app: Arc<AppState>,
) -> anyhow::Result<()> {
    // Try typed dispatch first for commands that are represented in DaemonCommand.
    // Falls through to the string-match path for commands not yet migrated.
    if let Ok(typed) = serde_json::from_value::<DaemonCommand>(msg.clone()) {
        return dispatch_typed(typed, msg, client, app).await;
    }

    match cmd {
        "set_fan_speed" | "set_pump_duty" => usecases::fan::set_fan_speed(msg, app).await,
        "set_fan_curve_points"  => usecases::fan_curve::set_fan_curve_points(msg, app).await,
        "set_fan_curve_preset"  => usecases::fan_curve::set_fan_curve_preset(msg, app).await,
        "remove_fan_curve"      => usecases::fan_curve::remove_fan_curve(msg, app).await,
        "list_running_apps" => usecases::running_apps::list(msg, client, app).await,
        "rgb_apply"             => usecases::rgb::rgb_apply(msg, app).await,
        "rgb_set_zone_transform" => usecases::rgb::set_zone_transform(msg, app).await,
        "rgb_chain_add_link"        => usecases::chain::rgb_chain_add_link(msg, app).await,
        "rgb_chain_remove_link"     => usecases::chain::rgb_chain_remove_link(msg, app).await,
        "rgb_chain_reorder_link"    => usecases::chain::rgb_chain_reorder_link(msg, app).await,
        "rgb_chain_detect_channel"  => usecases::chain::rgb_chain_detect_channel(msg, app).await,
        "set_button_mapping"        => usecases::key_remap::set_button_mapping(msg, app).await,
        "set_software_dpi_steps"    => usecases::key_remap::set_software_dpi_steps(msg, app).await,
        "onboard_profile_switch"      => usecases::onboard_profiles::switch_onboard_profile(msg, app).await,
        "onboard_profile_restore"     => usecases::onboard_profiles::restore_onboard_profile(msg, app).await,
        "onboard_profile_set_enabled" => usecases::onboard_profiles::set_onboard_profile_enabled(msg, app).await,
        "set_screen_image"            => usecases::lcd::set_screen_image(msg, app, client.clone()).await,
        "set_screen_image_from_library" => usecases::lcd::set_screen_image_from_library(msg, app, client.clone()).await,
        "set_screen_rotation"         => usecases::lcd::set_screen_rotation(msg, app).await,
        "set_screen_brightness"       => usecases::lcd::set_screen_brightness(msg, app).await,
        "set_screen_default"          => usecases::lcd::set_screen_default(msg, app).await,
        "list_lcd_images"             => usecases::lcd::list_lcd_images(msg, client).await,
        "delete_lcd_image"            => usecases::lcd::delete_lcd_image(msg).await,
        "canvas_set_effect"     => usecases::canvas::set_effect(msg, app).await,
        "canvas_place_zone"     => usecases::canvas::place_zone(msg, app).await,
        "canvas_remove_zone"    => usecases::canvas::remove_zone(msg, app).await,
        "canvas_move_zone"          => usecases::canvas::move_zone(msg, app).await,
        "canvas_set_sample_radius"  => usecases::canvas::set_sample_radius(msg, app).await,
        "canvas_subscribe"      => {
            tokio::spawn(engine_subscribe_loop(app, client, |e| e.canvas.subscribe(), "canvas_frame"));
            Ok(())
        }
        "shutdown" => {
            log::info!("Shutdown requested by a client");
            request_shutdown(&app);
            Ok(())
        }
        "get_debug_info"          => usecases::debug::get_debug_info(msg, client, app).await,
        "set_engine_config"       => usecases::settings::set_engine_config(msg, app).await,
        "lcd_engine_set_template" => usecases::lcd_engine::set_template(msg, app).await,
        "lcd_engine_deactivate"   => usecases::lcd_engine::deactivate(msg, app).await,
        "lcd_engine_subscribe"    => {
            tokio::spawn(engine_subscribe_loop(app, client, |e| e.lcd.subscribe(), "lcd_engine_frame"));
            Ok(())
        }
        other => anyhow::bail!("unknown command: {other}"),
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
