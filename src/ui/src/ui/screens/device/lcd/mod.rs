// SPDX-License-Identifier: GPL-3.0-or-later
//! LCD tab — live preview plus a media picker (image/GIF library, video, engine
//! templates).
//!
//! Layout:
//!   left  — Display card: shape-clipped preview, brightness, rotation, reset.
//!   right — Media card: Images/GIFs | Video | Template, selected by a pill row.
//!
//! Preview pipeline (driven by the daemon's reported `LcdMode` each frame):
//!   • Gif            — animate the active file locally. Frames stream in off the
//!                      UI thread so frame 0 paints the instant it decodes; the
//!                      rest accumulate in the background (fast GIF↔GIF switches).
//!   • Engine / Video — the daemon renders and pushes pre-decoded RGBA preview
//!                      frames on the `lcd_frames` channel; we just re-texture.
//!   • Image / Default— a static thumbnail decoded from the library file.
//!
//! Image/GIF bytes are read straight from the daemon's on-disk library
//! (`{config_dir}/lcd_images`). The daemon reports its own config dir and runs
//! as the same user on the same host (Unix socket / named pipe), so the GUI
//! opens the file directly instead of pulling it over IPC — no base64, no
//! frame-size ceiling.

pub(super) mod editor;
mod gif;
mod library;
mod params;
mod preview;

use halod_shared::types::{DeviceCapability, LcdMode, LcdStatus, ScreenRotation};

use gif::{advance_gif, clear_gif, rgba_texture};
use library::{delete_image_modal, media_content, poll_picker, select_image, select_none};
use preview::display_card;

use super::{DeviceUi, LcdMediaTab, PickerTarget, TabCtx};
use crate::ui::components as widgets;

/// Cache key for the preview texture. Each mode/image combination maps to a
/// distinct string so any transition (GIF → none, image → other image) is seen
/// as a change and never leaves a stale frame on screen.
fn preview_key(mode: &LcdMode, active_image: Option<&str>) -> String {
    match (mode, active_image) {
        (LcdMode::Gif, img) => format!("gif:{}", img.unwrap_or_default()),
        (_, Some(img)) => format!("img:{img}"),
        (_, None) => String::new(),
    }
}

fn rot_label(r: ScreenRotation) -> &'static str {
    match r {
        ScreenRotation::R0 => "0°",
        ScreenRotation::R90 => "90°",
        ScreenRotation::R180 => "180°",
        ScreenRotation::R270 => "270°",
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Engine preview is needed for Image/Video tabs but not the Template editor.
fn wants_engine_preview(tab: LcdMediaTab) -> bool {
    tab != LcdMediaTab::Template
}

/// Returns true (and updates the timestamp) when a keepalive is due.
fn preview_keepalive_due(at: &mut f64, now: f64) -> bool {
    if *at == 0.0 || now - *at >= halod_shared::types::LCD_PREVIEW_KEEPALIVE_SECS {
        *at = now;
        true
    } else {
        false
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    let Some(lcd) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Lcd(l) => Some(l),
        _ => None,
    }) else {
        return;
    };
    let id = ctx.dev.id.clone();

    // Seed the media tab from the daemon's mode, once per profile: a switch
    // re-applies that profile's LCD state, so the tab (and the editor's def)
    // must follow it instead of keeping the previous profile's selection.
    if st.lcd.seeded_profile.as_deref() != Some(ctx.state.profiles.active.as_str()) {
        st.lcd.media_tab = match lcd.mode {
            LcdMode::Video => LcdMediaTab::Video,
            LcdMode::Engine => LcdMediaTab::Template,
            _ => LcdMediaTab::Images,
        };
        st.lcd.editor.seeded = false;
        st.lcd.seeded_profile = Some(ctx.state.profiles.active.clone());
        // The profile already set the daemon to this mode; suppress re-activation.
        st.lcd.prev_mode_tab = Some(st.lcd.media_tab);
    }

    // Fetch the library once on first render of this device page.
    if !st.lcd.list_requested {
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::ListLcdImages,
        );
        st.lcd.list_requested = true;
    }

    // Renew the lease-gated preview keepalive while the Display card is visible.
    if wants_engine_preview(st.lcd.media_tab)
        && preview_keepalive_due(&mut st.lcd.preview_keepalive_at, ctx.time)
    {
        crate::runtime::ipc::send(
            ctx.cmd,
            halod_shared::commands::DaemonCommand::LcdEngineSubscribe,
        );
    }

    poll_picker(ui, ctx, st, &id);

    // Mode tabs sit in a full-width header above the mode-specific layout —
    // the Editor mode below has no room for (and no use of) a persistent
    // Display card, so it can't live nested inside a shared two-column split.
    mode_header(ui, ctx, st);

    // Navigating to a tab alone (no widget move, no image/video pick) still
    // flips the daemon's LcdMode to match.
    if Some(st.lcd.media_tab) != st.lcd.prev_mode_tab {
        match st.lcd.media_tab {
            LcdMediaTab::Images => activate_image_mode(ctx, st, &id, lcd),
            LcdMediaTab::Video => activate_video_mode(ctx, &id, lcd),
            LcdMediaTab::Template => {}
        }
    }

    ui.add_space(14.0);

    match st.lcd.media_tab {
        LcdMediaTab::Template => editor::show(ui, ctx, st, &id, lcd),
        _ => {
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::TabLcd,
                ui.max_rect(),
            );
            ui.columns(2, |cols| {
                display_card(&mut cols[0], ctx, st, &id, lcd);
                media_content(&mut cols[1], ctx, st, &id, lcd);
            });
        }
    }
    st.lcd.prev_mode_tab = Some(st.lcd.media_tab);

    // Modals overlay the content — must be drawn last.
    delete_image_modal(ui, ctx, st);
}

fn activate_image_mode(ctx: &TabCtx, st: &mut DeviceUi, id: &str, lcd: &LcdStatus) {
    let active_template_id = ctx.state.lcd.engine.device_templates.get(id).cloned();
    match lcd.active_image.clone() {
        Some(filename) => select_image(ctx, st, id, &filename, active_template_id.as_deref()),
        None => select_none(ctx, st, id, active_template_id.as_deref()),
    }
}

fn activate_video_mode(ctx: &TabCtx, id: &str, lcd: &LcdStatus) {
    let Some(path) = lcd.video_path.clone() else {
        return;
    };
    crate::runtime::ipc::send(
        ctx.cmd,
        halod_shared::commands::DaemonCommand::SetScreenVideo {
            id: id.to_string(),
            path,
        },
    );
}

fn mode_header(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    let hdr_rect =
        egui::Rect::from_min_size(ui.cursor().min, egui::Vec2::new(ui.available_width(), 40.0));
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::LcdModeTabs,
        hdr_rect,
    );
    widgets::card_with_margin(ui, egui::Margin::symmetric(8, 6), |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            if widgets::pill(
                ui,
                &t!("lcd.tab_image"),
                st.lcd.media_tab == LcdMediaTab::Images,
            ) {
                st.lcd.media_tab = LcdMediaTab::Images;
            }
            if widgets::pill(
                ui,
                &t!("lcd.tab_editor"),
                st.lcd.media_tab == LcdMediaTab::Template,
            ) {
                st.lcd.media_tab = LcdMediaTab::Template;
            }
            let video_active = st.lcd.media_tab == LcdMediaTab::Video;
            if ctx.state.health.ffmpeg_available {
                if widgets::pill(ui, &t!("lcd.tab_video"), video_active) {
                    st.lcd.media_tab = LcdMediaTab::Video;
                }
            } else {
                ui.add_enabled_ui(false, |ui| {
                    let _ = widgets::pill(ui, &t!("lcd.tab_video"), video_active);
                })
                .response
                .on_disabled_hover_text(t!("lcd.ffmpeg_not_installed").to_string());
            }
        });
    });
}

/// Refresh `preview_tex` from whichever source the current mode implies.
fn drive_preview(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, lcd: &LcdStatus) {
    match lcd.mode {
        LcdMode::Gif => {
            match &lcd.active_image {
                Some(filename) => advance_gif(ui, ctx, st, filename, ctx.time),
                None => {
                    clear_gif(st);
                    st.lcd.preview_tex = None;
                }
            }
            // Distinct from the Image/Default no-image key ("") so a switch to no
            // image is seen as a change and clears the stale frame.
            st.lcd.preview_key = preview_key(&LcdMode::Gif, lcd.active_image.as_deref());
        }
        LcdMode::Engine | LcdMode::Video | LcdMode::EditorPreview => {
            clear_gif(st);
            // The daemon pre-decodes engine/video preview frames to RGBA; rebuild
            // the texture only when the frame id changes.
            if let Some(f) = ctx.lcd_preview.as_ref() {
                let key = format!("frame:{}", f.frame_id);
                if st.lcd.preview_key != key {
                    st.lcd.preview_key = key;
                    st.lcd.preview_tex = Some(rgba_texture(
                        ui.ctx(),
                        "lcd_engine_frame",
                        &f.rgba,
                        f.width,
                        f.height,
                    ));
                }
            }
        }
        LcdMode::Image | LcdMode::Default => {
            clear_gif(st);
            let key = preview_key(&lcd.mode, lcd.active_image.as_deref());
            // Rebuild on a target change, or while we still have no texture (the
            // config dir may not have arrived on the first frame).
            if st.lcd.preview_key != key || st.lcd.preview_tex.is_none() {
                st.lcd.preview_key = key.clone();
                st.lcd.preview_tex = key
                    .strip_prefix("img:")
                    .and_then(|f| gif::load_tex_from_file(ctx, ui.ctx(), f));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{preview_keepalive_due, preview_key, rot_label, show, wants_engine_preview};
    use crate::runtime::ipc::DecodedFrame;
    use crate::ui::screens::device::{DeviceUi, LcdMediaTab, PickerTarget, TabCtx};
    use halod_shared::commands::DaemonCommand;
    use halod_shared::types::LcdUploadProgress;
    use halod_shared::types::{
        AppState, DeviceCapability, LcdDescriptor, LcdEngineTemplateDescriptor, LcdMode, LcdStatus,
        ScreenRotation, ScreenShape, WireDevice,
    };
    use std::time::{Duration, Instant};

    /// Encode a tiny `n`-frame GIF in memory for the decode tests.
    fn make_gif(n: usize) -> Vec<u8> {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, Rgba, RgbaImage};
        let mut buf = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut buf);
            for i in 0..n {
                let img = RgbaImage::from_pixel(2, 2, Rgba([(i * 20) as u8, 0, 0, 255]));
                enc.encode_frame(Frame::from_parts(
                    img,
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
            }
        }
        buf
    }

    /// Encode a tiny PNG in memory for the static-image tests.
    fn make_png() -> Vec<u8> {
        use image::{Rgba, RgbaImage};
        let img = RgbaImage::from_pixel(2, 2, Rgba([10, 20, 30, 255]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn descriptor() -> LcdDescriptor {
        LcdDescriptor {
            shape: ScreenShape::Square,
            width: 240,
            height: 240,
            supported_rotations: vec![],
            supported_image_types: vec![],
            latches_last_frame: false,
        }
    }

    fn lcd_status(mode: LcdMode, active_image: Option<&str>) -> LcdStatus {
        LcdStatus {
            descriptor: descriptor(),
            brightness: 50,
            rotation: ScreenRotation::R0,
            mode,
            active_image: active_image.map(str::to_string),
            video_path: None,
            raw_streaming: false,
            health: Default::default(),
        }
    }

    fn template(id: &str, name: &str) -> LcdEngineTemplateDescriptor {
        LcdEngineTemplateDescriptor {
            id: id.into(),
            name: name.into(),
            params: vec![],
        }
    }

    /// Drives the real `show` entry point through actual egui frames, so tests
    /// exercise the same code path the app does. Holds the daemon-side inputs
    /// (device state, library, engine preview) and captures the commands the
    /// widget dispatches. Image bytes live on disk under `_tmp/lcd_images`, the
    /// same way the GUI reads them from the daemon's library.
    struct Fixture {
        ctx: egui::Context,
        state: AppState,
        dev: WireDevice,
        images: Vec<String>,
        preview: Option<DecodedFrame>,
        upload: Option<LcdUploadProgress>,
        time: f64,
        tx: crate::runtime::ipc::CommandTx,
        rx: tokio::sync::mpsc::UnboundedReceiver<DaemonCommand>,
        _tmp: tempfile::TempDir,
    }

    impl Fixture {
        fn new(mode: LcdMode, active_image: Option<&str>) -> Self {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let dev = WireDevice {
                id: "lcd".into(),
                connected: true,
                capabilities: vec![DeviceCapability::Lcd(lcd_status(mode, active_image))],
                ..Default::default()
            };
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(tmp.path().join(halod_shared::types::LCD_IMAGES_SUBDIR))
                .unwrap();
            let state = AppState {
                health: halod_shared::types::HealthCheckState {
                    ffmpeg_available: true,
                    ..Default::default()
                },
                devices: vec![dev.clone()],
                config_dir: tmp.path().to_string_lossy().into_owned(),
                ..Default::default()
            };
            // The widgets reference custom font families ("semibold", "mono", …);
            // register them or text layout panics.
            let ctx = egui::Context::default();
            crate::ui::theme::install_fonts(&ctx);
            Fixture {
                ctx,
                state,
                dev,
                images: Vec::new(),
                preview: None,
                upload: None,
                time: 0.0,
                tx,
                rx,
                _tmp: tmp,
            }
        }

        /// Write an image file into the on-disk library the widget reads from.
        fn put_image(&self, filename: &str, bytes: &[u8]) {
            let dir = std::path::Path::new(&self.state.config_dir)
                .join(halod_shared::types::LCD_IMAGES_SUBDIR);
            std::fs::write(dir.join(filename), bytes).unwrap();
        }

        /// Reflect a daemon broadcast: change the reported LCD mode/active image.
        fn set_lcd(&mut self, mode: LcdMode, active_image: Option<&str>) {
            self.dev.capabilities = vec![DeviceCapability::Lcd(lcd_status(mode, active_image))];
            self.state.devices = vec![self.dev.clone()];
        }

        /// Run one real egui frame, returning the commands the widget dispatched.
        fn frame(&mut self, st: &mut DeviceUi) -> Vec<DaemonCommand> {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1000.0, 800.0),
                )),
                ..Default::default()
            };
            let Fixture {
                ctx,
                state,
                dev,
                images,
                preview,
                upload,
                time,
                tx,
                rx,
                ..
            } = self;
            let _ = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let tab = TabCtx {
                        state,
                        dev,
                        cmd: tx,
                        time: *time,
                        debug: None,
                        lcd_images: images.as_slice(),
                        lcd_preview: preview.clone(),
                        lcd_upload: upload.clone(),
                        lcd_upload_terminal: None,
                        lcd_template: None,
                        lcd_editor_render: None,
                        led_colors: crate::ui::screens::device::empty_led_colors(),
                        write_rate_history: None,
                    };
                    show(ui, &tab, st);
                });
            });
            let mut cmds = Vec::new();
            while let Ok(c) = rx.try_recv() {
                cmds.push(c);
            }
            cmds
        }

        /// Pump frames (yielding briefly for the background GIF decoder) until
        /// `done` holds, or panic on timeout.
        fn pump_until(&mut self, st: &mut DeviceUi, mut done: impl FnMut(&DeviceUi) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.frame(st);
                if done(st) {
                    return;
                }
                assert!(Instant::now() < deadline, "condition not met within 5s");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    // ── Behaviour: preview pipeline across modes and tabs ──────────────────

    #[test]
    fn first_frame_requests_the_image_library() {
        let mut fx = Fixture::new(LcdMode::Default, None);
        let mut st = DeviceUi::new("lcd".into());
        let cmds = fx.frame(&mut st);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, DaemonCommand::ListLcdImages)),
            "the library must be listed on first render: {cmds:?}"
        );
    }

    // ── Behaviour: file picker runs off the UI thread ──────────────────────

    #[test]
    fn picked_video_sends_set_screen_video() {
        let mut fx = Fixture::new(LcdMode::Default, None);
        let mut st = DeviceUi::new("lcd".into());
        let (tx, rx) = std::sync::mpsc::channel();
        st.lcd.picker = Some((PickerTarget::Video, rx));
        tx.send(Some(std::path::PathBuf::from("/tmp/movie.mp4")))
            .unwrap();

        let cmds = fx.frame(&mut st);
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                DaemonCommand::SetScreenVideo { path, .. } if path == "/tmp/movie.mp4"
            )),
            "the picked path must be sent to the daemon: {cmds:?}"
        );
        assert!(st.lcd.picker.is_none(), "the picker slot is cleared");
    }

    #[test]
    fn picked_image_is_uploaded_in_the_background() {
        let mut fx = Fixture::new(LcdMode::Default, None);
        let mut st = DeviceUi::new("lcd".into());
        let path = std::path::Path::new(&fx.state.config_dir).join("up.png");
        std::fs::write(&path, make_png()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        st.lcd.picker = Some((PickerTarget::Image, rx));
        tx.send(Some(path)).unwrap();

        let mut cmds = fx.frame(&mut st);
        assert!(st.lcd.picker.is_none(), "the picker slot is cleared");
        assert!(st.lcd.upload_base.is_some(), "the upload spinner is armed");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cmds
            .iter()
            .any(|c| matches!(c, DaemonCommand::SetScreenImage { .. }))
        {
            assert!(Instant::now() < deadline, "upload never sent: {cmds:?}");
            std::thread::sleep(Duration::from_millis(2));
            cmds.extend(fx.frame(&mut st));
        }
    }

    #[test]
    fn cancelled_pick_clears_without_side_effects() {
        let mut fx = Fixture::new(LcdMode::Default, None);
        let mut st = DeviceUi::new("lcd".into());
        let (tx, rx) = std::sync::mpsc::channel();
        st.lcd.picker = Some((PickerTarget::Image, rx));
        tx.send(None).unwrap();

        let cmds = fx.frame(&mut st);
        assert!(st.lcd.picker.is_none());
        assert!(st.lcd.upload_base.is_none(), "no upload spinner");
        assert!(
            !cmds.iter().any(|c| matches!(
                c,
                DaemonCommand::SetScreenImage { .. } | DaemonCommand::SetScreenVideo { .. }
            )),
            "no command from a cancelled dialog: {cmds:?}"
        );
    }

    #[test]
    fn pending_pick_stays_armed_until_the_dialog_resolves() {
        let mut fx = Fixture::new(LcdMode::Default, None);
        let mut st = DeviceUi::new("lcd".into());
        let (_tx, rx) = std::sync::mpsc::channel::<Option<std::path::PathBuf>>();
        st.lcd.picker = Some((PickerTarget::Video, rx));

        fx.frame(&mut st);
        assert!(
            st.lcd.picker.is_some(),
            "an open dialog must not be dropped by the poll"
        );
    }

    #[test]
    fn loading_page_with_gif_selected_decodes_and_previews() {
        // Daemon already reports Gif mode when the page opens (persisted state).
        let mut fx = Fixture::new(LcdMode::Gif, Some("a.gif"));
        fx.images = vec!["a.gif".into()];
        fx.put_image("a.gif", &make_gif(3));
        let mut st = DeviceUi::new("lcd".into());

        fx.pump_until(&mut st, |st| {
            st.lcd.gif_rx.is_none() && !st.lcd.gif_frames.is_empty()
        });
        assert_eq!(st.lcd.gif_source, "a.gif");
        assert_eq!(st.lcd.gif_frames.len(), 3, "all frames stream in");
        assert!(
            st.lcd.preview_tex.is_some(),
            "a frame is textured for preview"
        );
    }

    #[test]
    fn switching_between_gifs_restarts_the_decode() {
        let mut fx = Fixture::new(LcdMode::Gif, Some("a.gif"));
        fx.images = vec!["a.gif".into(), "b.gif".into()];
        fx.put_image("a.gif", &make_gif(3));
        fx.put_image("b.gif", &make_gif(5));
        let mut st = DeviceUi::new("lcd".into());

        fx.pump_until(&mut st, |st| {
            st.lcd.gif_source == "a.gif" && st.lcd.gif_rx.is_none()
        });
        assert_eq!(st.lcd.gif_frames.len(), 3);

        // Daemon confirms the switch to the second GIF.
        fx.set_lcd(LcdMode::Gif, Some("b.gif"));
        fx.pump_until(&mut st, |st| {
            st.lcd.gif_source == "b.gif" && st.lcd.gif_rx.is_none() && st.lcd.gif_frames.len() == 5
        });
        // Re-decoded from scratch — not the previous GIF's 3 frames.
        assert_eq!(st.lcd.gif_frames.len(), 5);
        assert!(st.lcd.preview_tex.is_some());
    }

    #[test]
    fn gif_animation_advances_frames_over_time() {
        let mut fx = Fixture::new(LcdMode::Gif, Some("a.gif"));
        fx.put_image("a.gif", &make_gif(3)); // 3 frames @ 100 ms
        let mut st = DeviceUi::new("lcd".into());
        fx.pump_until(&mut st, |st| {
            st.lcd.gif_rx.is_none() && st.lcd.gif_frames.len() == 3
        });
        assert_eq!(st.lcd.gif_idx, 0, "starts on the first frame");

        fx.time = 0.15; // past the 100 ms delay of frame 0
        fx.frame(&mut st);
        assert_eq!(st.lcd.gif_idx, 1);
        fx.time = 0.30;
        fx.frame(&mut st);
        assert_eq!(st.lcd.gif_idx, 2);
        fx.time = 0.45; // wraps back to the start
        fx.frame(&mut st);
        assert_eq!(st.lcd.gif_idx, 0);
    }

    #[test]
    fn png_preview_builds_a_texture_from_the_on_disk_file() {
        let mut fx = Fixture::new(LcdMode::Image, Some("p.png"));
        fx.images = vec!["p.png".into()];
        let mut st = DeviceUi::new("lcd".into());

        // File not on disk yet → no preview (the read fails, no panic).
        fx.frame(&mut st);
        assert!(st.lcd.preview_tex.is_none());

        // File appears in the library → the preview texture is built from disk
        // even though the preview key was already set on the earlier frame.
        fx.put_image("p.png", &make_png());
        fx.frame(&mut st);
        assert!(
            st.lcd.preview_tex.is_some(),
            "static PNG preview must appear"
        );
        assert_eq!(st.lcd.preview_key, "img:p.png");
    }

    #[test]
    fn video_and_engine_modes_preview_from_pushed_frames() {
        // A GIF is running first so we can prove its state is torn down.
        let mut fx = Fixture::new(LcdMode::Gif, Some("a.gif"));
        fx.put_image("a.gif", &make_gif(3));
        let mut st = DeviceUi::new("lcd".into());
        fx.pump_until(&mut st, |st| !st.lcd.gif_frames.is_empty());

        // Daemon switches to Video and pushes a decoded preview frame.
        fx.set_lcd(LcdMode::Video, None);
        fx.preview = Some(DecodedFrame {
            frame_id: 7,
            width: 2,
            height: 2,
            rgba: vec![255u8; 2 * 2 * 4],
        });
        fx.frame(&mut st);
        assert!(st.lcd.gif_source.is_empty(), "GIF state is cleared");
        assert!(st.lcd.gif_frames.is_empty());
        assert!(st.lcd.gif_rx.is_none());
        assert_eq!(st.lcd.preview_key, "frame:7");
        assert!(st.lcd.preview_tex.is_some());
    }

    #[test]
    fn switching_media_tabs_does_not_disturb_the_gif_preview() {
        // Mode stays Gif throughout; only the right-hand media tab changes.
        let mut fx = Fixture::new(LcdMode::Gif, Some("a.gif"));
        fx.images = vec!["a.gif".into()];
        fx.put_image("a.gif", &make_gif(3));
        let mut st = DeviceUi::new("lcd".into());
        fx.pump_until(&mut st, |st| {
            st.lcd.gif_rx.is_none() && st.lcd.gif_frames.len() == 3
        });

        // Switch to the Video tab, then to Template, then back to Images.
        for tab in [
            LcdMediaTab::Video,
            LcdMediaTab::Template,
            LcdMediaTab::Images,
        ] {
            st.lcd.media_tab = tab;
            fx.frame(&mut st);
        }

        // The display-card preview kept animating regardless of the media tab.
        assert_eq!(st.lcd.gif_source, "a.gif");
        assert_eq!(st.lcd.gif_frames.len(), 3);
        assert!(st.lcd.preview_tex.is_some());
    }

    #[test]
    fn profile_switch_reseeds_media_tab_and_editor() {
        // Opened under a profile driving the LCD via the engine → Editor tab.
        let mut fx = Fixture::new(LcdMode::Engine, None);
        fx.state.profiles.active = "A".into();
        let mut st = DeviceUi::new("lcd".into());
        fx.frame(&mut st);
        assert!(st.lcd.media_tab == LcdMediaTab::Template);
        st.lcd.editor.seeded = true;

        // Same profile: a manually chosen tab survives later frames.
        st.lcd.media_tab = LcdMediaTab::Images;
        fx.frame(&mut st);
        assert!(st.lcd.media_tab == LcdMediaTab::Images);
        st.lcd.media_tab = LcdMediaTab::Template;

        // The daemon switches to a profile with a static image selected.
        fx.state.profiles.active = "B".into();
        fx.images = vec!["a.png".into()];
        fx.put_image("a.png", &make_png());
        fx.set_lcd(LcdMode::Image, Some("a.png"));
        fx.frame(&mut st);
        assert!(
            st.lcd.media_tab == LcdMediaTab::Images,
            "tab must follow the new profile's LCD mode"
        );
        assert!(
            !st.lcd.editor.seeded,
            "editor must re-seed from the new profile's params"
        );
    }

    #[test]
    fn template_tab_renders_and_engine_preview_takes_over() {
        let mut fx = Fixture::new(LcdMode::Default, None);
        fx.state.lcd.engine.available_templates = vec![template("clock", "Clock")];
        let mut st = DeviceUi::new("lcd".into());
        st.lcd.media_tab = LcdMediaTab::Template;
        // Renders the template tab (pills + background picker) without panicking.
        fx.frame(&mut st);

        // Daemon activates the template: mode becomes Engine and frames are pushed.
        fx.set_lcd(LcdMode::Engine, None);
        fx.state
            .lcd
            .engine
            .device_templates
            .insert("lcd".into(), "clock".into());
        fx.preview = Some(DecodedFrame {
            frame_id: 42,
            width: 2,
            height: 2,
            rgba: vec![128u8; 2 * 2 * 4],
        });
        fx.frame(&mut st);
        assert_eq!(st.lcd.preview_key, "frame:42");
        assert!(st.lcd.preview_tex.is_some(), "engine preview is shown");
    }

    #[test]
    fn select_image_without_active_template_sets_it_directly() {
        let mut fx = Fixture::new(LcdMode::Default, None);
        fx.images = vec!["bg.png".into()];
        let mut st = DeviceUi::new("lcd".into());
        let ctx = TabCtx {
            state: &fx.state,
            dev: &fx.dev,
            cmd: &fx.tx,
            time: fx.time,
            debug: None,
            lcd_images: fx.images.as_slice(),
            lcd_preview: None,
            lcd_upload: None,
            lcd_upload_terminal: None,
            lcd_template: None,
            lcd_editor_render: None,
            led_colors: crate::ui::screens::device::empty_led_colors(),
            write_rate_history: None,
        };
        super::select_image(&ctx, &mut st, "lcd", "bg.png", None);
        assert_eq!(st.lcd.preview_pending.as_deref(), Some("bg.png"));
        let cmd = fx.rx.try_recv().expect("command dispatched");
        assert!(matches!(
            cmd,
            DaemonCommand::SetScreenImageFromLibrary { .. }
        ));
    }

    #[test]
    fn select_image_deactivates_a_running_template_first() {
        let mut fx = Fixture::new(LcdMode::Engine, None);
        fx.images = vec!["bg.png".into()];
        let mut st = DeviceUi::new("lcd".into());
        let ctx = TabCtx {
            state: &fx.state,
            dev: &fx.dev,
            cmd: &fx.tx,
            time: fx.time,
            debug: None,
            lcd_images: fx.images.as_slice(),
            lcd_preview: None,
            lcd_upload: None,
            lcd_upload_terminal: None,
            lcd_template: None,
            lcd_editor_render: None,
            led_colors: crate::ui::screens::device::empty_led_colors(),
            write_rate_history: None,
        };
        super::select_image(&ctx, &mut st, "lcd", "bg.png", Some("custom"));
        let first = fx.rx.try_recv().expect("deactivate dispatched");
        assert!(matches!(first, DaemonCommand::LcdEngineDeactivate { .. }));
        let second = fx.rx.try_recv().expect("image command dispatched");
        assert!(matches!(
            second,
            DaemonCommand::SetScreenImageFromLibrary { .. }
        ));
    }

    #[test]
    fn rot_label_covers_all_rotations() {
        assert_eq!(rot_label(ScreenRotation::R0), "0°");
        assert_eq!(rot_label(ScreenRotation::R90), "90°");
        assert_eq!(rot_label(ScreenRotation::R180), "180°");
        assert_eq!(rot_label(ScreenRotation::R270), "270°");
    }

    #[test]
    fn preview_key_distinguishes_gif_image_and_none() {
        // GIF and Image modes never collide, and "no image" in each mode maps to
        // a distinct key so a switch is always seen as a change.
        assert_eq!(preview_key(&LcdMode::Gif, Some("a.gif")), "gif:a.gif");
        assert_eq!(preview_key(&LcdMode::Image, Some("a.png")), "img:a.png");
        assert_eq!(preview_key(&LcdMode::Gif, None), "gif:");
        assert_eq!(preview_key(&LcdMode::Image, None), "");
        assert_ne!(
            preview_key(&LcdMode::Gif, None),
            preview_key(&LcdMode::Image, None)
        );
    }

    #[test]
    fn wants_engine_preview_true_for_images_and_video_false_for_template() {
        assert!(wants_engine_preview(LcdMediaTab::Images));
        assert!(wants_engine_preview(LcdMediaTab::Video));
        assert!(!wants_engine_preview(LcdMediaTab::Template));
    }

    #[test]
    fn preview_keepalive_due_fires_first_then_throttles_then_fires_again() {
        let mut at = 0.0;
        // Never sent: fires immediately.
        assert!(preview_keepalive_due(&mut at, 10.0));
        assert_eq!(at, 10.0);
        // Within the interval: no repeat.
        assert!(!preview_keepalive_due(
            &mut at,
            10.0 + halod_shared::types::LCD_PREVIEW_KEEPALIVE_SECS - 0.01
        ));
        assert_eq!(at, 10.0, "timestamp must not move on a suppressed call");
        // Past the interval: fires again.
        assert!(preview_keepalive_due(
            &mut at,
            10.0 + halod_shared::types::LCD_PREVIEW_KEEPALIVE_SECS
        ));
    }
}
