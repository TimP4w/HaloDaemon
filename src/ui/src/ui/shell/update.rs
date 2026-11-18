// SPDX-License-Identifier: GPL-3.0-or-later
//! The global application-update banner shown above every page.

use egui::{Color32, Pos2, Rect, Sense, Shape, Stroke, Vec2};
use halod_shared::update::UpdateError;

use crate::domain::update::{format_bytes, fraction, BannerEvent, BannerModel, UpdateAction};
use crate::ui::components::{self as widgets, BannerKind, ButtonKind};
use crate::ui::theme;

const ROW_H: f32 = 30.0;
const ICON: f32 = 16.0;
const STRIP_H: f32 = 3.0;
const PADDING: egui::Margin = egui::Margin {
    left: 16,
    right: 10,
    top: 10,
    bottom: 10,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Icon {
    Download,
    Spinner,
    Shield,
    Alert,
}

struct Content {
    danger: bool,
    icon: Icon,
    title: String,
    sub: String,
    meter: Option<String>,
    ghost: Option<(String, BannerEvent)>,
    primary: Option<(String, ButtonKind)>,
    close: bool,
    progress: Option<Option<f32>>,
}

fn error_text(error: UpdateError) -> String {
    match error {
        UpdateError::Network => t!("shell.update_error_network"),
        UpdateError::Signature => t!("shell.update_error_signature"),
        UpdateError::Hash => t!("shell.update_error_hash"),
        UpdateError::TooLarge => t!("shell.update_error_too_large"),
        UpdateError::Io => t!("shell.update_error_io"),
        UpdateError::Stale => t!("shell.update_error_stale"),
        UpdateError::Declined => t!("shell.update_error_declined"),
        UpdateError::Installer => t!("shell.update_error_installer"),
    }
    .to_string()
}

fn content(model: &BannerModel, windows: bool) -> Option<Content> {
    let current = env!("CARGO_PKG_VERSION");
    let base = |icon, title: String| Content {
        danger: false,
        icon,
        title,
        sub: String::new(),
        meter: None,
        ghost: None,
        primary: None,
        close: false,
        progress: None,
    };
    let release_notes = || {
        Some((
            t!("shell.update_release_notes").to_string(),
            BannerEvent::ReleaseNotes,
        ))
    };
    Some(match model {
        BannerModel::Hidden => return None,
        BannerModel::Available { version, action } => {
            let run = *action == UpdateAction::RunUpdater;
            Content {
                sub: if windows {
                    t!("shell.update_current", current = current)
                } else {
                    t!("shell.update_current_package", current = current)
                }
                .to_string(),
                ghost: if run { release_notes() } else { None },
                primary: Some((
                    if run {
                        t!("shell.update_now")
                    } else {
                        t!("shell.update_view_release")
                    }
                    .to_string(),
                    ButtonKind::Primary,
                )),
                close: true,
                ..base(
                    Icon::Download,
                    t!("shell.update_available_title", version = version).to_string(),
                )
            }
        }
        BannerModel::Downloading {
            version,
            done,
            total,
        } => Content {
            meter: Some(format_bytes(*done, *total)),
            ghost: Some((t!("shell.update_cancel").to_string(), BannerEvent::Cancel)),
            progress: Some(fraction(*done, *total)),
            ..base(
                Icon::Spinner,
                t!("shell.update_downloading_title", version = version).to_string(),
            )
        },
        BannerModel::Verifying { .. } => Content {
            sub: t!("shell.update_verifying_sub").to_string(),
            ..base(
                Icon::Spinner,
                t!("shell.update_verifying_title").to_string(),
            )
        },
        BannerModel::Consent { .. } => Content {
            sub: t!("shell.update_consent_sub").to_string(),
            ..base(Icon::Shield, t!("shell.update_consent_title").to_string())
        },
        BannerModel::Failed { version, error, .. } => Content {
            danger: true,
            sub: error_text(*error),
            ghost: release_notes(),
            primary: Some((t!("shell.update_retry").to_string(), ButtonKind::Secondary)),
            close: true,
            ..base(
                Icon::Alert,
                t!("shell.update_failed_title", version = version).to_string(),
            )
        },
    })
}

pub fn update_banner(ui: &mut egui::Ui, model: &BannerModel) -> Option<BannerEvent> {
    let c = content(model, cfg!(windows))?;
    let kind = if c.danger {
        BannerKind::Danger
    } else {
        BannerKind::Info
    };
    let (accent, _, _) = widgets::banner_palette(&kind);
    let mut event = None;
    let frame = widgets::banner_frame_with(ui, &kind, PADDING, |ui| {
        ui.set_width(ui.available_width());
        egui::Sides::new()
            .height(ROW_H)
            .shrink_left()
            .truncate()
            .show(
                ui,
                |ui| {
                    paint_icon(ui, c.icon, accent);
                    ui.add_space(theme::SPACE_7);
                    ui.label(
                        egui::RichText::new(&c.title)
                            .font(theme::subhead())
                            .color(theme::TEXT),
                    );
                    if !c.sub.is_empty() {
                        ui.add_space(theme::SPACE_6);
                        ui.label(
                            egui::RichText::new(&c.sub)
                                .font(theme::body_sm())
                                .color(theme::TEXT_DIM),
                        );
                    }
                },
                |ui| {
                    if c.close {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(ROW_H), Sense::hover());
                        let id = ui.scope_id().with("update_banner_close");
                        if widgets::glyph_button_at(ui, rect, id, widgets::paint_close_glyph)
                            .on_hover_text(t!("shell.update_dismiss"))
                            .clicked()
                        {
                            event = Some(BannerEvent::Dismiss);
                        }
                    }
                    if let Some((label, kind)) = &c.primary {
                        if widgets::button(ui, label, *kind, Vec2::new(0.0, ROW_H)).clicked() {
                            event = Some(BannerEvent::Primary);
                        }
                    }
                    if let Some((label, ghost_event)) = &c.ghost {
                        let size =
                            widgets::fit_size(ui, label, ButtonKind::Ghost, Vec2::new(0.0, ROW_H));
                        if widgets::button(ui, label, ButtonKind::Ghost, size).clicked() {
                            event = Some(*ghost_event);
                        }
                    }
                    if let Some(meter) = &c.meter {
                        ui.label(
                            egui::RichText::new(meter)
                                .font(theme::body_sm())
                                .color(theme::TEXT_DIM),
                        );
                    }
                },
            );
    });
    if let Some(progress) = c.progress {
        paint_strip(ui, frame.response.rect, progress);
    }
    event
}

fn paint_icon(ui: &mut egui::Ui, icon: Icon, color: Color32) {
    if icon == Icon::Spinner {
        widgets::spinner(ui, ICON);
        return;
    }
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(ICON), Sense::hover());
    let p = ui.painter();
    let at = |x: f32, y: f32| rect.min + Vec2::new(x, y) * (ICON / 16.0);
    let stroke = Stroke::new(1.5, color);
    let line = |pts: &[(f32, f32)]| {
        p.add(Shape::line(
            pts.iter().map(|&(x, y)| at(x, y)).collect(),
            stroke,
        ));
    };
    match icon {
        Icon::Download => {
            line(&[(8.0, 2.5), (8.0, 10.5)]);
            line(&[(4.8, 7.5), (8.0, 10.7), (11.2, 7.5)]);
            line(&[(3.0, 13.5), (13.0, 13.5)]);
        }
        Icon::Shield => line(&[
            (8.0, 1.8),
            (13.0, 3.8),
            (13.0, 7.8),
            (12.2, 10.7),
            (10.5, 12.8),
            (8.0, 14.2),
            (5.5, 12.8),
            (3.8, 10.7),
            (3.0, 7.8),
            (3.0, 3.8),
            (8.0, 1.8),
        ]),
        Icon::Alert => {
            p.circle_stroke(at(8.0, 8.0), 6.0 * ICON / 16.0, stroke);
            line(&[(8.0, 4.5), (8.0, 9.0)]);
            p.circle_filled(at(8.0, 11.2), 0.9, color);
        }
        Icon::Spinner => {}
    }
}

fn paint_strip(ui: &egui::Ui, frame: Rect, progress: Option<f32>) {
    let track = Rect::from_min_max(
        Pos2::new(frame.left() + 1.0, frame.bottom() - 1.0 - STRIP_H),
        Pos2::new(frame.right() - 1.0, frame.bottom() - 1.0),
    );
    let radius = egui::CornerRadius {
        sw: theme::RADIUS_MD as u8,
        se: theme::RADIUS_MD as u8,
        ..Default::default()
    };
    let p = ui.painter();
    p.rect_filled(track, radius, theme::TRACK);
    let (x0, x1) = match progress {
        Some(f) => (0.0, f.clamp(0.0, 1.0)),
        None => {
            ui.ctx().request_repaint();
            let t = (ui.input(|i| i.time) as f32 * 0.9).rem_euclid(1.0);
            widgets::indeterminate_window(t, 0.4)
        }
    };
    if x1 > x0 {
        let fill = Rect::from_min_max(
            Pos2::new(track.left() + track.width() * x0, track.top()),
            Pos2::new(track.left() + track.width() * x1, track.bottom()),
        );
        p.with_clip_rect(fill)
            .rect_filled(track, radius, theme::CYAN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version() -> String {
        "0.14.0".into()
    }

    #[test]
    fn available_on_windows_offers_update_and_release_notes() {
        let c = content(
            &BannerModel::Available {
                version: version(),
                action: UpdateAction::RunUpdater,
            },
            true,
        )
        .unwrap();
        assert_eq!(c.icon, Icon::Download);
        assert!(c.title.contains("0.14.0"));
        assert!(c.sub.contains(env!("CARGO_PKG_VERSION")));
        assert_eq!(c.primary.unwrap().0, "Update now");
        assert_eq!(c.ghost.unwrap().1, BannerEvent::ReleaseNotes);
        assert!(c.close && !c.danger);
    }

    #[test]
    fn available_elsewhere_points_at_the_package_manager() {
        let c = content(
            &BannerModel::Available {
                version: version(),
                action: UpdateAction::OpenReleasePage,
            },
            false,
        )
        .unwrap();
        assert!(c.sub.contains("package manager"));
        assert_eq!(c.primary.unwrap().0, "View release");
        assert!(c.ghost.is_none());
    }

    #[test]
    fn downloading_shows_progress_meter_and_cancel() {
        let c = content(
            &BannerModel::Downloading {
                version: version(),
                done: 23_400_000,
                total: Some(55_100_000),
            },
            true,
        )
        .unwrap();
        assert_eq!(c.icon, Icon::Spinner);
        assert_eq!(c.meter.as_deref(), Some("23.4 of 55.1 MB"));
        assert_eq!(c.ghost.unwrap().1, BannerEvent::Cancel);
        assert!(c.primary.is_none() && !c.close);
        assert!(c.progress.unwrap().is_some());
    }

    #[test]
    fn verifying_and_consent_offer_no_controls() {
        for (model, icon) in [
            (BannerModel::Verifying { version: version() }, Icon::Spinner),
            (BannerModel::Consent { version: version() }, Icon::Shield),
        ] {
            let c = content(&model, true).unwrap();
            assert_eq!(c.icon, icon);
            assert!(!c.sub.is_empty());
            assert!(c.ghost.is_none() && c.primary.is_none() && !c.close);
            assert!(c.progress.is_none());
        }
    }

    #[test]
    fn every_failure_has_its_own_danger_message() {
        let all = [
            UpdateError::Network,
            UpdateError::Signature,
            UpdateError::Hash,
            UpdateError::TooLarge,
            UpdateError::Io,
            UpdateError::Stale,
            UpdateError::Declined,
            UpdateError::Installer,
        ];
        let mut messages = std::collections::HashSet::new();
        for error in all {
            let c = content(
                &BannerModel::Failed {
                    version: version(),
                    error,
                    action: UpdateAction::RunUpdater,
                },
                true,
            )
            .unwrap();
            assert!(c.danger);
            assert_eq!(c.icon, Icon::Alert);
            assert!(!c.sub.contains("shell."), "untranslated: {}", c.sub);
            assert!(c.primary.as_ref().unwrap().1 == ButtonKind::Secondary);
            messages.insert(c.sub);
        }
        assert_eq!(messages.len(), all.len());
    }

    #[test]
    fn hidden_has_no_content() {
        assert!(content(&BannerModel::Hidden, true).is_none());
    }
}
