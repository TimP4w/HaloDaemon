// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: egui contributors <https://github.com/emilk/egui>
//! Linux "close to tray" quirk: a bespoke winit + glutin + `egui_glow` event
//! loop that **destroys and recreates the window** instead of hiding it.
//!
//! winit's Wayland backend cannot hide a surface (`Window::set_visible` is a
//! no-op there), so eframe's `ViewportCommand::Visible(false)` does nothing
//! under native Wayland. The only way to disappear the window while keeping
//! the process (and tray) alive is to drop the surface and rebuild it on
//! demand — the same approach Slint took (slint-ui/slint#5529). Across a
//! hide/show cycle the [`egui::Context`] and the GL context + `egui_glow`
//! painter (which owns egui's uploaded textures) survive; only the winit
//! window and its EGL surface are transient. egui is driven manually, not via
//! `egui_glow::EguiGlow`, so we can observe the outgoing `Close` command and
//! run `raw_input_hook` (egui#7959) ourselves.
//!
//! **This whole module is a workaround.** When eframe/winit gain native
//! Wayland window hiding, delete it and let Linux fall through to eframe.

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::App as _; // brings `raw_input_hook` into scope for the custom loop
use egui::ViewportId;
use egui_glow::egui_winit;
use egui_glow::glow;
use halod_shared::commands::DaemonCommand;
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::HasWindowHandle as _;
use winit::window::WindowId;

use crate::app::App;
use crate::domain::lifecycle::CloseAction;
use crate::domain::state::HideState;
use crate::domain::tray;
use crate::runtime::ipc;
use crate::ui::theme;

/// Wakes the event loop when egui asks for a repaint (from the UI thread or the
/// IPC thread), carrying egui's requested delay so we can schedule accordingly.
#[derive(Debug)]
enum UserEvent {
    Redraw(Duration),
}

/// Entry point for the Linux backend. Runs until the tray "Quit" (or a real
/// window close with "close to tray" off) exits the loop. `background` starts
/// tray-only, with no window ever created until the tray "Open" is used (sign-in
/// autostart via `--background`).
pub fn run(background: bool) {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("failed to build winit event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();

    let ctx = egui::Context::default();
    theme::install(&ctx);
    let cb_proxy = egui::mutex::Mutex::new(proxy.clone());
    ctx.set_request_repaint_callback(move |info| {
        let _ = cb_proxy.lock().send_event(UserEvent::Redraw(info.delay));
    });

    let force_quit = Arc::new(AtomicBool::new(false));
    let hide_state = Arc::new(HideState::default());
    // While the window is destroyed, egui's edge-triggered repaint callback
    // won't fire, so let the tray wake the loop directly for "Open"/"Quit".
    let waker_proxy = egui::mutex::Mutex::new(proxy.clone());
    hide_state.set_waker(move || {
        let _ = waker_proxy
            .lock()
            .send_event(UserEvent::Redraw(Duration::ZERO));
    });
    let ctx_for_ipc = ctx.clone();
    let (cmd_tx, ui_rx) = ipc::spawn(move || ctx_for_ipc.request_repaint());
    let tray = tray::Tray::new(&ctx, cmd_tx.clone(), force_quit.clone(), hide_state.clone());
    let app = App::new(ui_rx, cmd_tx, tray, force_quit.clone());

    let mut handler = WaylandApp {
        ctx,
        egui_winit: None,
        painter: None,
        viewport_info: egui::ViewportInfo::default(),
        gl_keep: None,
        win: None,
        gl: None,
        app,
        force_quit,
        hide_state,
        hidden: background,
        start_hidden: background,
        wm_close: false,
        repaint_delay: Duration::MAX,
    };
    event_loop
        .run_app(&mut handler)
        .expect("winit event loop failed");
}

struct WaylandApp {
    /// The egui context. Fonts, theme, memory, and the repaint callback survive
    /// every hide/show cycle.
    ctx: egui::Context,
    egui_winit: Option<egui_winit::State>,
    painter: Option<egui_glow::Painter>,
    viewport_info: egui::ViewportInfo,
    gl_keep: Option<GlKeep>,
    win: Option<WindowSurface>,
    gl: Option<Arc<glow::Context>>,
    app: App,
    force_quit: Arc<AtomicBool>,
    hide_state: Arc<HideState>,
    hidden: bool,
    /// `--background`: the first `resumed()` skips window/GL creation
    /// entirely instead of flashing a window then hiding it. Consumed there.
    start_hidden: bool,
    /// WM close-requested this cycle.
    wm_close: bool,
    repaint_delay: Duration,
}

impl WaylandApp {
    fn window(&self) -> Option<&winit::window::Window> {
        self.win.as_ref().map(|w| &w.window)
    }

    fn resize(&self, size: winit::dpi::PhysicalSize<u32>) {
        use glutin::surface::GlSurface as _;
        if let (Some(win), Some(keep)) = (&self.win, &self.gl_keep) {
            if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
            {
                win.gl_surface.resize(&keep.gl_context, w, h);
            }
        }
    }

    /// Draw + paint one frame, then act on any close request.
    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        if self.hidden {
            return;
        }
        let action = {
            let (Some(egui_winit), Some(painter), Some(win), Some(keep), Some(gl)) = (
                self.egui_winit.as_mut(),
                self.painter.as_mut(),
                self.win.as_ref(),
                self.gl_keep.as_ref(),
                self.gl.as_ref(),
            ) else {
                return;
            };
            let window = &win.window;

            egui_winit::update_viewport_info(&mut self.viewport_info, &self.ctx, window, false);
            egui_winit
                .egui_input_mut()
                .viewports
                .insert(ViewportId::ROOT, self.viewport_info.clone());
            let mut raw_input = egui_winit.take_egui_input(window);
            // eframe calls this for us; the custom loop must too, or the
            // pointer-release workaround (egui#7959) never fires and every other
            // native window drag/resize is silently dropped.
            self.app.raw_input_hook(&self.ctx, &mut raw_input);

            let app = &mut self.app;
            let egui::FullOutput {
                platform_output,
                textures_delta,
                shapes,
                pixels_per_point,
                viewport_output,
            } = self.ctx.run_ui(raw_input, |ui| app.draw(ui));

            egui_winit.handle_platform_output(window, platform_output);

            // Schedule the next wake from egui's own per-frame repaint request.
            // The repaint callback only ever fires with *shorter* delays (it's
            // skipped when a viewport settles back to `Duration::MAX`), so relying
            // on it alone leaves `repaint_delay` stuck at the smallest value ever
            // seen. A single zero-delay repaint — which every LCD video frame
            // triggers — would then pin the loop in `ControlFlow::Poll` forever,
            // busy-spinning the GPU paint path until it runs out of resources and
            // the window dies. The frame output is authoritative: it reports
            // `MAX` once egui has nothing more to draw, letting the loop idle.
            self.repaint_delay = viewport_output
                .get(&ViewportId::ROOT)
                .map_or(Duration::MAX, |out| out.repaint_delay);

            // Partition viewport commands: Close vs. all others
            let (cmd_close, passthrough) =
                partition_close(viewport_output.into_values().flat_map(|out| out.commands));
            let close_requested = self.wm_close || cmd_close;
            self.wm_close = false;
            let mut actions = Vec::new();
            egui_winit::process_viewport_commands(
                &self.ctx,
                &mut self.viewport_info,
                passthrough,
                window,
                &mut actions,
            );

            let action = self.app.close_action(close_requested);
            if action == CloseAction::Stay {
                for (id, delta) in &textures_delta.set {
                    painter.set_texture(*id, delta);
                }
                // Skip the GL draw on a zero-sized surface (a 0-sized `Resized`
                // slips through on some compositors); `resize` no-ops there, so
                // the surface size wouldn't match `dims`. Texture deltas still
                // apply so the painter stays in sync for the next real frame.
                let dims: [u32; 2] = window.inner_size().into();
                if dims[0] > 0 && dims[1] > 0 {
                    let clipped = self.ctx.tessellate(shapes, pixels_per_point);
                    // SAFETY: the GL context is current; plain state-setting calls.
                    unsafe {
                        use glow::HasContext as _;
                        gl.clear_color(0.0, 0.0, 0.0, 0.0);
                        gl.clear(glow::COLOR_BUFFER_BIT);
                    }
                    painter.paint_primitives(dims, pixels_per_point, &clipped);
                    use glutin::surface::GlSurface as _;
                    let _ = win.gl_surface.swap_buffers(&keep.gl_context);
                }
                for id in &textures_delta.free {
                    painter.free_texture(*id);
                }
            }
            action
        };

        match action {
            CloseAction::Stay => {
                let delay = self.repaint_delay;
                let flow = if delay.is_zero() {
                    if let Some(w) = self.window() {
                        w.request_redraw();
                    }
                    ControlFlow::Poll
                } else if let Some(at) = Instant::now().checked_add(delay) {
                    ControlFlow::WaitUntil(at)
                } else {
                    ControlFlow::Wait
                };
                event_loop.set_control_flow(flow);
            }
            CloseAction::HideToTray => {
                self.hide();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            CloseAction::Quit => {
                if !self.force_quit.load(Ordering::SeqCst) {
                    ipc::send(&self.app.cmd, DaemonCommand::Shutdown);
                }
                event_loop.exit();
            }
        }
    }

    /// "Close to tray": drop only the window + EGL surface so the compositor
    /// unmaps it. The GL context, glow, painter, egui context, and tray live on.
    fn hide(&mut self) {
        if self.win.is_none() {
            return;
        }
        // Drop order matters here — see `WindowSurface`.
        self.win = None;
        self.hidden = true;
        log::info!("close to tray: window destroyed, staying resident");
    }

    /// Tray "Open": build a new window + surface and rebind the surviving GL
    /// context to it. No painter/texture rebuild — everything is still uploaded.
    fn show(&mut self, event_loop: &ActiveEventLoop) {
        if !self.hidden {
            return;
        }
        let Some(keep) = self.gl_keep.as_ref() else {
            return;
        };
        // Recreating the window/surface can transiently fail (compositor restart,
        // GPU reset, resource pressure). Stay hidden and resident so the next tray
        // "Open" retries, instead of panicking and taking the tray down with us.
        let win = match new_window_surface(event_loop, keep) {
            Ok(win) => win,
            Err(e) => {
                log::error!("tray open: window recreation failed, staying hidden: {e}");
                return;
            }
        };
        win.window.set_visible(true);
        win.window.request_redraw();
        // Rebuild egui input state against the fresh window: the compositor may
        // hand back a different size/scale than the destroyed one, and the old
        // state cached the previous window's geometry.
        if let Some(max_side) = self.painter.as_ref().map(|p| p.max_texture_side()) {
            self.egui_winit = Some(self.new_egui_state(event_loop, &win.window, max_side));
        }
        self.win = Some(win);
        self.hidden = false;
        log::info!("tray open: window recreated");
    }

    /// egui's winit input adapter, seeded from a window's current scale factor.
    fn new_egui_state(
        &self,
        event_loop: &ActiveEventLoop,
        window: &winit::window::Window,
        max_texture_side: usize,
    ) -> egui_winit::State {
        egui_winit::State::new(
            self.ctx.clone(),
            ViewportId::ROOT,
            event_loop,
            Some(window.scale_factor() as f32),
            event_loop.system_theme(),
            Some(max_texture_side),
        )
    }

    /// First-time setup: window, GL, painter, and egui input state.
    fn create(&mut self, event_loop: &ActiveEventLoop) {
        let (keep, win, gl) = create_gl(event_loop);
        let gl = Arc::new(gl);
        win.window.set_visible(true);
        let painter =
            egui_glow::Painter::new(Arc::clone(&gl), "", None, true).expect("create glow painter");
        let egui_winit = self.new_egui_state(event_loop, &win.window, painter.max_texture_side());
        self.painter = Some(painter);
        self.egui_winit = Some(egui_winit);
        self.gl_keep = Some(keep);
        self.win = Some(win);
        self.gl = Some(gl);
        self.hidden = false;
    }
}

impl ApplicationHandler<UserEvent> for WaylandApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.egui_winit.is_none() {
            if self.start_hidden {
                // Tray "Open" (`user_event`) creates everything lazily instead.
                self.start_hidden = false;
                return;
            }
            self.create(event_loop);
        } else if self.hidden {
            self.show(event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if self.hidden {
            return;
        }
        if matches!(event, WindowEvent::RedrawRequested) {
            self.redraw(event_loop);
            return;
        }
        if let WindowEvent::Resized(size) = &event {
            self.resize(*size);
        }
        // A decorated WM's title-bar close arrives here; remember it and let the
        // next frame's `close_action` decide (hide vs quit). Never exit directly.
        if matches!(event, WindowEvent::CloseRequested) {
            self.wm_close = true;
        }
        let repaint = match (self.egui_winit.as_mut(), self.win.as_ref()) {
            (Some(egui_winit), Some(win)) => {
                egui_winit.on_window_event(&win.window, &event).repaint
            }
            _ => false,
        };
        if repaint || self.wm_close {
            if let Some(w) = self.window() {
                w.request_redraw();
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        // The delay is advisory only: it wakes the loop, but `redraw` recomputes
        // the authoritative schedule from egui's frame output. Using it to set
        // `repaint_delay` here would pin the loop in `Poll` (see `redraw`).
        let UserEvent::Redraw(delay) = event;

        // Tray "Quit" works from any state, including while hidden (there is no
        // window to route a close through), so honour it here.
        if self.force_quit.load(Ordering::SeqCst) {
            event_loop.exit();
            return;
        }
        // Tray "Open": recreate the window if it was closed to the tray, or —
        // for a `--background` launch that never created one — build it now.
        if self.hide_state.wants_show.swap(false, Ordering::SeqCst) && self.hidden {
            if self.egui_winit.is_none() {
                self.create(event_loop);
            } else {
                self.show(event_loop);
            }
            return;
        }
        if !self.hidden && delay.is_zero() {
            if let Some(w) = self.window() {
                w.request_redraw();
            }
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) && !self.hidden {
            if let Some(w) = self.window() {
                w.request_redraw();
            }
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(painter) = self.painter.as_mut() {
            painter.destroy();
        }
    }
}

/// The winit window icon from the shared embedded SVG.
fn window_icon() -> Option<winit::window::Icon> {
    let icon = tray::app_icon();
    winit::window::Icon::from_rgba(icon.rgba, icon.width, icon.height).ok()
}

/// The borderless, transparent window attributes shared by first-create and
/// every reopen so the recreated window matches the original.
fn window_attrs() -> winit::window::WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland as _;
    let mut attrs = winit::window::WindowAttributes::default()
        .with_resizable(true)
        .with_inner_size(winit::dpi::LogicalSize::new(1320.0, 860.0))
        .with_min_inner_size(winit::dpi::LogicalSize::new(900.0, 600.0))
        .with_title(halod_shared::app::APP_DISPLAY_NAME)
        .with_name(halod_shared::app::APP_ID, "")
        .with_decorations(false)
        .with_transparent(true)
        // Keep hidden until the first paint to avoid a flash (egui#2279).
        .with_visible(false);
    if let Some(icon) = window_icon() {
        attrs = attrs.with_window_icon(Some(icon));
    }
    attrs
}

/// Persistent GL state that survives every hide/show. Keeping the GL *context*
/// alive is what preserves egui's uploaded textures across a reopen.
struct GlKeep {
    gl_display: glutin::display::Display,
    gl_config: glutin::config::Config,
    gl_context: glutin::context::PossiblyCurrentContext,
}

/// The transient window + EGL surface, dropped on hide. Field order is
/// load-bearing: the EGL surface MUST drop before the `wl_surface` (the winit
/// `window`), or destroying a surface whose `wl_surface` is already gone is a
/// Wayland protocol violation that crashes the compositor.
struct WindowSurface {
    gl_surface: glutin::surface::Surface<glutin::surface::WindowSurface>,
    window: winit::window::Window,
}

/// First-time setup: create the display, config, context, first window/surface,
/// and a glow context. Adapted from the `egui_glow` `pure_glow` example.
fn create_gl(event_loop: &ActiveEventLoop) -> (GlKeep, WindowSurface, glow::Context) {
    use glutin::context::NotCurrentGlContext as _;
    use glutin::display::{GetGlDisplay as _, GlDisplay as _};
    use glutin::prelude::GlSurface as _;

    let attrs = window_attrs();
    let template = glutin::config::ConfigTemplateBuilder::new()
        .prefer_hardware_accelerated(None)
        .with_depth_size(0)
        .with_stencil_size(0)
        .with_transparency(true);

    let (mut window, gl_config) = glutin_winit::DisplayBuilder::new()
        .with_preference(glutin_winit::ApiPreference::FallbackEgl)
        .with_window_attributes(Some(attrs.clone()))
        .build(event_loop, template, |mut configs| {
            configs
                .next()
                .expect("no matching glutin config for the window")
        })
        .expect("failed to create glutin config");

    let gl_display = gl_config.display();
    let raw = window
        .as_ref()
        .map(|w| w.window_handle().expect("window handle").as_raw());
    let ctx_attrs = glutin::context::ContextAttributesBuilder::new().build(raw);
    // Fall back to GLES if a core GL context is unavailable.
    let fallback = glutin::context::ContextAttributesBuilder::new()
        .with_context_api(glutin::context::ContextApi::Gles(None))
        .build(raw);
    // SAFETY: the raw window handle is valid for the lifetime of `window`.
    let not_current = unsafe {
        gl_display
            .create_context(&gl_config, &ctx_attrs)
            .or_else(|_| gl_display.create_context(&gl_config, &fallback))
            .expect("failed to create a GL context")
    };

    let window = window.take().unwrap_or_else(|| {
        glutin_winit::finalize_window(event_loop, attrs.clone(), &gl_config)
            .expect("failed to finalize window")
    });
    let gl_surface =
        make_surface(&gl_display, &gl_config, &window).expect("failed to create GL surface");
    let gl_context = not_current
        .make_current(&gl_surface)
        .expect("failed to make GL context current");
    let _ = gl_surface.set_swap_interval(
        &gl_context,
        glutin::surface::SwapInterval::Wait(NonZeroU32::MIN),
    );

    // SAFETY: the context is current; we only load GL function pointers.
    let gl = unsafe {
        glow::Context::from_loader_function(|s| {
            let s = std::ffi::CString::new(s).expect("gl proc name");
            gl_display.get_proc_address(&s)
        })
    };

    (
        GlKeep {
            gl_display,
            gl_config,
            gl_context,
        },
        WindowSurface { gl_surface, window },
        gl,
    )
}

/// Split egui's outgoing viewport commands into a close request (the × button
/// or a WM close egui relays as `Close`) and the rest to forward to winit.
/// `CancelClose` is dropped — the wrapper's own close is what we drive.
fn partition_close(
    commands: impl IntoIterator<Item = egui::ViewportCommand>,
) -> (bool, Vec<egui::ViewportCommand>) {
    let mut close = false;
    let mut passthrough = Vec::new();
    for cmd in commands {
        match cmd {
            egui::ViewportCommand::Close => close = true,
            egui::ViewportCommand::CancelClose => {}
            other => passthrough.push(other),
        }
    }
    (close, passthrough)
}

/// Build a new window + surface reusing the persistent display/config, and make
/// the surviving context current on it (used when reopening from the tray).
fn new_window_surface(
    event_loop: &ActiveEventLoop,
    keep: &GlKeep,
) -> Result<WindowSurface, Box<dyn std::error::Error>> {
    use glutin::context::PossiblyCurrentGlContext as _;
    use glutin::surface::GlSurface as _;

    let window = glutin_winit::finalize_window(event_loop, window_attrs(), &keep.gl_config)?;
    let gl_surface = make_surface(&keep.gl_display, &keep.gl_config, &window)?;
    keep.gl_context.make_current(&gl_surface)?;
    let _ = gl_surface.set_swap_interval(
        &keep.gl_context,
        glutin::surface::SwapInterval::Wait(NonZeroU32::MIN),
    );
    Ok(WindowSurface { gl_surface, window })
}

fn make_surface(
    display: &glutin::display::Display,
    config: &glutin::config::Config,
    window: &winit::window::Window,
) -> Result<glutin::surface::Surface<glutin::surface::WindowSurface>, Box<dyn std::error::Error>> {
    use glutin::display::GlDisplay as _;
    let (w, h): (u32, u32) = window.inner_size().into();
    let attrs = glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
        .build(
            window.window_handle()?.as_raw(),
            NonZeroU32::new(w).unwrap_or(NonZeroU32::MIN),
            NonZeroU32::new(h).unwrap_or(NonZeroU32::MIN),
        );
    // SAFETY: the window outlives the surface (dropped together, surface first).
    let surface = unsafe { display.create_window_surface(config, &attrs)? };
    Ok(surface)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::ViewportCommand;

    #[test]
    fn partition_close_flags_close_and_forwards_rest() {
        let (close, rest) = partition_close([
            ViewportCommand::Focus,
            ViewportCommand::Close,
            ViewportCommand::CancelClose,
            ViewportCommand::Title("x".into()),
        ]);
        assert!(close);
        // `Close` and `CancelClose` are consumed; the rest passes through in order.
        assert_eq!(
            rest,
            vec![ViewportCommand::Focus, ViewportCommand::Title("x".into())]
        );
    }

    #[test]
    fn partition_close_no_close_when_absent() {
        let (close, rest) = partition_close([ViewportCommand::CancelClose, ViewportCommand::Focus]);
        assert!(!close);
        assert_eq!(rest, vec![ViewportCommand::Focus]);
    }
}
