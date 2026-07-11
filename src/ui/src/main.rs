// SPDX-License-Identifier: GPL-3.0-or-later
//! HaloDaemon egui UI — home screen, device pages, and Effects Canvas.

// Build as a Windows GUI app in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[macro_use]
extern crate rust_i18n;

// Translation catalogs live in ui/locales/<code>.yaml; `t!(...)` looks up the
// active locale (set from GuiConfig.language), falling back to English.
i18n!("locales", fallback = "en");

mod app;
mod domain;
mod runtime;
mod ui;

#[cfg(not(target_os = "linux"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
#[cfg(not(target_os = "linux"))]
use std::sync::Arc;

use app::App;
use domain::lifecycle::CloseAction;
#[cfg(not(target_os = "linux"))]
use domain::state::HideState;

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        // Transparent so the rounded panel/radar backgrounds show their corners.
        [0.0, 0.0, 0.0, 0.0]
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // egui#7959: after a native window drag/resize the WM eats the button
        // release, so egui's pointer stays stuck "down" and every other drag is
        // dropped. Inject the missing release on the frame after we armed it.
        if ui::shell::take_pending_pointer_release(ctx) && ctx.input(|i| i.pointer.any_down()) {
            let pos = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
            raw_input.events.push(egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
        // Hide via `Visible(false)`; see [`wayland_hide`] for the Linux path.
        let ctx = ui.ctx();
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        match self.close_action(close_requested) {
            CloseAction::Stay => {}
            CloseAction::HideToTray => {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            CloseAction::Quit => {
                if !self.force_quit.load(Ordering::SeqCst) {
                    domain::actions::system::shutdown(&self.cmd);
                }
            }
        }
    }
}

/// Linux uses a bespoke winit + glutin + egui_glow loop so "close to tray" can
/// destroy and recreate the window — winit can't hide a Wayland surface. See
/// [`wayland_hide`]. Everything else uses eframe.
#[cfg(target_os = "linux")]
fn main() {
    env_logger::init();
    let background = domain::lifecycle::start_in_background(std::env::args());
    runtime::wayland_hide::run(background);
}

#[cfg(not(target_os = "linux"))]
fn main() -> eframe::Result<()> {
    env_logger::init();

    let background = domain::lifecycle::start_in_background(std::env::args());

    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title(halod_shared::app::APP_DISPLAY_NAME)
            .with_icon(std::sync::Arc::new(domain::tray::app_icon()))
            .with_decorations(false)
            .with_transparent(true)
            .with_visible(!background),
        ..Default::default()
    };

    eframe::run_native(
        halod_shared::app::GUI_PROCESS_NAME,
        options,
        Box::new(move |cc| {
            ui::theme::install(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let (cmd_tx, ui) = runtime::ipc::spawn(move || ctx.request_repaint());
            let force_quit = Arc::new(AtomicBool::new(false));
            let hide_state = Arc::new(HideState::default());
            let tray = domain::tray::Tray::new(
                &cc.egui_ctx,
                cmd_tx.clone(),
                force_quit.clone(),
                hide_state,
            );
            Ok(Box::new(App::new(ui, cmd_tx, tray, force_quit)))
        }),
    )
}

// `classify_close`/`CloseAction` tests live with their definition in
// `domain::lifecycle::tests`.
