// SPDX-License-Identifier: MIT
//! Windows tray implementation using tray-icon with GLib polling.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk4::glib;
use libadwaita as adw;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use crate::ipc::IpcSender;
use crate::service;
use crate::state::AppState;
use crate::store::Store;

pub fn init(app: &adw::Application, window: &adw::ApplicationWindow, store: Store, ipc: IpcSender) {
    // Build initial menu
    let (menu, open_id, quit_id) = build_menu(&no_battery_lines());
    let icon = load_icon();

    let tray = TrayIconBuilder::new()
        .with_tooltip("HaloDaemon")
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .build()
        .expect("tray icon");

    let tray = Rc::new(RefCell::new(tray));
    let open_id = Rc::new(RefCell::new(open_id));
    let quit_id = Rc::new(RefCell::new(quit_id));
    let shown = Rc::new(RefCell::new(no_battery_lines()));

    // Poll tray events every 250ms on the GLib main loop
    let win = window.clone();
    let app_poll = app.clone();
    let ipc_poll = ipc.clone();
    let tray_poll = tray.clone();
    let open_poll = open_id.clone();
    let quit_poll = quit_id.clone();
    let store_poll = store.clone();
    let shown_poll = shown.clone();

    glib::timeout_add_local(Duration::from_millis(250), move || {
        let tray_rx = TrayIconEvent::receiver();
        let menu_rx = MenuEvent::receiver();

        while let Ok(ev) = tray_rx.try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = ev
            {
                win.present();
            }
        }

        while let Ok(ev) = menu_rx.try_recv() {
            if ev.id == *open_poll.borrow() {
                win.present();
            } else if ev.id == *quit_poll.borrow() {
                service::stop_service(&ipc_poll);
                app_poll.quit();
            }
        }

        // Refresh menu if battery state changed
        let state = store_poll.state();
        let lines = state_battery_lines(&state);
        drop(state);
        if lines != *shown_poll.borrow() {
            let (new_menu, new_open, new_quit) = build_menu(&lines);
            *open_poll.borrow_mut() = new_open;
            *quit_poll.borrow_mut() = new_quit;
            let _ = tray_poll.borrow().set_menu(Some(Box::new(new_menu)));
            *shown_poll.borrow_mut() = lines;
        }

        glib::ControlFlow::Continue
    });
}

fn state_battery_lines(state: &AppState) -> Vec<String> {
    use halod_protocol::types::{BatteryStatus, DeviceCapability};
    let mut lines = Vec::new();
    for device in &state.devices {
        for cap in &device.capabilities {
            if let DeviceCapability::Battery(batteries) = cap {
                for b in batteries {
                    let bolt = if b.status == BatteryStatus::Charging {
                        " ⚡"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "{} — {}: {}%{}",
                        device.name, b.label, b.level, bolt
                    ));
                }
            }
        }
    }
    lines
}

fn no_battery_lines() -> Vec<String> {
    vec!["No devices with battery".to_string()]
}

fn build_menu(lines: &[String]) -> (Menu, MenuId, MenuId) {
    let menu = Menu::new();
    for line in lines {
        let _ = menu.append(&MenuItem::new(line, false, None));
    }
    if !lines.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }
    let open = MenuItem::new("Open HaloDaemon", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let open_id = open.id().clone();
    let quit_id = quit.id().clone();
    let _ = menu.append(&open);
    let _ = menu.append(&quit);
    (menu, open_id, quit_id)
}

fn load_icon() -> Icon {
    use resvg::{tiny_skia, usvg};
    let bytes = include_bytes!("../../../../assets/icon.svg");
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(bytes, &opt).expect("embedded icon is valid SVG");
    let size = tree.size().to_int_size();
    let (w, h) = (size.width(), size.height());
    let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");
    resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
    Icon::from_rgba(pixmap.take(), w, h).expect("icon RGBA is valid")
}
