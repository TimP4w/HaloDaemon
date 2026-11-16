// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::bus::{BusEvent, BusEventReplay, BusSnapshot, BusTransaction};
use crate::debug_info::DebugInfo;
use crate::lcd_custom::{CustomTemplateDef, LcdEditorRender};
use crate::types::{
    CanvasFrame, CatalogErrorCode, LcdEngineFrame, LcdUploadProgress, RunningApp, SerialPortInfo,
    UdevRulesStatus,
};

/// Frames the daemon sends to an IPC client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonMessage {
    Pong,
    Error(IpcError),
    BusSnapshot {
        data: BusSnapshot,
    },
    BusTransaction {
        data: BusTransaction,
    },
    BusEvent {
        data: BusEvent,
    },
    BusEventReplay {
        data: BusEventReplay,
    },
    DebugInfo {
        data: DebugInfo,
    },
    ImageUploaded,
    LcdUploadProgress {
        data: LcdUploadProgress,
    },
    LcdEngineFrame {
        data: LcdEngineFrame,
    },
    LcdEditorRender {
        data: LcdEditorRender,
    },
    CanvasFrame {
        data: Arc<CanvasFrame>,
    },
    RunningAppsList {
        apps: Vec<RunningApp>,
    },
    LcdImages {
        files: Vec<LcdImageFile>,
    },
    PluginAsset {
        plugin_id: String,
        name: String,
        data_b64: String,
    },
    LcdWidgetIcon {
        catalog_id: String,
        data_b64: String,
    },
    UdevRulesStatus {
        data: UdevRulesStatus,
    },
    SerialPorts {
        ports: Vec<SerialPortInfo>,
    },
    LcdTemplate {
        name: String,
        def: CustomTemplateDef,
    },
    LcdPluginPreset {
        catalog_id: String,
        def: CustomTemplateDef,
    },
}

/// A failed command. `handled` means the daemon already surfaced it as a notification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<CatalogErrorCode>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub handled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LcdImageFile {
    pub name: String,
    pub size_bytes: u64,
}
