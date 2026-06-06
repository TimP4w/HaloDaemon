// On Windows, build as a GUI app so launching the UI does not spawn a console
// window. GTK warnings then go to a detached stderr instead of a popup terminal.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod commands;
mod ipc;
pub mod service;
mod state;
mod store;
mod tray;
mod ui;

use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use gtk4 as gtk;
use gtk4::gio;
use libadwaita as adw;

use crate::ipc::IpcSender;
use crate::store::Store;
use crate::ui::capability_registry::CapabilityRegistry;
use crate::ui::main_window::MainWindow;

const APP_ID: &str = "dev.timp4w.Halod";

fn main() -> glib::ExitCode {
    env_logger::init();
    ensure_daemon_running();

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_startup(|_| {
        load_icons();
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::PreferDark);
        load_css();
    });

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &adw::Application) {
    // The UI is single-instance: a second launch forwards `activate` here.
    // Just refocus the existing window instead of building a duplicate.
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ipc::IpcCmd>();
    let (event_tx, event_rx) = async_channel::unbounded::<ipc::client::DaemonMsg>();
    // Canvas frames flow on their own bounded channel (capacity 2 = always the latest).
    let (frame_tx, frame_rx) =
        async_channel::bounded::<Arc<halod_protocol::types::CanvasFrame>>(2);
    // LCD engine frames (1 Hz, per device).
    let (lcd_frame_tx, lcd_frame_rx) =
        async_channel::bounded::<Arc<halod_protocol::types::LcdEngineFrame>>(2);

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(ipc::client::run(cmd_rx, event_tx, frame_tx, lcd_frame_tx));
    });

    let store = Store::new(IpcSender::new(cmd_tx));

    let registry = Rc::new(CapabilityRegistry::default_registry());
    let win = MainWindow::new(app, store.clone(), registry);

    let background = std::env::args().skip(1).any(|a| a == "--background");
    if !background {
        win.present();
    }

    crate::tray::init(app, &win.window, store.clone(), store.ipc().clone());

    // State events → main GTK loop.
    let store_clone = store.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(msg) = event_rx.recv().await {
            store_clone.apply_msg(msg);
        }
    });

    // Canvas frames → canvas page (separate fast path).
    let canvas_page = win.canvas_page.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(frame) = frame_rx.recv().await {
            canvas_page.on_canvas_frame(frame);
        }
    });

    // LCD engine frames → device page's LCD widget (1 Hz fast path).
    let device_page = win.device_page.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(frame) = lcd_frame_rx.recv().await {
            if let Some(w) = device_page.lcd_widget().as_ref() {
                w.on_engine_frame(&frame);
            }
        }
    });
}

fn ensure_daemon_running() {
    std::thread::spawn(|| {
        for _ in 0..3 {
            if daemon_reachable() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        service::start_service();
    });
}

fn daemon_reachable() -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(crate::ipc::socket_path()).is_ok()
    }
    #[cfg(windows)]
    {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(crate::ipc::socket_path())
        {
            Ok(_) => true,
            Err(e) => e.raw_os_error() == Some(231), // ERROR_PIPE_BUSY
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

fn load_icons() {
    let bytes = glib::Bytes::from_static(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/periphctl-icons.gresource"
    )));
    let resource = gio::Resource::from_data(&bytes).expect("bundled icon gresource");
    gio::resources_register(&resource);
}

fn load_css() {
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("../style.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("display"),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
