// SPDX-License-Identifier: GPL-3.0-or-later
//! The single render path shared by eframe (Windows/macOS) and the Linux
//! winit+glow loop: `App::draw`, the tour-key resolver, and the borderless
//! window's resize grips.

use crate::app::App;
use crate::domain;
use crate::domain::state::Page;

const DEPCHECK_GRACE_SECS: f64 = 4.0;

impl App {
    // Single render path shared by eframe (Windows/macOS) and winit+glow (Linux).
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Only re-clone when the watch channel reports a change.
        if self.ui.state.has_changed().unwrap_or_else(|_| {
            log::warn!("IPC state channel closed");
            false
        }) {
            self.state_cache = std::sync::Arc::new(self.ui.state.borrow_and_update().clone());
            crate::ui::screens::settings::apply_locale(&self.state_cache.gui.language);
        }
        let state = std::sync::Arc::clone(&self.state_cache);
        self.tray.sync(ctx, &state);
        // Close handling (hide-to-tray vs quit) is decided by `close_action` and
        // applied by each backend after `draw` — not here, so it isn't run twice.
        let connected = *self.ui.connected.borrow();
        let debug = self.ui.debug.borrow().clone();
        if self.ui.lcd_images.has_changed().unwrap_or_else(|_| {
            log::warn!("IPC lcd_images channel closed");
            false
        }) {
            self.lcd_images_cache =
                std::sync::Arc::new(self.ui.lcd_images.borrow_and_update().clone());
        }
        let lcd_images = std::sync::Arc::clone(&self.lcd_images_cache);
        if self.ui.plugin_assets.has_changed().unwrap_or_else(|_| {
            log::warn!("IPC plugin_assets channel closed");
            false
        }) {
            self.plugin_assets_cache =
                std::sync::Arc::new(self.ui.plugin_assets.borrow_and_update().clone());
        }
        let plugin_assets = std::sync::Arc::clone(&self.plugin_assets_cache);
        if self.ui.repo_updates.has_changed().unwrap_or_else(|_| {
            log::warn!("IPC repo_updates channel closed");
            false
        }) {
            self.repo_updates_cache = self.ui.repo_updates.borrow_and_update().clone();
        }
        if self.ui.plugin_updates.has_changed().unwrap_or_else(|_| {
            log::warn!("IPC plugin_updates channel closed");
            false
        }) {
            self.plugin_updates_cache = self.ui.plugin_updates.borrow_and_update().clone();
        }
        if self.ui.repo_branches.has_changed().unwrap_or_else(|_| {
            log::warn!("IPC repo_branches channel closed");
            false
        }) {
            self.repo_branches_cache = self.ui.repo_branches.borrow_and_update().clone();
        }
        let lcd_preview = if let Page::Device(ref id) = self.page {
            self.ui.lcd_frames.borrow().get(id).cloned()
        } else {
            None
        };
        // A delta reply is one small message per ~200ms; cloning it every
        // frame (as with a plain `.borrow()`) churns the allocator for
        // nothing on the ~60 idle frames between updates.
        if self.ui.lcd_editor_render.has_changed().unwrap_or(false) {
            self.lcd_editor_render_cache = self.ui.lcd_editor_render.borrow_and_update().clone();
        }
        let lcd_editor_render = if let Page::Device(ref id) = self.page {
            self.lcd_editor_render_cache
                .clone()
                .filter(|r| &r.device_id == id)
        } else {
            None
        };
        // A terminal (`Done`/`Failed`) is consumed as a one-shot edge: `Some`
        // only on the frame it newly arrives, so a retained stale terminal
        // can't clear a freshly-armed upload spinner.
        let lcd_upload_terminal = if self.ui.lcd_upload.has_changed().unwrap_or(false) {
            self.ui.lcd_upload.borrow_and_update().clone().filter(|p| {
                matches!(
                    p.stage,
                    halod_shared::types::LcdUploadStage::Done
                        | halod_shared::types::LcdUploadStage::Failed
                )
            })
        } else {
            None
        };
        let lcd_upload = self.ui.lcd_upload.borrow().clone();
        if self.ui.lcd_template.has_changed().unwrap_or(false) {
            self.pending_lcd_template = self.ui.lcd_template.borrow_and_update().clone();
        }
        let lcd_template = self.pending_lcd_template.take();
        let canvas_frame = if self.page == Page::Lighting {
            self.ui.canvas_frame.borrow().clone()
        } else {
            None
        };
        let running_apps = self.ui.running_apps.borrow().clone();
        let time = ctx.input(|i| i.time);

        // Suppress the startup healthcheck dialog until a settled snapshot.
        let (within_grace, grace_action) =
            self.depcheck_grace
                .advance(connected, time, DEPCHECK_GRACE_SECS);
        match grace_action {
            crate::ui::screens::depcheck::GraceAction::Recheck => {
                domain::actions::system::get_debug_info(&self.cmd)
            }
            crate::ui::screens::depcheck::GraceAction::RepaintAfter(secs) => {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(secs));
            }
            crate::ui::screens::depcheck::GraceAction::None => {}
        }

        // Move any daemon-pushed notifications into the toast stack.
        let incoming: Vec<_> = self
            .ui
            .notifications
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        crate::ui::screens::profile::observe_notifications(&mut self.profile_ui, &incoming);
        self.toasts.ingest(incoming, time);

        if let Some(ref frame) = canvas_frame {
            crate::ui::screens::canvas::ingest_frame(ctx, &mut self.canvas_ui, frame);
        } else if matches!(self.page, Page::Device(_)) {
            if let Some(frame) = self.ui.canvas_frame.borrow().as_ref() {
                crate::ui::screens::canvas::ingest_led_colors(&mut self.canvas_ui, frame);
            }
        }

        if time - self.last_sample >= 1.0 {
            self.last_sample = time;
            for s in domain::models::sensors::sensors(&state, true) {
                let h = self.sensor_history.entry(s.id).or_default();
                h.push_back(s.value as f32);
                if h.len() > domain::state::HISTORY_LEN {
                    h.pop_front();
                }
            }
            for dev in &state.devices {
                let Some(wr) = domain::models::device::effective_write_rate(&state, dev) else {
                    continue;
                };
                let h = self.write_rate_history.entry(dev.id.clone()).or_default();
                h.push_back(wr.current_bytes_per_sec);
                if h.len() > domain::state::HISTORY_LEN {
                    h.pop_front();
                }
            }
        }

        if !self.entered {
            if crate::ui::screens::radar::show(ui, &state, connected, time) {
                self.entered = true;
            }
            ctx.request_repaint();
            return;
        }

        // Prevent desktop bleed-through under transparent panels.
        let screen = ui.max_rect();
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            screen,
            12.0,
            crate::ui::theme::SIDEBAR_BG,
        );

        // Repaint bottom-right rounded corner, which egui's scrollbar would
        // otherwise square off.
        let border = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("win_border"),
        ));
        border.rect_filled(
            egui::Rect::from_min_max(
                screen.right_bottom() - egui::vec2(16.0, 16.0),
                screen.right_bottom(),
            ),
            egui::CornerRadius {
                nw: 0,
                ne: 0,
                sw: 0,
                se: 12,
            },
            crate::ui::theme::MAIN_BG,
        );
        border.rect_stroke(
            screen,
            12.0,
            egui::Stroke::new(1.0, crate::ui::theme::BORDER),
            egui::StrokeKind::Inside,
        );

        resize_grips(ctx);

        egui::Panel::top("title")
            .exact_size(46.0)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.painter().rect_filled(
                    ui.max_rect(),
                    egui::CornerRadius {
                        nw: 12,
                        ne: 12,
                        sw: 0,
                        se: 0,
                    },
                    crate::ui::theme::TITLE_BG,
                );
                crate::ui::shell::title_bar(
                    ui,
                    &state,
                    &self.cmd,
                    &mut self.profile_ui,
                    &mut self.page,
                );
            });

        if let Page::Device(id) = &self.page {
            if !state.devices.iter().any(|d| &d.id == id) {
                self.page = Page::Home;
            }
        }
        if let Page::Profile(name) = &self.page {
            if !state.profiles.available.contains(name) {
                self.page = Page::Home;
            }
        }

        egui::Panel::left("sidebar")
            .exact_size(236.0)
            .resizable(false)
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.painter().rect_filled(
                    ui.max_rect(),
                    egui::CornerRadius {
                        nw: 0,
                        ne: 0,
                        sw: 12,
                        se: 0,
                    },
                    crate::ui::theme::SIDEBAR_BG,
                );
                crate::ui::shell::sidebar(ui, &state, connected, &mut self.page);
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.painter().rect_filled(
                    ui.max_rect(),
                    egui::CornerRadius {
                        nw: 0,
                        ne: 0,
                        sw: 0,
                        se: 12,
                    },
                    crate::ui::theme::MAIN_BG,
                );
                match &self.page {
                    Page::Home => {
                        crate::ui::screens::home::show(
                            ui,
                            &state,
                            &self.cmd,
                            &mut self.show_hidden,
                            &mut self.variant,
                            &mut self.search,
                            &mut self.rename,
                            &mut self.confirm_remove,
                            &self.sensor_history,
                            &mut self.page,
                        );
                    }
                    Page::Device(id) => {
                        let id = id.clone();
                        crate::ui::screens::device::show(
                            ui,
                            &state,
                            &self.cmd,
                            &id,
                            &mut self.device_ui,
                            &mut self.page,
                            time,
                            debug.as_ref(),
                            &lcd_images,
                            lcd_preview,
                            lcd_upload,
                            lcd_upload_terminal,
                            lcd_template,
                            lcd_editor_render,
                            self.canvas_ui.led_colors(),
                            self.write_rate_history.get(&id),
                        );
                    }
                    Page::Cooling => {
                        crate::ui::screens::cooling::show(
                            ui,
                            &state,
                            &self.cmd,
                            &self.sensor_history,
                            time,
                            &mut self.page,
                        );
                    }
                    Page::Lighting => {
                        crate::ui::screens::lighting::show(
                            ui,
                            &state,
                            &self.cmd,
                            &mut self.lighting_ui,
                            &mut self.canvas_ui,
                            &mut self.effect_designer_ui,
                            canvas_frame.as_ref(),
                            time,
                            &mut self.page,
                        );
                    }
                    Page::EffectDesigner => {
                        self.effect_designer_ui.show(ui, &self.cmd, &mut self.page);
                    }
                    Page::Settings => {
                        crate::ui::screens::settings::show(
                            ui,
                            &state,
                            &self.cmd,
                            connected,
                            debug.as_ref(),
                            &mut self.settings_ui,
                        );
                    }
                    Page::Plugins => {
                        self.plugins_ui.show(
                            ui,
                            &state,
                            &self.cmd,
                            &plugin_assets,
                            &self.repo_updates_cache,
                            &self.plugin_updates_cache,
                            &self.repo_branches_cache,
                        );
                    }
                    Page::Integrations => {
                        self.integrations_ui.show(ui, &state, &self.cmd);
                    }
                    Page::Profile(name) => {
                        let name = name.clone();
                        crate::ui::screens::profile::show(
                            ui,
                            &state,
                            &self.cmd,
                            &mut self.profile_ui,
                            &name,
                            &mut self.page,
                            &running_apps,
                        );
                    }
                }
                // Daemon-down scrim over the whole content area.
                if !connected {
                    crate::ui::shell::daemon_overlay(ui);
                }

                // Modal overlays rendered unconditionally so they work from any page.
                crate::ui::screens::profile::add_modal(
                    ui.ctx(),
                    &state,
                    &self.cmd,
                    &mut self.profile_ui,
                );
            });

        crate::ui::screens::depcheck::show(
            ctx,
            &state,
            &self.cmd,
            debug.as_ref(),
            connected,
            within_grace,
            &mut self.depcheck_ui,
        );

        // The tutorial tour: suppressed while the healthcheck dialog is up so
        // the two overlays don't fight over the screen.
        let depcheck_visible = crate::ui::screens::depcheck::visible(
            &state,
            debug.as_ref(),
            connected,
            within_grace,
            &self.depcheck_ui,
        );
        let tour_key = tour_key_for(
            &self.page,
            &self.device_ui,
            self.lighting_ui.tab,
            &self.tour,
            &state.gui.seen_tours,
        );
        crate::ui::tour::show(
            ctx,
            &mut self.tour,
            &state.gui.seen_tours,
            &self.cmd,
            tour_key,
            connected,
            depcheck_visible,
        );

        // Toasts overlay everything, including the daemon-down scrim.
        self.toasts.show(ctx);
    }
}

/// Which tour applies to whatever the user is currently viewing. The device
/// page's own tour (explaining the back link, rename, and tab bar) takes
/// priority over any tab tour until it's been seen.
fn tour_key_for(
    page: &Page,
    device_ui: &crate::ui::screens::device::DeviceUi,
    lighting_tab: usize,
    tour_state: &domain::tour::TourState,
    daemon_seen: &std::collections::BTreeSet<String>,
) -> Option<domain::tour::TourKey> {
    use crate::ui::screens::lighting::TAB_CANVAS;
    match page {
        Page::Home => Some(domain::tour::TourKey::PageHome),
        Page::Lighting if lighting_tab == TAB_CANVAS => Some(domain::tour::TourKey::PageCanvas),
        Page::Lighting => Some(domain::tour::TourKey::PageLighting),
        Page::Cooling => Some(domain::tour::TourKey::PageCooling),
        Page::Settings => Some(domain::tour::TourKey::PageSettings),
        Page::Profile(_) => Some(domain::tour::TourKey::PageProfile),
        Page::Device(_) => {
            if domain::tour::is_seen(tour_state, daemon_seen, domain::tour::TourKey::PageDevice) {
                device_ui.tour_key()
            } else {
                Some(domain::tour::TourKey::PageDevice)
            }
        }
        Page::EffectDesigner => Some(domain::tour::TourKey::EffectDesigner),
        Page::Plugins => None,
        Page::Integrations => None,
    }
}

// Borderless-window resize grips along edges and corners.
fn resize_grips(ctx: &egui::Context) {
    use egui::{CursorIcon, Id, Rect, ResizeDirection, Sense, Vec2, ViewportCommand};

    let screen = ctx.content_rect();
    const M: f32 = 6.0; // edge thickness
    const C: f32 = 14.0; // corner square size

    // (sub-rect, direction, cursor)
    //
    // Corners must come after edges — egui picks the last-hit widget for
    // overlapping rects, so the corner squares must come after the full-span
    // edges to win the overlap and give a diagonal resize at the corners.
    let grips: [(Rect, ResizeDirection, CursorIcon); 8] = [
        // Edges.
        (
            Rect::from_min_max(screen.left_top(), screen.right_top() + Vec2::new(0.0, M)),
            ResizeDirection::North,
            CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(
                screen.left_bottom() - Vec2::new(0.0, M),
                screen.right_bottom(),
            ),
            ResizeDirection::South,
            CursorIcon::ResizeVertical,
        ),
        (
            Rect::from_min_max(screen.left_top(), screen.left_bottom() + Vec2::new(M, 0.0)),
            ResizeDirection::West,
            CursorIcon::ResizeHorizontal,
        ),
        (
            Rect::from_min_max(
                screen.right_top() - Vec2::new(M, 0.0),
                screen.right_bottom(),
            ),
            ResizeDirection::East,
            CursorIcon::ResizeHorizontal,
        ),
        // Corners last so they win the hit-test where they overlap the edges.
        (
            Rect::from_min_size(screen.left_top(), Vec2::splat(C)),
            ResizeDirection::NorthWest,
            CursorIcon::ResizeNwSe,
        ),
        (
            Rect::from_min_size(screen.right_top() - Vec2::new(C, 0.0), Vec2::splat(C)),
            ResizeDirection::NorthEast,
            CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_size(screen.left_bottom() - Vec2::new(0.0, C), Vec2::splat(C)),
            ResizeDirection::SouthWest,
            CursorIcon::ResizeNeSw,
        ),
        (
            Rect::from_min_size(screen.right_bottom() - Vec2::splat(C), Vec2::splat(C)),
            ResizeDirection::SouthEast,
            CursorIcon::ResizeNwSe,
        ),
    ];

    egui::Area::new(Id::new("resize_grips"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            for (i, (rect, dir, cursor)) in grips.into_iter().enumerate() {
                let resp = ui.interact(rect, Id::new(("resize_grip_region", i)), Sense::drag());
                if resp.hovered() || resp.dragged() {
                    ui.ctx().set_cursor_icon(cursor);
                }
                if resp.drag_started() {
                    ui.ctx()
                        .send_viewport_cmd(ViewportCommand::BeginResize(dir));
                    crate::ui::shell::arm_pointer_release_workaround(ui.ctx());
                }
            }
        });
}
