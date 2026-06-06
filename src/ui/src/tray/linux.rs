// SPDX-License-Identifier: MIT
//! Linux SNI tray implementation using ksni.

use async_channel::Sender;
use halod_protocol::types::{BatteryStatus, DeviceCapability};
use ksni::menu::{MenuItem, StandardItem};

use crate::state::AppState;

/// Actions the tray can request on the GTK main thread.
pub enum TrayAction {
    Open,
    Quit,
}

/// The ksni tray model for HaloDaemon.
pub struct HalodTray {
    battery_lines: Vec<String>,
    action_tx: Sender<TrayAction>,
    icon: Vec<ksni::Icon>,
}

impl HalodTray {
    pub fn new(action_tx: Sender<TrayAction>) -> Self {
        Self {
            battery_lines: Vec::new(),
            action_tx,
            icon: Self::load_icon(),
        }
    }

    /// Load and cache the embedded icon, converting SVG to ARGB32.
    fn load_icon() -> Vec<ksni::Icon> {
        use resvg::{tiny_skia, usvg};
        let bytes = include_bytes!("../../../../assets/icon.svg");
        let opt = usvg::Options::default();
        let tree = usvg::Tree::from_data(bytes, &opt).expect("embedded icon is valid SVG");
        let size = tree.size().to_int_size();
        let (w, h) = (size.width(), size.height());
        let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");
        resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
        // ksni expects ARGB32 in network (big-endian) byte order; pixmap is RGBA.
        let data: Vec<u8> = pixmap
            .data()
            .chunks_exact(4)
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect();
        vec![ksni::Icon {
            width: w as i32,
            height: h as i32,
            data,
        }]
    }

    /// Update cached battery lines from a fresh AppState broadcast.
    pub fn apply_state(&mut self, state: &AppState) {
        self.battery_lines = battery_lines(state);
    }

    /// Send an action to the GTK main thread, logging if the channel is full.
    fn send_action(&self, action: TrayAction) {
        match self.action_tx.try_send(action) {
            Ok(()) => {}
            Err(async_channel::TrySendError::Full(_)) => {
                log::warn!("tray: action channel full, dropping action");
            }
            Err(async_channel::TrySendError::Closed(_)) => {}
        }
    }
}

impl ksni::Tray for HalodTray {
    fn id(&self) -> String {
        "halod".to_string()
    }

    fn title(&self) -> String {
        "HaloDaemon".to_string()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send_action(TrayAction::Open);
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = if self.battery_lines.is_empty()
            || self.battery_lines == ["Daemon not running"]
            || self.battery_lines == ["No devices with battery"]
        {
            String::new()
        } else {
            self.battery_lines.join("\n")
        };
        ksni::ToolTip {
            title: "HaloDaemon".to_string(),
            description,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = self
            .battery_lines
            .iter()
            .map(|line| {
                let label = line.clone();
                MenuItem::Standard(StandardItem {
                    label,
                    enabled: false,
                    ..Default::default()
                })
            })
            .collect();

        if !items.is_empty() {
            items.push(MenuItem::Separator);
        }

        let tx_open = self.action_tx.clone();
        items.push(MenuItem::Standard(StandardItem {
            label: "Open HaloDaemon".to_string(),
            activate: Box::new(move |_this| {
                // Note: TryFromClosureError::Full will be logged by send_action.
                // This closure can't call a method, so we match directly here.
                match tx_open.try_send(TrayAction::Open) {
                    Ok(()) => {}
                    Err(async_channel::TrySendError::Full(_)) => {
                        log::warn!("tray: action channel full, dropping action");
                    }
                    Err(async_channel::TrySendError::Closed(_)) => {}
                }
            }),
            ..Default::default()
        }));

        let tx_quit = self.action_tx.clone();
        items.push(MenuItem::Standard(StandardItem {
            label: "Quit".to_string(),
            activate: Box::new(move |_this| match tx_quit.try_send(TrayAction::Quit) {
                Ok(()) => {}
                Err(async_channel::TrySendError::Full(_)) => {
                    log::warn!("tray: action channel full, dropping action");
                }
                Err(async_channel::TrySendError::Closed(_)) => {}
            }),
            ..Default::default()
        }));

        items
    }
}

/// Returns one human-readable line per battery on any device, e.g.
/// `"G560 — Battery: 75%"`. Charging batteries get a trailing ↑.
pub fn battery_lines(state: &AppState) -> Vec<String> {
    let mut lines = Vec::new();
    for device in &state.devices {
        for cap in &device.capabilities {
            if let DeviceCapability::Battery(batteries) = cap {
                for b in batteries {
                    let charge = if b.status == BatteryStatus::Charging {
                        " ↑"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "{} — {}: {}%{}",
                        device.name, b.label, b.level, charge
                    ));
                }
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{Battery, ConnectionType, DeviceType, WireDevice};

    fn device_with_batteries(name: &str, batteries: Vec<Battery>) -> WireDevice {
        WireDevice {
            id: name.to_string(),
            name: name.to_string(),
            vendor: "Test".to_string(),
            model: name.to_string(),
            device_type: DeviceType::Mouse,
            connected: true,
            capabilities: vec![DeviceCapability::Battery(batteries)],
            connection_type: Some(ConnectionType::Wireless),
            serial_number: None,
            ..Default::default()
        }
    }

    #[test]
    fn battery_lines_formats_discharging() {
        let mut state = AppState::default();
        state.devices = vec![device_with_batteries(
            "G560",
            vec![Battery {
                key: "battery".into(),
                label: "Battery".into(),
                level: 75,
                status: BatteryStatus::Discharging,
            }],
        )];
        let lines = battery_lines(&state);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "G560 — Battery: 75%");
    }

    #[test]
    fn battery_lines_appends_arrow_when_charging() {
        let mut state = AppState::default();
        state.devices = vec![device_with_batteries(
            "G560",
            vec![Battery {
                key: "battery".into(),
                label: "Battery".into(),
                level: 40,
                status: BatteryStatus::Charging,
            }],
        )];
        let lines = battery_lines(&state);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "G560 — Battery: 40% ↑");
    }

    #[test]
    fn battery_lines_empty_when_no_battery_capability() {
        let state = AppState::default();
        assert!(battery_lines(&state).is_empty());
    }
}
