// SPDX-License-Identifier: GPL-3.0-or-later
//! LCD screen and template-editor actions: media library, screen mode,
//! templates, and the editor's render/preview requests.

use std::collections::HashMap;

use halod_shared::commands::DaemonCommand;
use halod_shared::lcd_custom::CustomTemplateDef;
use halod_shared::types::{EffectParamValue, ScreenRotation};

use crate::runtime::ipc::{self, CommandTx};

pub fn list_lcd_images(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::ListLcdImages);
}

pub fn lcd_engine_subscribe(cmd: &CommandTx) {
    ipc::send(cmd, DaemonCommand::LcdEngineSubscribe);
}

pub fn set_screen_video(cmd: &CommandTx, id: &str, path: String) {
    ipc::send(
        cmd,
        DaemonCommand::SetScreenVideo {
            id: id.to_string(),
            path,
        },
    );
}

pub fn set_screen_rotation(cmd: &CommandTx, id: &str, rotation: ScreenRotation) {
    ipc::send(
        cmd,
        DaemonCommand::SetScreenRotation {
            id: id.to_string(),
            rotation,
        },
    );
}

pub fn set_screen_default(cmd: &CommandTx, id: &str) {
    ipc::send(cmd, DaemonCommand::SetScreenDefault { id: id.to_string() });
}

pub fn set_screen_raw_streaming(cmd: &CommandTx, id: &str, enabled: bool) {
    ipc::send(
        cmd,
        DaemonCommand::SetScreenRawStreaming {
            id: id.to_string(),
            enabled,
        },
    );
}

pub fn delete_lcd_image(cmd: &CommandTx, filename: &str) {
    ipc::send(
        cmd,
        DaemonCommand::DeleteLcdImage {
            filename: filename.to_string(),
        },
    );
}

pub fn lcd_engine_deactivate(cmd: &CommandTx, device_id: &str) {
    ipc::send(
        cmd,
        DaemonCommand::LcdEngineDeactivate {
            device_id: device_id.to_string(),
        },
    );
}

pub fn set_screen_image_from_library(cmd: &CommandTx, id: &str, filename: &str) {
    ipc::send(
        cmd,
        DaemonCommand::SetScreenImageFromLibrary {
            id: id.to_string(),
            filename: filename.to_string(),
            request_id: None,
        },
    );
}

pub fn set_screen_image(cmd: &CommandTx, id: &str, data_b64: String) {
    ipc::send(
        cmd,
        DaemonCommand::SetScreenImage {
            id: id.to_string(),
            data_b64,
            request_id: None,
        },
    );
}

pub fn lcd_engine_set_template(
    cmd: &CommandTx,
    device_id: &str,
    template_id: &str,
    params: HashMap<String, EffectParamValue>,
) {
    ipc::send(
        cmd,
        DaemonCommand::LcdEngineSetTemplate {
            device_id: device_id.to_string(),
            template_id: template_id.to_string(),
            params,
        },
    );
}

pub fn save_lcd_template(cmd: &CommandTx, name: &str, def: CustomTemplateDef) {
    ipc::send(
        cmd,
        DaemonCommand::SaveLcdTemplate {
            name: name.to_string(),
            def,
        },
    );
}

pub fn load_lcd_template(cmd: &CommandTx, name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::LoadLcdTemplate {
            name: name.to_string(),
        },
    );
}

pub fn delete_lcd_template(cmd: &CommandTx, name: &str) {
    ipc::send(
        cmd,
        DaemonCommand::DeleteLcdTemplate {
            name: name.to_string(),
        },
    );
}

pub fn render_lcd_editor(
    cmd: &CommandTx,
    device_id: &str,
    def: CustomTemplateDef,
    known: HashMap<String, u64>,
) {
    ipc::send(
        cmd,
        DaemonCommand::RenderLcdEditor {
            device_id: device_id.to_string(),
            def,
            known,
        },
    );
}
