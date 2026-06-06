use halod_protocol::commands::DaemonCommand;

/// All user-initiated actions that reach the daemon.
/// `Store::dispatch` translates each variant via `to_json()`.
pub enum Command {
    // ── Fan curve ──────────────────────────────────────────────────────────
    SetFanCurvePreset {
        fan_id:    String,
        preset_id: String,
        sensor_id: Option<String>,
    },
    SetFanCurvePoints {
        fan_id:    String,
        points:    Vec<[f64; 2]>,
        sensor_id: Option<String>,
    },
    SetFanFailsafeDuty { fan_id: String, duty: u8 },

    // ── Equalizer ──────────────────────────────────────────────────────────
    SetEqPreset { device_id: String, preset_index: usize },
    SetEqBands  { device_id: String, values: Vec<f32> },

    // ── DPI ────────────────────────────────────────────────────────────────
    SetDpiSteps { device_id: String, steps: Vec<u32> },

    // ── Lighting ───────────────────────────────────────────────────────────
    RgbApply { device_id: String, state: serde_json::Value },

    // ── Onboard profiles ───────────────────────────────────────────────────
    OnboardProfileSwitch(serde_json::Value),
    OnboardProfileRestore(serde_json::Value),
    OnboardProfileSetEnabled(serde_json::Value),

    // ── Device capability settings ─────────────────────────────────────────
    SetChoice  { device_id: String, key: String, selected: usize },
    SetRange   { device_id: String, key: String, value: i32 },
    SetBoolean { device_id: String, key: String, value: bool },
    TriggerAction { device_id: String, key: String },

    // ── Key remap ──────────────────────────────────────────────────────────
    ResetAllButtonMappings { device_id: String },
    ResetButtonMapping     { device_id: String, cid: u16 },
    SetButtonMapping { device_id: String, mapping: serde_json::Value },

    // ── Canvas / LCD engine ────────────────────────────────────────────────
    /// Pass-through for canvas operations and LCD engine commands.
    CanvasOp(serde_json::Value),

    // ── Profile management ─────────────────────────────────────────────────
    AddProfile    { name: String },
    RenameProfile { old_name: String, new_name: String },
    RemoveProfile { name: String },
    SwitchProfile { name: String },

    // ── App rules ──────────────────────────────────────────────────────────
    AddAppRule { process_names: Vec<String>, profile: String, enabled: bool },
    UpdateAppRule { index: usize, process_names: Vec<String>, profile: String, enabled: bool },
    RemoveAppRule { index: usize },

    // ── Engine config / global settings ────────────────────────────────────
    SetEngineConfig { engine: String, config: serde_json::Value },
    Rediscover,
    SetLogLevel { level: String },
    SetUiConfig { close_to_tray: bool },
}

impl Command {
    pub fn to_json(self) -> serde_json::Value {
        match self {
            Self::SetFanCurvePreset { fan_id, preset_id, sensor_id } => {
                let mut m = serde_json::json!({
                    "type":   "set_fan_curve_preset",
                    "fan_id": fan_id,
                    "preset": preset_id,
                });
                if let Some(sid) = sensor_id { m["sensor_id"] = sid.into(); }
                m
            }
            Self::SetFanCurvePoints { fan_id, points, sensor_id } => {
                let mut m = serde_json::json!({
                    "type":   "set_fan_curve_points",
                    "fan_id": fan_id,
                    "points": points,
                });
                if let Some(sid) = sensor_id { m["sensor_id"] = sid.into(); }
                m
            }
            Self::SetFanFailsafeDuty { fan_id, duty } =>
                serde_json::to_value(DaemonCommand::SetFanFailsafeDuty { fan_id, duty })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetEqPreset { device_id, preset_index } =>
                serde_json::to_value(DaemonCommand::SetEqPreset { id: device_id, preset_index })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetEqBands { device_id, values } =>
                serde_json::to_value(DaemonCommand::SetEqBands { id: device_id, values })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetDpiSteps { device_id, steps } =>
                serde_json::to_value(DaemonCommand::SetDpiSteps { id: device_id, steps })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::RgbApply { device_id, state } => serde_json::json!({
                "type":  "rgb_apply",
                "id":    device_id,
                "state": state,
            }),
            Self::OnboardProfileSwitch(v)
            | Self::OnboardProfileRestore(v)
            | Self::OnboardProfileSetEnabled(v) => v,
            Self::SetChoice { device_id, key, selected } =>
                serde_json::to_value(DaemonCommand::SetChoice { id: device_id, key, selected })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetRange { device_id, key, value } =>
                serde_json::to_value(DaemonCommand::SetRange { id: device_id, key, value })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetBoolean { device_id, key, value } =>
                serde_json::to_value(DaemonCommand::SetBoolean { id: device_id, key, value })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::TriggerAction { device_id, key } =>
                serde_json::to_value(DaemonCommand::TriggerAction { id: device_id, key })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::ResetAllButtonMappings { device_id } =>
                serde_json::to_value(DaemonCommand::ResetAllButtonMappings { id: device_id })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::ResetButtonMapping { device_id, cid } =>
                serde_json::to_value(DaemonCommand::ResetButtonMapping { id: device_id, cid })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetButtonMapping { device_id, mut mapping } => {
                mapping["id"] = device_id.into();
                mapping
            }
            Self::CanvasOp(v) => v,
            Self::AddProfile { name } =>
                serde_json::to_value(DaemonCommand::AddProfile { name })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::RenameProfile { old_name, new_name } =>
                serde_json::to_value(DaemonCommand::RenameProfile { old_name, new_name })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::RemoveProfile { name } =>
                serde_json::to_value(DaemonCommand::RemoveProfile { name })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SwitchProfile { name } =>
                serde_json::to_value(DaemonCommand::SwitchProfile { name })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::AddAppRule { process_names, profile, enabled } =>
                serde_json::to_value(DaemonCommand::AddAppRule { process_names, profile, enabled })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::UpdateAppRule { index, process_names, profile, enabled } =>
                serde_json::to_value(DaemonCommand::UpdateAppRule { index, process_names, profile, enabled })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::RemoveAppRule { index } =>
                serde_json::to_value(DaemonCommand::RemoveAppRule { index })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetEngineConfig { engine, config } => {
                let mut m = config;
                m["type"]   = "set_engine_config".into();
                m["engine"] = engine.into();
                m
            }
            Self::Rediscover =>
                serde_json::to_value(DaemonCommand::Rediscover)
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetLogLevel { level } =>
                serde_json::to_value(DaemonCommand::SetLogLevel { level })
                    .expect("DaemonCommand serialisation is infallible"),
            Self::SetUiConfig { close_to_tray } =>
                serde_json::to_value(DaemonCommand::SetUiConfig { close_to_tray })
                    .expect("DaemonCommand serialisation is infallible"),
        }
    }
}
