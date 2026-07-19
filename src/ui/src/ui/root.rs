// SPDX-License-Identifier: GPL-3.0-or-later
//! The single render path shared by eframe (Windows/macOS) and the Linux
//! winit+glow loop: `App::draw`, the tour-key resolver, and the borderless
//! window's resize grips.

use crate::app::App;
use crate::domain;
use crate::domain::state::Page;

const DEPCHECK_GRACE_SECS: f64 = 4.0;

impl App {
    pub(crate) fn accept_state(&mut self, state: crate::domain::topic_store::TopicStore) {
        crate::ui::screens::settings::apply_locale(&state.gui.language);
        self.state_cache = std::sync::Arc::new(state);
    }

    #[cfg(windows)]
    pub(crate) fn draw_background(&mut self, ctx: &egui::Context) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));
        if let Some(state) = crate::runtime::ipc::take_changed(&mut self.ui.state, "state") {
            self.accept_state(state);
        }
        self.tray.sync(ctx, &self.state_cache);
    }

    /// Re-deliver an active integrity alert when a tray-resident Linux window
    /// is opened again. The sticky in-app toast survives close-to-tray, so its
    /// native counterpart must not be limited to the toast's original ingest.
    #[cfg(target_os = "linux")]
    pub(crate) fn replay_integrity_native_on_reopen(&self) {
        let state = self.ui.state.borrow();
        let Some(alert) = crate::domain::models::plugin_issues::repository_integrity_alert(
            &state,
            &std::collections::HashSet::new(),
        ) else {
            return;
        };
        if !self.integrity_alert_notified.contains(&alert.key) {
            return;
        }
        let notification = halod_shared::types::Notification {
            code: halod_shared::types::NotificationCode::RepositoryIntegrityError {
                repository: alert.repository,
                package: alert.package,
                expected: alert.expected,
                actual: alert.actual,
                restore_slug: alert.restore_slug,
            },
            show_native: true,
            timestamp_ms: 0,
        };
        show_native_notifications(std::slice::from_ref(&notification));
    }

    // Single render path shared by eframe (Windows/macOS) and winit+glow (Linux).
    pub(crate) fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Only re-clone when the watch channel reports a change.
        if let Some(state) = crate::runtime::ipc::take_changed(&mut self.ui.state, "state") {
            self.accept_state(state);
        }
        let state = std::sync::Arc::clone(&self.state_cache);
        let connected = *self.ui.connected.borrow();
        let onboarding_active = onboarding_is_active(
            state.gui_present,
            crate::ui::screens::plugins::onboarding_pending(&state.gui.seen_tours),
            self.onboarding_completed,
        );
        self.tray.sync(ctx, &state);
        // Close handling (hide-to-tray vs quit) is decided by `close_action` and
        // applied by each backend after `draw` — not here, so it isn't run twice.
        if let Some(debug) = crate::runtime::ipc::take_changed(&mut self.ui.debug, "debug") {
            self.debug_cache = debug;
        }
        if let Some(status) =
            crate::runtime::ipc::take_changed(&mut self.ui.udev_rules, "udev_rules")
        {
            self.udev_rules_cache = status;
        }
        let debug = self.debug_cache.as_ref();
        let udev_rules = self.udev_rules_cache.as_ref();
        if let Some(imgs) = crate::runtime::ipc::take_changed(&mut self.ui.lcd_images, "lcd_images")
        {
            self.lcd_images_cache = std::sync::Arc::new(imgs);
        }
        let lcd_images = std::sync::Arc::clone(&self.lcd_images_cache);
        if let Some(assets) =
            crate::runtime::ipc::take_changed(&mut self.ui.plugin_assets, "plugin_assets")
        {
            self.plugin_assets_cache = std::sync::Arc::new(assets);
        }
        let plugin_assets = std::sync::Arc::clone(&self.plugin_assets_cache);
        if let Some(branches) =
            crate::runtime::ipc::take_changed(&mut self.ui.repo_branches, "repo_branches")
        {
            self.repo_branches_cache = branches;
        }
        if let Some(ports) =
            crate::runtime::ipc::take_changed(&mut self.ui.serial_ports, "serial_ports")
        {
            self.serial_ports_cache = ports;
        }
        let lcd_preview = if let Page::Device(ref id) = self.page {
            self.ui.lcd_frames.borrow().get(id).cloned()
        } else {
            None
        };
        // A delta reply is one small message per ~200ms; cloning it every
        // frame (as with a plain `.borrow()`) churns the allocator for
        // nothing on the ~60 idle frames between updates.
        if let Some(render) =
            crate::runtime::ipc::take_changed(&mut self.ui.lcd_editor_render, "lcd_editor_render")
        {
            self.lcd_editor_render_cache = render;
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
        let lcd_upload_changed =
            crate::runtime::ipc::take_changed(&mut self.ui.lcd_upload, "lcd_upload");
        let lcd_upload_terminal = lcd_upload_changed
            .as_ref()
            .and_then(|progress| progress.clone())
            .filter(|p| {
                matches!(
                    p.stage,
                    halod_shared::types::LcdUploadStage::Done
                        | halod_shared::types::LcdUploadStage::Failed
                )
            });
        if let Some(progress) = lcd_upload_changed {
            self.lcd_upload_cache = progress;
        }
        let lcd_upload = self.lcd_upload_cache.clone();
        if let Some(template) =
            crate::runtime::ipc::take_changed(&mut self.ui.lcd_template, "lcd_template")
        {
            self.pending_lcd_template = template;
        }
        let lcd_template = self.pending_lcd_template.take();
        if let Some(frame) =
            crate::runtime::ipc::take_changed(&mut self.ui.canvas_frame, "canvas_frame")
        {
            self.canvas_frame_cache = frame;
        }
        let canvas_frame = self.canvas_frame_cache.as_ref();
        if let Some(apps) =
            crate::runtime::ipc::take_changed(&mut self.ui.running_apps, "running_apps")
        {
            self.running_apps_cache = apps;
        }
        let running_apps = &self.running_apps_cache;
        let time = ctx.input(|i| i.time);

        // Suppress the startup healthcheck dialog until a settled snapshot.
        let (within_grace, grace_action) =
            self.depcheck_grace
                .advance(connected, time, DEPCHECK_GRACE_SECS);
        match grace_action {
            crate::ui::screens::depcheck::GraceAction::Recheck => crate::runtime::ipc::send(
                &self.cmd,
                halod_shared::commands::DaemonCommand::GetDebugInfo,
            ),
            crate::ui::screens::depcheck::GraceAction::RepaintAfter(secs) => {
                ctx.request_repaint_after(std::time::Duration::from_secs_f64(secs));
            }
            crate::ui::screens::depcheck::GraceAction::None => {}
        }

        // Move any daemon-pushed notifications into the toast stack.
        let mut incoming: Vec<_> = self
            .ui
            .notifications
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        // Native delivery belongs to the same authoritative ingestion point as
        // in-app toasts. Doing it in the IPC reader made delivery depend on a
        // background transport thread and obscured whether an event had
        // actually reached the UI. This path also runs while the Wayland
        // window is destroyed and the application is resident in the tray.
        show_native_notifications(&incoming);
        if onboarding_active {
            incoming.retain(|notification| {
                !matches!(
                    notification.code,
                    halod_shared::types::NotificationCode::PluginContentChanged { .. }
                        | halod_shared::types::NotificationCode::PluginRecommended { .. }
                )
            });
        }
        crate::ui::screens::profile::observe_notifications(&mut self.profile_ui, &incoming);
        self.toasts.ingest(incoming, time);

        // Repository validation failures are part of the state snapshot rather
        // than a transient daemon event, so synthesize a notification once per
        // distinct mismatch episode. This also catches startup failures that
        // happened before the GUI connected.
        if let Some(alert) = crate::domain::models::plugin_issues::repository_integrity_alert(
            &state,
            &self.integrity_alert_notified,
        ) {
            self.integrity_alert_notified.insert(alert.key);
            let code = halod_shared::types::NotificationCode::RepositoryIntegrityError {
                repository: alert.repository,
                package: alert.package,
                expected: alert.expected,
                actual: alert.actual,
                restore_slug: alert.restore_slug,
            };
            let notification = halod_shared::types::Notification {
                code,
                show_native: true,
                timestamp_ms: (time * 1000.0) as u64,
            };
            show_native_notifications(std::slice::from_ref(&notification));
            self.toasts.ingest([notification], time);
        }

        if self.page == Page::Lighting {
            if let Some(frame) = canvas_frame {
                crate::ui::screens::canvas::ingest_frame(ctx, &mut self.canvas_ui, frame);
            }
        } else if matches!(self.page, Page::Device(_)) {
            if let Some(frame) = canvas_frame {
                crate::ui::screens::canvas::ingest_led_colors(&mut self.canvas_ui, frame);
            }
        }

        if time - self.last_sample >= 1.0 {
            self.last_sample = time;
            let sensors = domain::models::sensors::sensors(&state, true);
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

        // After the radar gate (which returns before toasts.show) so it renders.
        if !onboarding_active {
            let quarantine = crate::ui::screens::plugins::quarantine_toasts(
                &state.plugins.plugins,
                &state.plugins.updates,
                &mut self.quarantine_toasted,
                (time * 1000.0) as u64,
            );
            show_native_notifications(&quarantine);
            self.toasts.ingest(quarantine, time);
        }

        // Prevent desktop bleed-through under transparent panels.
        let screen = ui.max_rect();
        ctx.layer_painter(egui::LayerId::background()).rect_filled(
            screen,
            crate::ui::theme::RADIUS_LG,
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
            crate::ui::theme::RADIUS_LG,
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
                crate::ui::shell::sidebar(
                    ui,
                    &state,
                    connected,
                    &mut self.page,
                    &state.plugins.updates,
                    udev_rules,
                );
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
                crate::ui::theme::top_right_halo(ui.painter(), ui.max_rect(), time as f32);
                ctx.request_repaint_after(std::time::Duration::from_millis(33));
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
                            &mut self.conflict_resolve,
                            &self.sensor_history,
                            &mut self.page,
                            !tour_active && !onboarding_active,
                        );
                    }
                    Page::Device(id) => {
                        let id = id.clone();
                        let frame = crate::ui::screens::device::FrameCtx {
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
                        };
                        crate::ui::screens::device::show(
                            ui,
                            frame,
                            &id,
                            &mut self.device_ui,
                            &mut self.page,
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
                            canvas_frame,
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
                            debug,
                            &mut self.settings_ui,
                        );
                    }
                    Page::Plugins => {
                        self.plugins_ui.show(
                            ui,
                            &state,
                            &self.cmd,
                            &plugin_assets,
                            &state.plugins.repo_updates,
                            &state.plugins.updates,
                            &self.repo_branches_cache,
                            udev_rules,
                        );
                    }
                    Page::Integrations => {
                        if !self.serial_ports_requested {
                            self.serial_ports_requested = true;
                            crate::runtime::ipc::send(
                                &self.cmd,
                                halod_shared::commands::DaemonCommand::ListSerialPorts,
                            );
                        }
                        self.integrations_ui.show(
                            ui,
                            &state,
                            &self.cmd,
                            &plugin_assets,
                            &self.serial_ports_cache,
                        );
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
                            running_apps,
                        );
                    }
                }
                // Daemon-down scrim over the whole content area.
                if !connected {
                    crate::ui::shell::daemon_overlay(ui);
                }

                // Modal overlays rendered unconditionally so they work from any page.
                if !tour_active && !onboarding_active {
                    crate::ui::screens::profile::add_modal(
                        ui.ctx(),
                        &state,
                        &self.cmd,
                        &mut self.profile_ui,
                    );
                }
            });

        // Onboarding owns first-run health, plugin selection, and authority
        // consent, so their standalone surfaces are suppressed for this session.
        let mut onboarding_shown = false;
        if onboarding_active {
            use crate::ui::screens::plugins as plugins_screen;
            use halod_shared::commands::DaemonCommand::MarkTourSeen;
            let recs = &state.plugins.recommendations;
            onboarding_shown = true;
            self.depcheck_ui.dismiss_for_session();
            let outcome = crate::ui::screens::onboarding::show(
                ctx,
                &state,
                debug,
                &self.cmd,
                &mut self.onboarding_ui,
                time,
            );
            if !matches!(outcome, crate::ui::screens::onboarding::Outcome::Pending) {
                self.onboarding_completed = true;
                crate::runtime::ipc::send(
                    &self.cmd,
                    MarkTourSeen {
                        tour: plugins_screen::ONBOARDING_KEY.to_string(),
                    },
                );
                for rec in recs {
                    let key = plugins_screen::recommendation_key(rec);
                    self.recommendation_toasted.insert(key.clone());
                    crate::runtime::ipc::send(&self.cmd, MarkTourSeen { tour: key });
                }
            }
        }

        if !onboarding_shown && !tour_active {
            crate::ui::screens::depcheck::show(
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
            && crate::ui::screens::depcheck::visible(
                &state,
                debug,
                connected,
                within_grace,
                &self.depcheck_ui,
            );

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
                    crate::runtime::ipc::send(&self.cmd, MarkTourSeen { tour: key });
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
                crate::ui::components::toast::ToastEvent::Details(note) => {
                    if let Some(detail) = note.code.detail() {
                        let (title, _) =
                            crate::domain::models::notifications::notification_text(&note.code);
                        self.issue_details_modal = Some((title, detail.to_owned()));
                    }
                }
                crate::ui::components::toast::ToastEvent::RestoreRepository(slug) => {
                    crate::runtime::ipc::send(
                        &self.cmd,
                        halod_shared::commands::DaemonCommand::UpdatePluginRepo { slug },
                    );
                }
            }
        }
        if !domain::tour::is_active(&self.tour) && !onboarding_active {
            crate::ui::components::issue_modal_slot(
                ctx,
                "issue_details",
                &mut self.issue_details_modal,
            );
        }
    }
}

pub(crate) fn show_native_notifications(notifications: &[halod_shared::types::Notification]) {
    for notification in notifications {
        if !notification.show_native {
            continue;
        }
        let (title, message) =
            crate::domain::models::notifications::notification_text(&notification.code);
        crate::domain::native_notification::show(&title, &message);
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
        Page::Plugins => Some(domain::tour::TourKey::PagePlugins),
        Page::Integrations => Some(domain::tour::TourKey::PageIntegrations),
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
