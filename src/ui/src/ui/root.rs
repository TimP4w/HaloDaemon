// SPDX-License-Identifier: GPL-3.0-or-later
//! The single render path shared by eframe (Windows/macOS) and the Linux
//! winit+glow loop: `App::draw`, the tour-key resolver, and the borderless
//! window's resize grips.

use crate::app::App;
use crate::domain;
use crate::domain::models::notifications::NotificationTarget;
use crate::domain::native_notification;
use crate::domain::state::Page;
use crate::runtime::ipc;
use crate::ui::components;
use crate::ui::components::toast::ToastEvent;
use crate::ui::screens::canvas;
use crate::ui::screens::cooling;
use crate::ui::screens::depcheck;
use crate::ui::screens::depcheck::GraceAction;
use crate::ui::screens::device;
use crate::ui::screens::device::DeviceUi;
use crate::ui::screens::device::FrameCtx;
use crate::ui::screens::home;
use crate::ui::screens::onboarding;
use crate::ui::screens::onboarding::Outcome;
use crate::ui::screens::plugins::PluginsCtx;
use crate::ui::screens::profile;
use crate::ui::screens::settings;
use crate::ui::screens::startup;
use crate::ui::shell;
use crate::ui::theme;
use crate::ui::theme::BORDER;
use crate::ui::theme::MAIN_BG;
use crate::ui::theme::RADIUS_LG;
use crate::ui::theme::SIDEBAR_BG;
use crate::ui::theme::TITLE_BG;

const DEPCHECK_GRACE_SECS: f64 = 4.0;

impl App {
    pub(crate) fn sync_state(&mut self) {
        if self.ui.state.poll() {
            settings::apply_locale(&self.ui.state.get().gui.language);
        }
    }

    #[cfg(windows)]
    pub(crate) fn draw_background(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        self.sync_state();
        self.quit_when_installing(ctx);
        self.tray.sync(ctx, self.ui.state.get());
    }

    fn quit_when_installing(&self, ctx: &egui::Context) {
        if self.updater.phase() == domain::update::UpdaterPhase::Installing
            && !self.force_quit.load(std::sync::atomic::Ordering::SeqCst)
        {
            domain::tray::quit(ctx, &self.cmd, &self.force_quit, &self.hide_state);
        }
    }

    // Single render path shared by eframe (Windows/macOS) and winit+glow (Linux).
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.sync_state();
        let state = self.ui.state.arc();
        let connected = *self.ui.connected.borrow();
        let onboarding_active = onboarding_is_active(
            state.gui_present,
            crate::ui::screens::plugins::onboarding_pending(&state.gui.seen_tours),
            self.onboarding_completed,
        );
        self.tray.sync(ctx, &state);
        // Close handling (hide-to-tray vs quit) is decided by `close_action` and
        // applied by each backend after `draw` — not here, so it isn't run twice.
        if self.ui.debug.poll() {
            self.settings_ui.debug_info_arrived();
            let failing: Vec<_> = self
                .ui
                .debug
                .get()
                .as_ref()
                .map(|i| depcheck::failing(i).iter().map(|d| d.id).collect())
                .unwrap_or_default();
            self.depcheck_grace.observe(&failing);
        }
        self.ui.udev_rules.poll();
        self.ui.lcd_images.poll();
        self.ui.plugin_assets.poll();
        self.ui.serial_ports.poll();
        self.ui.lcd_editor_render.poll();
        self.ui.canvas_frame.poll();
        self.ui.running_apps.poll();
        let debug = self.ui.debug.get().as_ref();
        let udev_rules = self.ui.udev_rules.get().as_ref();
        let lcd_images = self.ui.lcd_images.arc();
        let plugin_assets = self.ui.plugin_assets.arc();
        let lcd_preview = if let Page::Device(ref id) = self.page {
            self.ui.lcd_frames.borrow().get(id).cloned()
        } else {
            None
        };
        let lcd_editor_render = if let Page::Device(ref id) = self.page {
            self.ui
                .lcd_editor_render
                .get()
                .clone()
                .filter(|r| r.device_id == id)
        } else {
            None
        };
        // A terminal (`Done`/`Failed`) is consumed as a one-shot edge: `Some`
        // only on the frame it newly arrives, so a retained stale terminal
        // can't clear a freshly-armed upload spinner.
        let lcd_upload_terminal = self
            .ui
            .lcd_upload
            .poll()
            .then(|| self.ui.lcd_upload.get().clone())
            .flatten()
            .filter(|p| {
                matches!(
                    p.stage,
                    halod_shared::types::LcdUploadStage::Done
                        | halod_shared::types::LcdUploadStage::Failed
                )
            });
        let lcd_upload = self.ui.lcd_upload.get().clone();
        if let Some(template) = ipc::take_changed(&mut self.ui.lcd_template, "lcd_template") {
            self.pending_lcd_template = template;
        }
        let lcd_template = self.pending_lcd_template.take();
        let canvas_frame = self.ui.canvas_frame.get().as_ref();
        let running_apps = self.ui.running_apps.get();
        let time = ctx.input(|i| i.time);

        // Suppress the startup healthcheck dialog until a settled snapshot.
        let (within_grace, grace_action) =
            self.depcheck_grace
                .advance(connected, time, DEPCHECK_GRACE_SECS);
        match grace_action {
            GraceAction::Recheck => ipc::send(
                &self.cmd,
                halod_shared::commands::DaemonCommand::GetDebugInfo,
            ),
            GraceAction::RepaintAfter(secs) => {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(secs));
            }
            GraceAction::None => {}
        }

        // Move any daemon-pushed notifications into the toast stack.
        let mut incoming: Vec<_> = self
            .ui
            .notifications
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        // Native delivery already happened at IPC ingestion. This queue is
        // drained on a frame only for in-app toasts and UI observers; a hidden
        // Wayland tray application may not receive frames at all.
        if onboarding_active {
            incoming.retain(|notification| {
                !matches!(
                    notification.code,
                    halod_shared::types::NotificationCode::PluginRecommended { .. }
                )
            });
        }
        self.plugins_ui.observe_notifications(&incoming);
        profile::observe_notifications(&mut self.profile_ui, &incoming);
        self.toasts.ingest(incoming, time);

        if self.page == Page::Lighting {
            if let Some(frame) = canvas_frame {
                canvas::ingest_frame(ctx, &mut self.canvas_ui, frame);
            }
        } else if matches!(self.page, Page::Device(_)) {
            if let Some(frame) = canvas_frame {
                canvas::ingest_led_colors(&mut self.canvas_ui, frame);
            }
        }

        if time - self.last_sample >= 1.0 {
            self.last_sample = time;
            let sensors = domain::models::sensors::sensors(&state);
            let sensor_ids = sensors
                .iter()
                .map(|s| s.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            self.sensor_history
                .retain(|id, _| sensor_ids.contains(id.as_str()));
            for s in sensors {
                let h = self.sensor_history.entry(s.id).or_default();
                h.push_back(s.value as f32);
                if h.len() > domain::state::HISTORY_LEN {
                    h.pop_front();
                }
            }
            let device_ids = state
                .devices
                .iter()
                .map(|dev| dev.id.as_str())
                .collect::<std::collections::HashSet<_>>();
            self.write_rate_history
                .retain(|id, _| device_ids.contains(id.as_str()));
            for dev in &state.devices {
                let Some(wr) = domain::models::device::effective_write_rate(&state, dev) else {
                    continue;
                };
                let h = self.write_rate_history.entry(dev.id.clone()).or_default();
                h.push_back(wr.current_bytes_per_sec);
                if h.len() > domain::state::WRITE_RATE_HISTORY_LEN {
                    h.pop_front();
                }
            }
        }

        if !self.entered {
            if startup::show(ui, &state, connected) {
                self.entered = true;
            }
            ctx.request_repaint();
            return;
        }

        // Prevent desktop bleed-through under transparent panels.
        let screen = ui.max_rect();
        ctx.layer_painter(egui::LayerId::background())
            .rect_filled(screen, RADIUS_LG, SIDEBAR_BG);

        // Repaint bottom-right rounded corner, which egui's scrollbar would
        // otherwise square off.
        let border = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("win_border"),
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
            MAIN_BG,
        );
        border.rect_stroke(
            screen,
            RADIUS_LG,
            egui::Stroke::new(1.0, BORDER),
            egui::StrokeKind::Inside,
        );

        resize_grips(ctx);

        let update_model = if onboarding_active || domain::tour::is_active(&self.tour) {
            domain::update::BannerModel::Hidden
        } else {
            domain::update::banner_model(
                &state.update,
                &self.updater.phase(),
                self.update_dismissed.as_deref(),
                self.update_action,
            )
        };
        let update_event = egui::Panel::top("title")
            .exact_size(52.0)
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
                    TITLE_BG,
                );
                shell::title_bar(
                    ui,
                    &state,
                    &self.cmd,
                    &mut self.profile_ui,
                    &mut self.page,
                    udev_rules,
                    &mut self.toasts,
                    &update_model,
                    &mut self.update_open,
                )
            })
            .inner;
        apply_update_event(
            update_event,
            &update_model,
            &mut self.updater,
            &mut self.update_dismissed,
            self.update_action,
            ctx,
        );

        if let Page::Device(id) = &self.page {
            if !state.devices.iter().any(|d| d.id == id) {
                self.page = Page::Home;
            }
        }
        if let Page::Profile(name) = &self.page {
            if !state.profiles.available.contains(name) {
                self.page = Page::Home;
            }
        }

        // Resolve the page tour early so its anchors can be registered during
        // page rendering, but onboarding always has first-run precedence.
        let tour_key = tour_key_for(
            &self.page,
            &self.device_ui,
            self.lighting_ui.tab,
            &self.tour,
            &state.gui.seen_tours,
        );
        if state.gui_present {
            domain::tour::reconcile_seen(&mut self.tour, &state.gui.seen_tours);
        }
        if state.gui_present && !onboarding_active {
            if let Some(key) = tour_key {
                domain::tour::maybe_start(&mut self.tour, &state.gui.seen_tours, key);
            }
        }
        // An already-active tour is suspended while onboarding is pending. It
        // resumes on the frame after onboarding is explicitly completed.
        let tour_active = !onboarding_active && domain::tour::is_active(&self.tour);

        self.quit_when_installing(ctx);

        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| {
                ui.painter().rect_filled(
                    ui.max_rect(),
                    egui::CornerRadius {
                        nw: 0,
                        ne: 0,
                        sw: 12,
                        se: 12,
                    },
                    MAIN_BG,
                );
                theme::top_right_halo(ui.painter(), ui.max_rect(), time as f32);
                ctx.request_repaint_after(std::time::Duration::from_millis(33));
                match &self.page {
                    Page::Home => {
                        home::show(
                            ui,
                            &state,
                            &self.cmd,
                            &mut self.home_ui,
                            &self.sensor_history,
                            &mut self.page,
                            time,
                            !tour_active && !onboarding_active,
                        );
                    }
                    Page::Device(id) => {
                        let id = id.clone();
                        let frame = FrameCtx {
                            state: &state,
                            cmd: &self.cmd,
                            time,
                            debug,
                            lcd_images: &lcd_images,
                            lcd_preview,
                            lcd_upload,
                            lcd_upload_terminal,
                            lcd_template,
                            lcd_editor_render,
                            led_colors: self.canvas_ui.led_colors(),
                            write_rate_history: &self.write_rate_history,
                            plugin_assets: &plugin_assets,
                            sensor_history: &self.sensor_history,
                        };
                        device::show(ui, frame, &id, &mut self.device_ui, &mut self.page);
                    }
                    Page::Cooling => {
                        cooling::show(ui, &state, &self.cmd, &mut self.page, &mut self.cooling_ui);
                    }
                    Page::Lighting => {
                        crate::ui::screens::lighting::show(
                            ui,
                            &state,
                            &self.cmd,
                            &mut self.lighting_ui,
                            &mut self.canvas_ui,
                            &mut self.effect_designer_ui,
                            canvas_frame,
                            time,
                            &mut self.page,
                        );
                    }
                    Page::EffectDesigner => {
                        self.effect_designer_ui.show(ui, &self.cmd, &mut self.page);
                    }
                    Page::Settings => {
                        settings::show(
                            ui,
                            &state,
                            &self.cmd,
                            connected,
                            debug,
                            &mut self.settings_ui,
                            &mut self.home_ui.show_hidden,
                        );
                    }
                    Page::Plugins => {
                        if !self.serial_ports_requested {
                            self.serial_ports_requested = true;
                            ipc::send(
                                &self.cmd,
                                halod_shared::commands::DaemonCommand::ListSerialPorts,
                            );
                        }
                        let cx = PluginsCtx {
                            state: &state,
                            cmd: &self.cmd,
                            assets: &plugin_assets,
                            udev: udev_rules,
                            serial_ports: self.ui.serial_ports.get(),
                        };
                        self.plugins_ui.show(ui, &cx, &mut self.page);
                    }
                    Page::Profile(name) => {
                        let name = name.clone();
                        profile::show(
                            ui,
                            &state,
                            &self.cmd,
                            &mut self.profile_ui,
                            &name,
                            &mut self.page,
                            running_apps,
                        );
                    }
                }
                // Daemon-down scrim over the whole content area.
                if !connected {
                    shell::daemon_overlay(ui);
                }

                // Modal overlays rendered unconditionally so they work from any page.
                if !tour_active && !onboarding_active {
                    profile::add_modal(ui.ctx(), &state, &self.cmd, &mut self.profile_ui);
                }
            });

        // Onboarding owns first-run health and plugin selection, so their
        // standalone surfaces are suppressed for this session.
        let mut onboarding_shown = false;
        if onboarding_active {
            use crate::ui::screens::plugins as plugins_screen;
            use halod_shared::commands::DaemonCommand::MarkTourSeen;
            let recs = &state.plugins.recommendations;
            onboarding_shown = true;
            self.depcheck_ui.dismiss_for_session();
            let outcome =
                onboarding::show(ctx, &state, debug, &self.cmd, &mut self.onboarding_ui, time);
            for id in self.onboarding_ui.take_enable_requests() {
                self.plugins_ui.enable_prompt.request_consented(&id);
            }
            if !matches!(outcome, Outcome::Pending) {
                self.onboarding_completed = true;
                ipc::send(
                    &self.cmd,
                    MarkTourSeen {
                        tour: plugins_screen::ONBOARDING_KEY.to_string(),
                    },
                );
                for rec in recs {
                    let key = plugins_screen::recommendation_key(rec);
                    self.recommendation_toasted.insert(key.clone());
                    ipc::send(&self.cmd, MarkTourSeen { tour: key });
                }
            }
        }

        self.plugins_ui
            .show_enable_prompt(ctx, &state.plugins.plugins, &self.cmd);

        if !onboarding_shown && !tour_active {
            depcheck::show(
                ctx,
                &state,
                &self.cmd,
                debug,
                connected,
                within_grace,
                &mut self.depcheck_ui,
            );
        }
        let depcheck_visible = !onboarding_shown
            && !tour_active
            && depcheck::visible(&state, debug, connected, within_grace, &self.depcheck_ui);

        if !onboarding_shown && !tour_active && !depcheck_visible {
            use crate::ui::screens::plugins as plugins_screen;
            use halod_shared::commands::DaemonCommand::MarkTourSeen;
            let toasts = plugins_screen::recommendation_toasts(
                &state.plugins.recommendations,
                &state.gui.seen_tours,
                &mut self.recommendation_toasted,
                (time * 1000.0) as u64,
            );
            if !toasts.is_empty() {
                let mut notifications = Vec::with_capacity(toasts.len());
                for (key, notification) in toasts {
                    ipc::send(&self.cmd, MarkTourSeen { tour: key });
                    notifications.push(notification);
                }
                show_native_notifications(&notifications);
                self.toasts.ingest(notifications, time);
            }
        }

        crate::ui::tour::show(
            ctx,
            &mut self.tour,
            &state.gui.seen_tours,
            &self.cmd,
            tour_key,
            connected,
            depcheck_visible || onboarding_shown || !state.gui_present,
        );

        // Toasts overlay everything, including the daemon-down scrim.
        if let Some(event) = self.toasts.show(ctx) {
            match event {
                ToastEvent::Open(note) => {
                    match crate::domain::models::notifications::notification_target(
                        &note.code,
                        &state.devices,
                    ) {
                        Some(NotificationTarget::Page(page)) => self.page = page,
                        Some(NotificationTarget::Plugin(id)) => {
                            self.plugins_ui.select_plugin(&id);
                            self.page = Page::Plugins;
                        }
                        None => {}
                    }
                }
                ToastEvent::Details(note) => {
                    if let Some(detail) = note.code.detail() {
                        let (title, _) =
                            crate::domain::models::notifications::notification_text(&note.code);
                        self.issue_details_modal = Some((title, detail.to_owned()));
                    }
                }
            }
        }
        if !domain::tour::is_active(&self.tour) && !onboarding_active {
            components::issue_modal_slot(ctx, "issue_details", &mut self.issue_details_modal);
        }
    }
}

fn apply_update_event(
    event: Option<domain::update::BannerEvent>,
    model: &domain::update::BannerModel,
    updater: &mut domain::update::Updater,
    dismissed: &mut Option<String>,
    action: domain::update::UpdateAction,
    ctx: &egui::Context,
) {
    use domain::update::BannerEvent;
    #[cfg(not(windows))]
    let _ = ctx;
    let Some(version) = model.version().map(str::to_owned) else {
        return;
    };
    let open_release = || {
        let _ = webbrowser::open(&halod_shared::app::release_page_url(&version));
    };
    match event {
        Some(BannerEvent::Primary) => match action {
            #[cfg(windows)]
            domain::update::UpdateAction::RunUpdater => updater.start(&version, ctx),
            _ => open_release(),
        },
        Some(BannerEvent::ReleaseNotes) => open_release(),
        Some(BannerEvent::Cancel) => updater.cancel(),
        Some(BannerEvent::Dismiss) => {
            updater.cancel();
            *dismissed = Some(version);
        }
        None => {}
    }
}

pub(crate) fn show_native_notifications(notifications: &[halod_shared::types::Notification]) {
    for notification in notifications {
        if !notification.show_native {
            continue;
        }
        let (title, message) =
            crate::domain::models::notifications::notification_text(&notification.code);
        native_notification::show(&title, &message, notification.code.severity());
    }
}

/// Which tour applies to whatever the user is currently viewing. The device
/// page's own tour (explaining the back link, rename, and tab bar) takes
/// priority over any tab tour until it's been seen.
fn tour_key_for(
    page: &Page,
    device_ui: &DeviceUi,
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
        Page::Plugins => Some(domain::tour::TourKey::PagePlugins),
    }
}

fn onboarding_is_active(gui_present: bool, pending: bool, completed: bool) -> bool {
    gui_present && pending && !completed
}

#[cfg(test)]
mod hydration_tests {
    use super::onboarding_is_active;

    #[test]
    fn onboarding_waits_for_authoritative_snapshot() {
        assert!(!onboarding_is_active(false, true, false));
        assert!(onboarding_is_active(true, true, false));
        assert!(!onboarding_is_active(true, false, false));
        assert!(!onboarding_is_active(true, true, true));
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

    egui::Area::new(Id::unique("resize_grips"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            for (i, (rect, dir, cursor)) in grips.into_iter().enumerate() {
                let resp = ui.interact(rect, Id::unique(("resize_grip_region", i)), Sense::drag());
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
