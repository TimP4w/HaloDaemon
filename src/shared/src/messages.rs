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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CatalogErrorCode, LcdUploadStage};
    use serde_json::json;

    fn error(
        code: Option<CatalogErrorCode>,
        handled: bool,
        request_id: Option<&str>,
    ) -> DaemonMessage {
        DaemonMessage::Error(IpcError {
            message: "failed".into(),
            code,
            handled,
            request_id: request_id.map(str::to_owned),
        })
    }

    #[test]
    fn wire_formats_are_stable_and_round_trip() {
        let cases = [
            (DaemonMessage::Pong, json!({"type": "pong"})),
            (
                DaemonMessage::ImageUploaded,
                json!({"type": "image_uploaded"}),
            ),
            (
                error(None, false, None),
                json!({"type": "error", "message": "failed"}),
            ),
            (
                error(Some(CatalogErrorCode::Stale), true, Some("req-1")),
                json!({"type": "error", "message": "failed", "code": "stale", "handled": true, "request_id": "req-1"}),
            ),
            (
                DaemonMessage::LcdUploadProgress {
                    data: LcdUploadProgress {
                        device_id: "lcd".into(),
                        stage: LcdUploadStage::Processing,
                        percent: Some(42),
                    },
                },
                json!({"type": "lcd_upload_progress", "data": {"device_id": "lcd", "stage": "processing", "percent": 42}}),
            ),
            (
                DaemonMessage::CanvasFrame {
                    data: Arc::new(CanvasFrame {
                        frame_id: 7,
                        timestamp_ms: 9,
                        canvas_srgb_b64: "AA==".into(),
                        canvas_w: 1,
                        canvas_h: 1,
                        led_colors: Vec::new(),
                    }),
                },
                json!({"type": "canvas_frame", "data": {"frame_id": 7, "timestamp_ms": 9, "canvas_srgb_b64": "AA==", "canvas_w": 1, "canvas_h": 1, "led_colors": []}}),
            ),
            (
                DaemonMessage::RunningAppsList {
                    apps: vec![RunningApp {
                        process_name: "game".into(),
                        display_name: "Game".into(),
                        icon_name: "game".into(),
                    }],
                },
                json!({"type": "running_apps_list", "apps": [{"process_name": "game", "display_name": "Game", "icon_name": "game"}]}),
            ),
            (
                DaemonMessage::LcdImages {
                    files: vec![LcdImageFile {
                        name: "a.png".into(),
                        size_bytes: 3,
                    }],
                },
                json!({"type": "lcd_images", "files": [{"name": "a.png", "size_bytes": 3}]}),
            ),
            (
                DaemonMessage::PluginAsset {
                    plugin_id: "acme".into(),
                    name: "logo.png".into(),
                    data_b64: "AA==".into(),
                },
                json!({"type": "plugin_asset", "plugin_id": "acme", "name": "logo.png", "data_b64": "AA=="}),
            ),
            (
                DaemonMessage::LcdWidgetIcon {
                    catalog_id: "clock".into(),
                    data_b64: "AA==".into(),
                },
                json!({"type": "lcd_widget_icon", "catalog_id": "clock", "data_b64": "AA=="}),
            ),
            (
                DaemonMessage::SerialPorts {
                    ports: vec![SerialPortInfo {
                        value: "/dev/ttyACM0".into(),
                        label: "ACM0".into(),
                    }],
                },
                json!({"type": "serial_ports", "ports": [{"value": "/dev/ttyACM0", "label": "ACM0"}]}),
            ),
        ];
        for (message, wire) in cases {
            assert_eq!(serde_json::to_value(&message).unwrap(), wire);
            let parsed: DaemonMessage = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&parsed).unwrap(), wire);
        }
    }
}
