// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows tray via `tray-icon`, pumped from the egui update loop.

use tray_icon::menu::{
    CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use super::{present, quit, render_icon_rgba, TrayModel};
use crate::runtime::ipc::CommandTx;

pub struct PlatformTray {
    tray: TrayIcon,
    cmd: CommandTx,
    force_quit: Arc<AtomicBool>,
    hide_state: Arc<crate::domain::state::HideState>,
    open_id: MenuId,
    quit_id: MenuId,
    /// (menu id, profile name) for each profile entry in the submenu.
    profile_ids: Vec<(MenuId, String)>,
}

impl PlatformTray {
    pub fn new(
        _ctx: &egui::Context,
        cmd: CommandTx,
        force_quit: Arc<AtomicBool>,
        hide_state: Arc<crate::domain::state::HideState>,
    ) -> Self {
        let built = build_menu(&TrayModel::default());
        let tray = TrayIconBuilder::new()
            .with_tooltip(halod_shared::app::APP_DISPLAY_NAME)
            .with_menu(Box::new(built.menu))
            .with_icon(load_icon())
            .build()
            .expect("tray icon");
        Self {
            tray,
            cmd,
            force_quit,
            hide_state,
            open_id: built.open_id,
            quit_id: built.quit_id,
            profile_ids: built.profile_ids,
        }
    }

    pub fn sync(&mut self, ctx: &egui::Context, model: &TrayModel, changed: bool) {
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                present(ctx, &self.hide_state);
            }
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.open_id {
                present(ctx, &self.hide_state);
            } else if ev.id == self.quit_id {
                quit(ctx, &self.cmd, &self.force_quit, &self.hide_state);
            } else if let Some((_, name)) = self.profile_ids.iter().find(|(id, _)| *id == ev.id) {
                crate::domain::actions::profiles::switch_profile(&self.cmd, name);
            }
        }
        if changed {
            let built = build_menu(model);
            self.open_id = built.open_id;
            self.quit_id = built.quit_id;
            self.profile_ids = built.profile_ids;
            self.tray.set_menu(Some(Box::new(built.menu)));
        }
    }
}

struct BuiltMenu {
    menu: Menu,
    open_id: MenuId,
    quit_id: MenuId,
    profile_ids: Vec<(MenuId, String)>,
}

fn build_menu(model: &TrayModel) -> BuiltMenu {
    let menu = Menu::new();
    for line in &model.battery_lines {
        let _ = menu.append(&MenuItem::new(line, false, None));
    }
    if !model.battery_lines.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    let mut profile_ids = Vec::new();
    if !model.profiles.is_empty() {
        let submenu = Submenu::new(t!("tray.profile"), true);
        for name in &model.profiles {
            let item = CheckMenuItem::new(name, true, name == &model.active, None);
            profile_ids.push((item.id().clone(), name.clone()));
            let _ = submenu.append(&item);
        }
        let _ = menu.append(&submenu);
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    let open = MenuItem::new(t!("tray.open"), true, None);
    let quit_item = MenuItem::new(t!("tray.quit"), true, None);
    let open_id = open.id().clone();
    let quit_id = quit_item.id().clone();
    let _ = menu.append(&open);
    let _ = menu.append(&quit_item);
    BuiltMenu {
        menu,
        open_id,
        quit_id,
        profile_ids,
    }
}

fn load_icon() -> Icon {
    let (rgba, w, h) = render_icon_rgba();
    Icon::from_rgba(rgba, w, h).expect("icon RGBA is valid")
}
