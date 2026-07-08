// SPDX-License-Identifier: GPL-3.0-or-later
//! System/daemon-lifecycle actions: shutdown, rediscovery, debug snapshot,
//! settings (language, log level, UI config, tours, engine toggles).

use halod_shared::commands::{DaemonCommand, EngineKind};

use crate::runtime::ipc::{self, CommandTx};

pub fn shutdown(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::Shutdown);
}

pub fn rediscover(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::Rediscover);
}

pub fn get_debug_info(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::GetDebugInfo);
}

pub fn set_language(cmd: &CommandTx, lang: &str) {
    ipc::send(
        cmd,
        DaemonCommand::SetLanguage {
            lang: lang.to_string(),
        },
    );
}

pub fn set_log_level(cmd: &CommandTx, level: &str) {
    ipc::send(
        cmd,
        DaemonCommand::SetLogLevel {
            level: level.to_string(),
        },
    );
}

pub fn set_ui_config(
    cmd: &CommandTx,
    close_to_tray: bool,
    suppress_dependency_warning: bool,
    hide_window_controls: bool,
) {
    ipc::send(
        cmd,
        DaemonCommand::SetUiConfig {
            close_to_tray,
            suppress_dependency_warning,
            hide_window_controls,
        },
    );
}

pub fn reset_tours_seen(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::ResetToursSeen);
}

pub fn mark_tour_seen(cmd: &CommandTx, tour: &str) {
    ipc::send(
        cmd,
        DaemonCommand::MarkTourSeen {
            tour: tour.to_string(),
        },
    );
}

pub fn set_engine_fps(cmd: &CommandTx, kind: EngineKind, fps: u64) {
    ipc::send(cmd, DaemonCommand::set_engine_fps(kind, fps));
}

pub fn set_engine_enabled(cmd: &CommandTx, kind: EngineKind, enabled: bool) {
    ipc::send(cmd, DaemonCommand::set_engine_enabled(kind, enabled));
}
