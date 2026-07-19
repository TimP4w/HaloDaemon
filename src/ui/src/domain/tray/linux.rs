// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux StatusNotifierItem tray via `ksni`.
//!
//! `ksni` runs its own background thread + D-Bus connection, so the menu
//! callbacks act directly on a cloned egui `Context` (`send_viewport_cmd` +
//! `request_repaint` are thread-safe) rather than routing through a channel.

use halod_shared::app;
use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::{present, quit, render_icon_rgba, TrayModel};
use crate::runtime::ipc::CommandTx;

/// Longest gap between retries when the tray service keeps failing to register.
const MAX_TRAY_BACKOFF: Duration = Duration::from_secs(30);

pub struct PlatformTray {
    /// Handle to the currently-running ksni service, or `None` while a rebuild
    /// is pending. Swapped by the supervisor thread on every (re)start.
    handle: Arc<Mutex<Option<ksni::Handle<HalodTray>>>>,
    /// Last model pushed from the UI, used to seed a freshly rebuilt tray so it
    /// comes up with current battery/profile state rather than empty.
    latest: Arc<Mutex<TrayModel>>,
}

impl PlatformTray {
    pub fn new(
        ctx: &egui::Context,
        cmd: CommandTx,
        force_quit: Arc<AtomicBool>,
        hide_state: Arc<crate::domain::state::HideState>,
    ) -> Self {
        let handle: Arc<Mutex<Option<ksni::Handle<HalodTray>>>> = Arc::new(Mutex::new(None));
        let latest = Arc::new(Mutex::new(TrayModel::default()));
        let ctx = ctx.clone();
        {
            let handle = handle.clone();
            let latest = latest.clone();
            std::thread::spawn(move || {
                let mut backoff = Duration::from_millis(500);
                loop {
                    let seed = latest.lock().unwrap().clone();
                    let service = ksni::TrayService::new(HalodTray::new(
                        ctx.clone(),
                        cmd.clone(),
                        force_quit.clone(),
                        hide_state.clone(),
                        seed,
                    ));
                    *handle.lock().unwrap() = Some(service.handle());
                    match service.run() {
                        Ok(()) => break,
                        Err(e) => log::warn!("tray service exited, retrying: {e}"),
                    }
                    *handle.lock().unwrap() = None;
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(MAX_TRAY_BACKOFF);
                }
            });
        }
        Self { handle, latest }
    }

    /// A tray with no running ksni service — for headless tests, so no D-Bus
    /// connection or background thread is started. `sync` becomes a no-op.
    #[cfg(all(test, feature = "screenshots"))]
    pub fn headless() -> Self {
        Self {
            handle: Arc::new(Mutex::new(None)),
            latest: Arc::new(Mutex::new(TrayModel::default())),
        }
    }

    pub fn sync(&mut self, _ctx: &egui::Context, model: &TrayModel, changed: bool) {
        if !changed {
            return;
        }
        *self.latest.lock().unwrap() = model.clone();
        if let Some(handle) = self.handle.lock().unwrap().as_ref() {
            let model = model.clone();
            handle.update(move |tray: &mut HalodTray| {
                tray.battery_lines = model.battery_lines;
                tray.profiles = model.profiles;
                tray.active = model.active;
            });
        }
    }

    /// Feed the windowless tray directly from daemon state. At login there may
    /// be no egui frame at all, and desktop event loops are free to coalesce
    /// repaint wakeups, so tray data must not depend on window rendering.
    pub fn watch_state(
        &self,
        mut state: tokio::sync::watch::Receiver<crate::domain::topic_store::TopicStore>,
    ) {
        let handle = self.handle.clone();
        let latest = self.latest.clone();
        std::thread::spawn(move || loop {
            match state.has_changed() {
                Ok(true) => {
                    let model = TrayModel::from_state(&state.borrow_and_update());
                    *latest.lock().unwrap() = model.clone();
                    if let Some(handle) = handle.lock().unwrap().as_ref() {
                        handle.update(move |tray: &mut HalodTray| {
                            tray.battery_lines = model.battery_lines;
                            tray.profiles = model.profiles;
                            tray.active = model.active;
                        });
                    }
                }
                Ok(false) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        });
    }
}

struct HalodTray {
    ctx: egui::Context,
    cmd: CommandTx,
    force_quit: Arc<AtomicBool>,
    hide_state: Arc<crate::domain::state::HideState>,
    battery_lines: Vec<String>,
    profiles: Vec<String>,
    active: String,
    icon: Vec<ksni::Icon>,
}

impl HalodTray {
    fn new(
        ctx: egui::Context,
        cmd: CommandTx,
        force_quit: Arc<AtomicBool>,
        hide_state: Arc<crate::domain::state::HideState>,
        seed: TrayModel,
    ) -> Self {
        Self {
            ctx,
            cmd,
            force_quit,
            hide_state,
            battery_lines: seed.battery_lines,
            profiles: seed.profiles,
            active: seed.active,
            icon: load_icon(),
        }
    }
}

/// The embedded icon as ARGB32 in network (big-endian) byte order, as ksni
/// expects; `render_icon_rgba` returns RGBA.
fn load_icon() -> Vec<ksni::Icon> {
    let (rgba, width, height) = render_icon_rgba();
    let data = rgba
        .chunks_exact(4)
        .flat_map(|p| [p[3], p[0], p[1], p[2]])
        .collect();
    vec![ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }]
}

impl ksni::Tray for HalodTray {
    fn id(&self) -> String {
        app::APP_NAME.to_string()
    }

    fn title(&self) -> String {
        app::APP_DISPLAY_NAME.to_string()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        present(&self.ctx, &self.hide_state);
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: app::APP_DISPLAY_NAME.to_string(),
            description: self.battery_lines.join("\n"),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items: Vec<MenuItem<Self>> = self
            .battery_lines
            .iter()
            .map(|line| {
                MenuItem::Standard(StandardItem {
                    label: line.clone(),
                    enabled: false,
                    ..Default::default()
                })
            })
            .collect();

        if !items.is_empty() {
            items.push(MenuItem::Separator);
        }

        if !self.profiles.is_empty() {
            let selected = self
                .profiles
                .iter()
                .position(|p| p == &self.active)
                .unwrap_or(0);
            let options = self
                .profiles
                .iter()
                .map(|p| RadioItem {
                    label: p.clone(),
                    ..Default::default()
                })
                .collect();
            items.push(MenuItem::SubMenu(SubMenu {
                label: t!("tray.profile").to_string(),
                submenu: vec![MenuItem::RadioGroup(RadioGroup {
                    selected,
                    options,
                    select: Box::new(|this: &mut Self, idx| {
                        if let Some(name) = this.profiles.get(idx) {
                            crate::runtime::ipc::send(
                                &this.cmd,
                                halod_shared::commands::DaemonCommand::SwitchProfile {
                                    name: name.to_string(),
                                },
                            );
                        }
                    }),
                })],
                ..Default::default()
            }));
            items.push(MenuItem::Separator);
        }

        items.push(MenuItem::Standard(StandardItem {
            label: t!("tray.open").to_string(),
            activate: Box::new(|this: &mut Self| present(&this.ctx, &this.hide_state)),
            ..Default::default()
        }));
        items.push(MenuItem::Standard(StandardItem {
            label: t!("tray.quit").to_string(),
            activate: Box::new(|this: &mut Self| {
                quit(&this.ctx, &this.cmd, &this.force_quit, &this.hide_state)
            }),
            ..Default::default()
        }));

        items
    }
}
