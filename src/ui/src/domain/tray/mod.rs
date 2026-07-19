// SPDX-License-Identifier: GPL-3.0-or-later
//! System-tray icon + menu and the window/app icon.
//!
//! Linux speaks the StatusNotifierItem protocol via `ksni` on its own
//! background thread (independent of the winit event loop); Windows uses
//! `tray-icon` pumped from the egui update loop. Both share the embedded-SVG
//! icon and the per-device battery summary lines. The window/taskbar icon comes
//! from the same SVG via [`app_icon`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::domain::topic_store::TopicStore;
use halod_shared::types::{BatteryStatus, DeviceCapability, DEFAULT_PROFILE_NAME};

use crate::runtime::ipc::CommandTx;

#[cfg(unix)]
mod linux;
#[cfg(windows)]
mod windows;

/// Suffix appended to a charging battery line. Shared by the displayed text and
/// the change-detection so the two can never disagree on when to re-render.
const CHARGING_SUFFIX: &str = " ↑";

/// One human-readable line per battery across all devices, e.g.
/// `"G560 - Battery: 75%"`. Charging batteries get [`CHARGING_SUFFIX`] appended.
pub fn battery_lines(state: &TopicStore) -> Vec<String> {
    let mut lines = Vec::new();
    for device in &state.devices {
        for cap in &device.capabilities {
            if let DeviceCapability::Battery(batteries) = cap {
                for b in batteries {
                    let suffix = if b.status == BatteryStatus::Charging {
                        CHARGING_SUFFIX
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "{} - {}: {}%{}",
                        device.name, b.label, b.level, suffix
                    ));
                }
            }
        }
    }
    lines
}

/// The embedded app icon rendered to RGBA8 pixels: `(rgba, width, height)`.
fn render_icon_rgba() -> (Vec<u8>, u32, u32) {
    use resvg::{tiny_skia, usvg};
    let bytes = include_bytes!("../../../../../assets/icon.svg");
    let tree = usvg::Tree::from_data(bytes, &usvg::Options::default())
        .expect("embedded icon is valid SVG");
    let size = tree.size().to_int_size();
    let (w, h) = (size.width(), size.height());
    let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    (pixmap.take(), w, h)
}

/// The window/taskbar icon for the eframe viewport (`ViewportBuilder::with_icon`).
pub fn app_icon() -> egui::IconData {
    let (rgba, width, height) = render_icon_rgba();
    egui::IconData {
        rgba,
        width,
        height,
    }
}

/// Bring the main window to the foreground (used by both backends' "Open").
///
/// On eframe, `Visible(true)` re-shows the window. On the Linux custom loop the
/// window may have been destroyed, so set `wants_show` too — the loop recreates
/// it. `request_repaint` wakes the loop (via its proxy) to act on the flag.
pub(crate) fn present(ctx: &egui::Context, hide_state: &crate::domain::state::HideState) {
    hide_state.wants_show.store(true, Ordering::SeqCst);
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
    // Linux: the window may be destroyed, so wake the loop to recreate it.
    hide_state.wake();
}

/// Quit the whole application: shut down the daemon and close the UI window.
///
/// Sets `force_quit` so the backend lets the close through even when "close to
/// tray" is enabled (which otherwise hides the window instead).
fn quit(
    ctx: &egui::Context,
    cmd: &CommandTx,
    force_quit: &AtomicBool,
    hide_state: &crate::domain::state::HideState,
) {
    force_quit.store(true, Ordering::SeqCst);
    crate::runtime::ipc::send(cmd, halod_shared::commands::DaemonCommand::Shutdown);
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    ctx.request_repaint();
    // Linux: while closed to tray there is no window to receive the close, so
    // wake the loop to observe `force_quit` and exit.
    hide_state.wake();
}

#[cfg(unix)]
use linux::PlatformTray;
#[cfg(windows)]
use windows::PlatformTray;

#[cfg(not(any(unix, windows)))]
struct PlatformTray;
#[cfg(not(any(unix, windows)))]
impl PlatformTray {
    fn new(
        _: &egui::Context,
        _: CommandTx,
        _: Arc<AtomicBool>,
        _: Arc<crate::domain::state::HideState>,
    ) -> Self {
        Self
    }
    fn sync(&mut self, _: &egui::Context, _: &TrayModel, _: bool) {}
}

/// The tray's rendered state: battery readouts plus the profile list and which
/// one is active. Held by [`Tray`] for change-detection so the platform menu is
/// only rebuilt when something the user sees actually changed.
#[derive(Clone, Default, PartialEq)]
pub struct TrayModel {
    pub battery_lines: Vec<String>,
    pub profiles: Vec<String>,
    pub active: String,
}

impl TrayModel {
    fn from_state(state: &TopicStore) -> Self {
        let active = if state.profiles.active.is_empty() {
            DEFAULT_PROFILE_NAME.to_string()
        } else {
            state.profiles.active.clone()
        };
        Self {
            battery_lines: battery_lines(state),
            profiles: state.profiles.available.clone(),
            active,
        }
    }
}

/// The platform tray, owned by the egui `App`. Construct once with the egui
/// `Context` and a command sender, then call [`Tray::sync`] each frame to
/// refresh battery/profile readouts (and, on Windows, to pump the event queue).
pub struct Tray {
    inner: PlatformTray,
    shown: TrayModel,
}

impl Tray {
    pub fn new(
        ctx: &egui::Context,
        cmd: CommandTx,
        force_quit: Arc<AtomicBool>,
        hide_state: Arc<crate::domain::state::HideState>,
    ) -> Self {
        Self {
            inner: PlatformTray::new(ctx, cmd, force_quit, hide_state),
            shown: TrayModel::default(),
        }
    }

    /// A tray with no platform backend, for headless snapshot rendering — it
    /// starts no D-Bus/ksni thread and registers no icon.
    #[cfg(all(test, target_os = "linux", feature = "screenshots"))]
    pub(crate) fn headless() -> Self {
        Self {
            inner: PlatformTray::headless(),
            shown: TrayModel::default(),
        }
    }

    pub fn sync(&mut self, ctx: &egui::Context, state: &TopicStore) {
        let model = TrayModel::from_state(state);
        let changed = model != self.shown;
        self.inner.sync(ctx, &model, changed);
        if changed {
            self.shown = model;
        }
    }

    #[cfg(unix)]
    pub fn watch_state(&self, state: tokio::sync::watch::Receiver<TopicStore>) {
        self.inner.watch_state(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::commands::DaemonCommand;
    use halod_shared::types::{Battery, DeviceType, WireDevice};

    fn device_with_batteries(name: &str, batteries: Vec<Battery>) -> WireDevice {
        WireDevice {
            id: name.to_string(),
            name: name.to_string(),
            device_type: DeviceType::Mouse,
            connected: true,
            capabilities: vec![DeviceCapability::Battery(batteries)],
            ..Default::default()
        }
    }

    fn battery(level: u8, status: BatteryStatus) -> Battery {
        Battery {
            key: "battery".into(),
            label: "Battery".into(),
            level,
            status,
        }
    }

    #[test]
    fn battery_lines_formats_discharging() {
        let state = TopicStore {
            devices: vec![device_with_batteries(
                "G560",
                vec![battery(75, BatteryStatus::Discharging)],
            )],
            ..Default::default()
        };
        assert_eq!(
            battery_lines(&state),
            vec!["G560 - Battery: 75%".to_string()]
        );
    }

    #[test]
    fn battery_lines_appends_suffix_when_charging() {
        let state = TopicStore {
            devices: vec![device_with_batteries(
                "G560",
                vec![battery(40, BatteryStatus::Charging)],
            )],
            ..Default::default()
        };
        assert_eq!(
            battery_lines(&state),
            vec![format!("G560 - Battery: 40%{CHARGING_SUFFIX}")]
        );
    }

    #[test]
    fn battery_lines_empty_when_no_battery_capability() {
        assert!(battery_lines(&TopicStore::default()).is_empty());
    }

    #[test]
    fn quit_sets_force_quit_and_sends_shutdown() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let force_quit = AtomicBool::new(false);
        let hide_state = crate::domain::state::HideState::default();
        quit(&egui::Context::default(), &tx, &force_quit, &hide_state);
        assert!(force_quit.load(Ordering::SeqCst));
        assert!(matches!(rx.try_recv(), Ok(DaemonCommand::Shutdown)));
    }

    #[test]
    fn app_icon_is_nonempty_square_rgba() {
        let icon = app_icon();
        assert!(icon.width > 0 && icon.height > 0);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
    }
}
