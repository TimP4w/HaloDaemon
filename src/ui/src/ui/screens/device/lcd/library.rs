// SPDX-License-Identifier: GPL-3.0-or-later
//! Media card: the Images/GIFs library grid, Video source picker, and the
//! background upload/native-file-picker plumbing they share.

use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;

use egui::{Color32, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{LcdStatus, LcdUploadProgress, LcdUploadStage};

use super::gif::decode_next_thumb;
use super::{DeviceUi, LcdMediaTab, PickerTarget, TabCtx};
use crate::ui::components as widgets;
use crate::ui::components::resolve_delete_confirm;
use crate::ui::theme;

/// Maximum library tiles shown (not counting the "None" slot).
const MAX_TILES: usize = 11;
const THUMB: f32 = 64.0;

/// Image/Video mode content (Editor mode bypasses this — see `show`).
pub(super) fn media_content(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    lcd: &LcdStatus,
) {
    widgets::card_titled(
        ui,
        &t!("lcd.media"),
        |_| {},
        |ui| match st.lcd.media_tab {
            LcdMediaTab::Video => video_section(ui, ctx, st, lcd),
            LcdMediaTab::Images => image_section(ui, ctx, st, id, lcd),
            LcdMediaTab::Template => {}
        },
    );
}

// ── Video section ─────────────────────────────────────────────────────────────

pub(super) fn video_section(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, lcd: &LcdStatus) {
    let label = lcd
        .video_path
        .as_deref()
        .and_then(|p| std::path::Path::new(p).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| t!("lcd.no_video_selected").to_string());

    ui.label(
        egui::RichText::new(&label)
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
    );
    ui.add_space(10.0);

    let enabled = ctx.state.health.ffmpeg_available;
    if widgets::button(
        ui,
        &t!("lcd.choose_video"),
        widgets::ButtonKind::Ghost,
        egui::vec2(140.0, 30.0),
    )
    .clicked()
        && enabled
    {
        spawn_picker(
            ui,
            st,
            PickerTarget::Video,
            &t!("lcd.filter_videos"),
            &["mp4", "mkv", "avi", "mov", "webm"],
        );
    }

    if !enabled {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(t!("lcd.ffmpeg_unavailable_host"))
                .font(theme::caption())
                .color(theme::TEXT_FAINT2),
        );
    }
}

// ── Image section ─────────────────────────────────────────────────────────────

/// The upload spinner's `upload_base`, cleared either by a fresh terminal
/// (`Done`/`Failed`) upload signal for this device, or — as a fallback — once
/// the refreshed library has grown past the size captured when the upload was
/// sent.
fn cleared_upload_base(
    base: Option<usize>,
    lib_len: usize,
    terminal: Option<&LcdUploadProgress>,
    device_id: &str,
) -> Option<usize> {
    if crate::ui::screens::device::is_terminal_upload_for(terminal, device_id) {
        return None;
    }
    base.filter(|&b| lib_len <= b)
}

/// Set the picked image directly on the device, first deactivating any
/// running template.
pub(super) fn image_section(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    lcd: &LcdStatus,
) {
    let active_template_id = ctx.state.lcd.engine.device_templates.get(id).cloned();
    let highlighted = lcd.active_image.as_deref();

    st.lcd.upload_base = cleared_upload_base(
        st.lcd.upload_base,
        ctx.lcd_images.len(),
        ctx.lcd_upload_terminal.as_ref(),
        id,
    );

    decode_next_thumb(ui, ctx, st, ctx.lcd_images.iter().take(MAX_TILES));

    // Grid: "None" tile + up to MAX_TILES library tiles.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);

        if draw_none_tile(ui, highlighted.is_none()) {
            select_none(ctx, st, id, active_template_id.as_deref());
        }

        for filename in ctx.lcd_images.iter().take(MAX_TILES) {
            let tex = st.lcd.image_cache.get(filename);
            let selected = highlighted == Some(filename.as_str());
            let action = draw_thumb_tile(ui, tex, selected, filename);
            if action.delete {
                st.lcd.confirm_delete_image = Some(filename.clone());
            } else if action.clicked {
                select_image(ctx, st, id, filename, active_template_id.as_deref());
            }
        }
    });

    ui.add_space(10.0);

    // Upload button (with in-flight spinner + daemon-reported progress, so a
    // long GIF re-encode doesn't look stuck).
    if st.lcd.upload_base.is_some() {
        widgets::button_loading(
            ui,
            &upload_label(ctx.lcd_upload.as_ref(), id),
            widgets::ButtonKind::Ghost,
            egui::vec2(180.0, 30.0),
        );
    } else if widgets::button(
        ui,
        &t!("lcd.upload_image_gif"),
        widgets::ButtonKind::Ghost,
        egui::vec2(180.0, 30.0),
    )
    .clicked()
    {
        spawn_picker(
            ui,
            st,
            PickerTarget::Image,
            &t!("lcd.filter_images"),
            &["png", "jpg", "jpeg", "gif"],
        );
    }
}

/// A self-contained image-library picker: a "None" tile, the library
/// thumbnails, and an "Upload Image / GIF…" button that browses for a new file.
/// Returns `Some(filename)` when the selection changes (empty string = "None").
/// Shared by the editor's background and Image-widget pickers; the Display tab's
/// [`image_section`] applies picks straight to the device instead, so it isn't
/// built on this.
pub(super) fn image_picker(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    current: &str,
) -> Option<String> {
    st.lcd.upload_base = cleared_upload_base(
        st.lcd.upload_base,
        ctx.lcd_images.len(),
        ctx.lcd_upload_terminal.as_ref(),
        id,
    );
    decode_next_thumb(ui, ctx, st, ctx.lcd_images.iter());

    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
        if draw_none_tile(ui, current.is_empty()) && !current.is_empty() {
            picked = Some(String::new());
        }
        for filename in ctx.lcd_images {
            let action = draw_thumb_tile(
                ui,
                st.lcd.image_cache.get(filename),
                current == filename,
                filename,
            );
            if action.delete {
                st.lcd.confirm_delete_image = Some(filename.clone());
                if current == filename {
                    picked = Some(String::new());
                }
            } else if action.clicked && current != filename {
                picked = Some(filename.clone());
            }
        }
    });

    ui.add_space(8.0);
    if st.lcd.upload_base.is_some() {
        widgets::button_loading(
            ui,
            &upload_label(ctx.lcd_upload.as_ref(), id),
            widgets::ButtonKind::Ghost,
            egui::vec2(180.0, 30.0),
        );
    } else if widgets::button(
        ui,
        &t!("lcd.upload_image_gif"),
        widgets::ButtonKind::Ghost,
        egui::vec2(180.0, 30.0),
    )
    .clicked()
    {
        spawn_picker(
            ui,
            st,
            PickerTarget::Image,
            &t!("lcd.filter_images"),
            &["png", "jpg", "jpeg", "gif"],
        );
    }
    picked
}

/// Confirmation modal for deleting an image from the library.
pub(super) fn delete_image_modal(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    let Some(filename) = st.lcd.confirm_delete_image.clone() else {
        return;
    };
    let (mut confirm, mut cancel) = (false, false);
    let dismissed = widgets::dialog(
        ui.ctx(),
        "lcd_delete_image",
        &t!("lcd.delete_image_title"),
        420.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("lcd.delete_confirm_body", name = filename))
                    .font(theme::body_md())
                    .color(theme::TEXT_MUT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("lcd.delete"),
                widgets::ButtonKind::Danger,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                confirm = true;
            }
            ui.add_space(8.0);
            if widgets::button(
                ui,
                &t!("lcd.cancel"),
                widgets::ButtonKind::Ghost,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );
    if let Some(filename) = resolve_delete_confirm(
        &mut st.lcd.confirm_delete_image,
        confirm,
        cancel || dismissed,
    ) {
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::DeleteLcdImage {
                filename: filename.clone(),
            },
        );
        st.lcd.image_cache.remove(&filename);
        st.lcd.requested.remove(&filename);
        // The daemon doesn't push the library after a delete; re-list so
        // the removed tile disappears.
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::ListLcdImages,
        );
    }
}

/// Handle a click on the "None" tile: reset the screen to default, first
/// deactivating any running template.
pub(super) fn select_none(
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    active_template_id: Option<&str>,
) {
    st.lcd.preview_pending = None;
    if active_template_id.is_some() {
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::LcdEngineDeactivate {
                device_id: id.to_string(),
            },
        );
        st.lcd.template_params.clear();
    }
    crate::runtime::ipc::send(
        ctx.cmd,
        halod_shared::commands::DaemonCommand::SetScreenDefault { id: id.to_string() },
    );
}

/// Handle a click on an image tile: apply it directly to the device, first
/// deactivating any running template.
pub(super) fn select_image(
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    filename: &str,
    active_template_id: Option<&str>,
) {
    // Reject anything the daemon would reject anyway (path traversal, bad
    // extension) before it ever leaves the GUI — the daemon re-checks on receipt.
    if let Err(e) = halod_shared::types::validate_image_filename(filename) {
        log::warn!("[LCD] refusing to apply invalid image filename {filename:?}: {e}");
        return;
    }
    if active_template_id.is_some() {
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::LcdEngineDeactivate {
                device_id: id.to_string(),
            },
        );
        st.lcd.template_params.clear();
    }
    st.lcd.preview_pending = Some(filename.to_string());
    crate::runtime::ipc::send(
        ctx.cmd,
        halod_shared::commands::DaemonCommand::SetScreenImageFromLibrary {
            id: id.to_string(),
            filename: filename.to_string(),
            request_id: None,
        },
    );
}

/// Label for the in-flight upload spinner. Falls back to a generic
/// "Uploading" until the daemon reports which stage it is in.
fn upload_label(progress: Option<&LcdUploadProgress>, device_id: &str) -> String {
    match progress {
        Some(p) if p.device_id == device_id => match (p.stage, p.percent) {
            (LcdUploadStage::Processing, Some(pct)) => {
                t!("lcd.upload_processing_pct", pct = pct).to_string()
            }
            (LcdUploadStage::Processing, None) => t!("lcd.upload_processing").to_string(),
            (LcdUploadStage::Applying, _) => t!("lcd.upload_writing").to_string(),
            // Terminal stages clear the spinner the same frame; the label is a
            // transient fallback only.
            (LcdUploadStage::Done | LcdUploadStage::Failed, _) => t!("lcd.uploading").to_string(),
        },
        _ => t!("lcd.uploading").to_string(),
    }
}

/// Open the native file picker on a background thread. Blocking the UI thread
/// on the dialog stops the event loop, and GNOME's liveness ping then reports
/// the app as not responding. The result lands in `st.lcd.picker`, applied by
/// `poll_picker` on a later frame. No-op while a picker is already open.
fn spawn_picker(
    ui: &egui::Ui,
    st: &mut DeviceUi,
    target: PickerTarget,
    filter_name: &str,
    extensions: &[&str],
) {
    if st.lcd.picker.is_some() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    st.lcd.picker = Some((target, rx));
    let filter_name = filter_name.to_string();
    let extensions: Vec<String> = extensions.iter().map(|e| e.to_string()).collect();
    let egui_ctx = ui.ctx().clone();
    std::thread::spawn(move || {
        let picked = rfd::FileDialog::new()
            .add_filter(filter_name, &extensions)
            .pick_file();
        let _ = tx.send(picked);
        egui_ctx.request_repaint();
    });
}

/// Apply a finished file pick: upload the image or switch the video source.
/// A cancelled dialog (or a dead picker thread) just clears the slot.
pub(super) fn poll_picker(ui: &egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    let Some((target, rx)) = st.lcd.picker.take() else {
        return;
    };
    match rx.try_recv() {
        Err(TryRecvError::Empty) => st.lcd.picker = Some((target, rx)),
        Ok(Some(path)) => match target {
            PickerTarget::Image => start_upload(ui, ctx, st, id, path),
            PickerTarget::Video => crate::runtime::ipc::send(
                ctx.cmd,
                halod_shared::commands::DaemonCommand::SetScreenVideo {
                    id: id.to_string(),
                    path: path.to_string_lossy().into_owned(),
                },
            ),
        },
        Ok(None) | Err(TryRecvError::Disconnected) => {}
    }
}

/// Read + base64-encode the picked file off the UI thread (a multi-MB GIF would
/// otherwise freeze the page for the whole encode) and send it to the daemon.
fn start_upload(ui: &egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str, path: PathBuf) {
    // Feedback only: the daemon re-enforces the same ceiling on receipt.
    match std::fs::metadata(&path) {
        Ok(m) => {
            if let Err(e) = halod_shared::types::validate_image_upload_size(m.len()) {
                log::warn!("image file {path:?} rejected: {e}");
                return;
            }
        }
        Err(e) => {
            log::warn!("image file {path:?} not readable: {e}");
            return;
        }
    }

    // The spinner clears when the refreshed library grows (see `upload_base`).
    st.lcd.upload_base = Some(ctx.lcd_images.len());
    let cmd = ctx.cmd.clone();
    let egui_ctx = ui.ctx().clone();
    let device_id = id.to_string();
    std::thread::spawn(move || {
        match std::fs::read(&path) {
            Ok(bytes) => {
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                crate::runtime::ipc::send(
                    &cmd,
                    halod_shared::commands::DaemonCommand::SetScreenImage {
                        id: device_id,
                        data_b64: b64,
                        request_id: None,
                    },
                );
            }
            Err(e) => log::warn!("upload: reading {path:?} failed: {e}"),
        }
        egui_ctx.request_repaint();
    });
}

/// Draw the "None" placeholder tile. Returns true if clicked.
pub(super) fn draw_none_tile(ui: &mut egui::Ui, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(THUMB), Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 8.0, theme::INNER_BG);
    let border = if selected { theme::CYAN } else { theme::BORDER };
    p.rect_stroke(
        rect,
        8.0,
        Stroke::new(if selected { 2.0 } else { 1.0 }, border),
        egui::StrokeKind::Middle,
    );
    let c = rect.center();
    let d = 9.0_f32;
    let ink = if selected {
        theme::CYAN
    } else {
        theme::TEXT_FAINT
    };
    p.line_segment(
        [c + egui::vec2(-d, -d), c + egui::vec2(d, d)],
        Stroke::new(1.5, ink),
    );
    p.line_segment(
        [c + egui::vec2(-d, d), c + egui::vec2(d, -d)],
        Stroke::new(1.5, ink),
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// Outcome of interacting with a thumbnail tile.
#[derive(Default)]
pub(super) struct TileAction {
    /// The tile body was clicked (select this image).
    pub(super) clicked: bool,
    /// The hover delete badge was clicked (remove this image).
    pub(super) delete: bool,
}

/// Draw a thumbnail tile (image or grey placeholder) with a hover delete badge.
/// `id_salt` disambiguates the badge's interaction id across tiles.
pub(super) fn draw_thumb_tile(
    ui: &mut egui::Ui,
    tex: Option<&egui::TextureHandle>,
    selected: bool,
    id_salt: &str,
) -> TileAction {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(THUMB), Sense::click());

    // Delete badge in the top-right corner, hit-tested before painting.
    let badge_r = 8.0;
    let badge_c = rect.right_top() + egui::vec2(-badge_r - 2.0, badge_r + 2.0);
    let badge_rect = Rect::from_center_size(badge_c, Vec2::splat(badge_r * 2.0));
    let badge = ui.interact(badge_rect, ui.id().with(("del", id_salt)), Sense::click());
    let show_badge = resp.hovered() || badge.hovered();

    let p = ui.painter();
    if let Some(tex) = tex {
        p.image(
            tex.id(),
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        p.rect_filled(rect, theme::RADIUS_XS, theme::INNER_BG);
    }
    let ring = if selected {
        theme::CYAN
    } else if resp.hovered() {
        theme::TEXT_MUT
    } else {
        Color32::TRANSPARENT
    };
    p.rect_stroke(
        rect,
        theme::RADIUS_XS,
        Stroke::new(2.0, ring),
        egui::StrokeKind::Middle,
    );

    if show_badge {
        p.circle_filled(badge_c, badge_r, theme::OFFLINE);
        let d = 3.0;
        p.line_segment(
            [badge_c + egui::vec2(-d, -d), badge_c + egui::vec2(d, d)],
            Stroke::new(1.4, Color32::WHITE),
        );
        p.line_segment(
            [badge_c + egui::vec2(-d, d), badge_c + egui::vec2(d, -d)],
            Stroke::new(1.4, Color32::WHITE),
        );
    }
    if show_badge {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    TileAction {
        // A badge click also lands on the tile; don't let it select.
        clicked: resp.clicked() && !badge.clicked(),
        delete: badge.clicked(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(stage: LcdUploadStage, device_id: &str) -> LcdUploadProgress {
        LcdUploadProgress {
            device_id: device_id.into(),
            stage,
            percent: None,
        }
    }

    #[test]
    fn upload_spinner_clears_only_once_the_library_grows() {
        // No spinner armed → stays cleared regardless of library size.
        assert_eq!(cleared_upload_base(None, 5, None, "lcd"), None);
        // Armed at 3: still uploading while the library hasn't grown past 3.
        assert_eq!(cleared_upload_base(Some(3), 3, None, "lcd"), Some(3));
        assert_eq!(cleared_upload_base(Some(3), 2, None, "lcd"), Some(3));
        // The refreshed library grew past the captured size → spinner clears.
        assert_eq!(cleared_upload_base(Some(3), 4, None, "lcd"), None);
    }

    #[test]
    fn upload_spinner_clears_on_terminal_signal() {
        // A fresh Done for this device clears even without library growth.
        let done = terminal(LcdUploadStage::Done, "lcd");
        assert_eq!(cleared_upload_base(Some(3), 3, Some(&done), "lcd"), None);
        // A Failed likewise clears (the reported bug: a failed device write).
        let failed = terminal(LcdUploadStage::Failed, "lcd");
        assert_eq!(cleared_upload_base(Some(3), 3, Some(&failed), "lcd"), None);
        // A terminal for a different device is ignored.
        let other = terminal(LcdUploadStage::Failed, "other");
        assert_eq!(
            cleared_upload_base(Some(3), 3, Some(&other), "lcd"),
            Some(3)
        );
        // Staleness: a retained terminal is delivered as a `None` one-shot on
        // non-arrival frames, so a freshly-armed spinner is not cleared.
        assert_eq!(cleared_upload_base(Some(3), 3, None, "lcd"), Some(3));
    }

    #[test]
    fn upload_label_reflects_daemon_progress_for_the_open_device() {
        let p = |stage, percent| {
            Some(LcdUploadProgress {
                device_id: "lcd".into(),
                stage,
                percent,
            })
        };
        assert_eq!(upload_label(None, "lcd"), "Uploading…");
        assert_eq!(
            upload_label(p(LcdUploadStage::Processing, Some(42)).as_ref(), "lcd"),
            "Processing… 42%"
        );
        assert_eq!(
            upload_label(p(LcdUploadStage::Processing, None).as_ref(), "lcd"),
            "Processing…"
        );
        assert_eq!(
            upload_label(p(LcdUploadStage::Applying, None).as_ref(), "lcd"),
            "Writing to device…"
        );
        // Progress for a different device must not leak into this spinner.
        assert_eq!(
            upload_label(p(LcdUploadStage::Applying, None).as_ref(), "other"),
            "Uploading…"
        );
    }
}
