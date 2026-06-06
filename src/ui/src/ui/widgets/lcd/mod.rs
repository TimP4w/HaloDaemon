mod image_library;
mod engine_panel;

use image_library::{PreviewSource, PendingPayload, PendingItem, draw_preview, draw_thumb};

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use halod_protocol::types::{
    LcdEngineFrame, LcdEngineTemplateDescriptor, LcdMode, LcdStatus, Sensor,
    WireLcdEngineState,
};

/// Reads the current value of one param widget as a JSON value for the IPC message.
type ParamGetter = Box<dyn Fn() -> serde_json::Value>;

#[derive(Clone)]
pub struct LcdWidget {
    pub root: gtk::Box,
    preview: gtk::DrawingArea,
    preview_source: Rc<RefCell<Option<PreviewSource>>>,
    anim_timer: Rc<RefCell<Option<gtk::glib::SourceId>>>,
    active_image_rotation: Rc<RefCell<u32>>,
    library_box: gtk::FlowBox,
    upload_btn: gtk::Button,
    spinner: gtk::Spinner,
    /// In-flight requests: req_id → action + 15 s timeout handle.
    pending: Rc<RefCell<HashMap<String, PendingItem>>>,
    device_id: String,
    store: Store,
    // ── LCD engine section ───────────────────────────────────────────────────
    engine_dropdown: gtk::DropDown,
    engine_activate_btn: gtk::Button,
    engine_deactivate_btn: gtk::Button,
    engine_status_label: gtk::Label,
    /// Box holding the selected template's parameter widgets.
    param_box: gtk::Box,
    /// Full template descriptors, in the same order as engine_dropdown items.
    engine_templates: Rc<RefCell<Vec<LcdEngineTemplateDescriptor>>>,
    /// Latest sensor list, used to populate Sensor-kind param widgets.
    sensors: Rc<RefCell<Vec<(String, Sensor)>>>,
    /// Getter closures for the live param widgets, paired with their param id.
    param_getters: Rc<RefCell<Vec<(String, ParamGetter)>>>,
    /// True while the LCD engine is active for this device — gates live param pushes.
    engine_active: Rc<RefCell<bool>>,
}

impl LcdWidget {
    pub fn build(device_id: &str, status: &LcdStatus, store: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(16)
            .build();

        let preview_source: Rc<RefCell<Option<PreviewSource>>> = Rc::new(RefCell::new(None));
        let anim_timer: Rc<RefCell<Option<gtk::glib::SourceId>>> = Rc::new(RefCell::new(None));
        let active_image_rotation: Rc<RefCell<u32>> = Rc::new(RefCell::new(status.rotation));

        // ── Top row: preview (left) + controls (right) ────────────────────
        let top_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(16)
            .build();

        // Preview card
        let preview_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            .valign(gtk::Align::Start)
            .build();

        let preview = gtk::DrawingArea::builder()
            .content_width(220)
            .content_height(220)
            .margin_start(16)
            .margin_end(16)
            .margin_top(16)
            .margin_bottom(16)
            .build();

        {
            let source_ref = Rc::clone(&preview_source);
            let shape = status.descriptor.shape.clone();
            let rot_ref = Rc::clone(&active_image_rotation);
            preview.set_draw_func(move |_, cr, w, h| {
                draw_preview(cr, w, h, &shape, &source_ref.borrow(), *rot_ref.borrow());
            });
        }

        // Overlay: preview drawing area + loading spinner centered on top.
        let spinner = gtk::Spinner::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .width_request(48)
            .height_request(48)
            .visible(false)
            .build();
        let preview_overlay = gtk::Overlay::new();
        preview_overlay.set_child(Some(&preview));
        preview_overlay.add_overlay(&spinner);
        preview_card.append(&preview_overlay);
        top_row.append(&preview_card);

        // Controls card (right side, fills remaining width)
        let controls_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["card"])
            .hexpand(true)
            .valign(gtk::Align::Start)
            .build();

        let controls_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_start(16)
            .margin_end(16)
            .margin_top(16)
            .margin_bottom(16)
            .build();

        // Brightness row
        let brightness_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let brightness_label = gtk::Label::builder()
            .label("Brightness")
            .halign(gtk::Align::Start)
            .width_request(100)
            .build();
        let brightness_adj =
            gtk::Adjustment::new(status.brightness as f64, 0.0, 100.0, 1.0, 10.0, 0.0);
        let brightness_scale = gtk::Scale::builder()
            .adjustment(&brightness_adj)
            .orientation(gtk::Orientation::Horizontal)
            .draw_value(true)
            .value_pos(gtk::PositionType::Right)
            .hexpand(true)
            .build();
        brightness_scale.set_digits(0);
        brightness_row.append(&brightness_label);
        brightness_row.append(&brightness_scale);
        controls_inner.append(&brightness_row);

        // Rotation row
        let rotation_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let rotation_label = gtk::Label::builder()
            .label("Rotation")
            .halign(gtk::Align::Start)
            .width_request(100)
            .build();
        let rotation_options = gtk::StringList::new(&["0°", "90°", "180°", "270°"]);
        let rotation_selected = (status.rotation / 90).min(3);
        let rotation_drop = gtk::DropDown::builder()
            .model(&rotation_options)
            .selected(rotation_selected)
            .build();
        rotation_row.append(&rotation_label);
        rotation_row.append(&rotation_drop);
        controls_inner.append(&rotation_row);

        // Reset button
        let reset_btn = gtk::Button::builder()
            .label("Reset to Default")
            .halign(gtk::Align::End)
            .css_classes(["destructive-action"])
            .build();
        controls_inner.append(&reset_btn);

        controls_card.append(&controls_inner);
        top_row.append(&controls_card);
        root.append(&top_row);

        // ── LCD Engine section ────────────────────────────────────────────
        let engine_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["card"])
            .build();

        let engine_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_start(16)
            .margin_end(16)
            .margin_top(16)
            .margin_bottom(16)
            .build();

        let engine_heading_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let engine_heading = gtk::Label::builder()
            .label("LCD Engine")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .css_classes(["heading"])
            .build();

        let engine_status_label = gtk::Label::builder()
            .label("")
            .halign(gtk::Align::End)
            .css_classes(["dim-label"])
            .build();

        engine_heading_row.append(&engine_heading);
        engine_heading_row.append(&engine_status_label);
        engine_inner.append(&engine_heading_row);

        let engine_template_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let engine_template_label = gtk::Label::builder()
            .label("Template")
            .halign(gtk::Align::Start)
            .width_request(100)
            .build();

        let engine_dropdown = gtk::DropDown::builder()
            .hexpand(true)
            .build();

        engine_template_row.append(&engine_template_label);
        engine_template_row.append(&engine_dropdown);
        engine_inner.append(&engine_template_row);

        // Per-template parameter widgets are rebuilt into this box whenever the
        // selected template changes (see rebuild_param_controls in engine_panel.rs).
        let param_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        engine_inner.append(&param_box);

        let engine_btn_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .halign(gtk::Align::End)
            .build();

        let engine_activate_btn = gtk::Button::builder()
            .label("Use Engine")
            .css_classes(["suggested-action"])
            .build();

        let engine_deactivate_btn = gtk::Button::builder()
            .label("Deactivate")
            .css_classes(["destructive-action"])
            .visible(false)
            .build();

        engine_btn_row.append(&engine_deactivate_btn);
        engine_btn_row.append(&engine_activate_btn);
        engine_inner.append(&engine_btn_row);

        engine_card.append(&engine_inner);
        root.append(&engine_card);

        let engine_templates: Rc<RefCell<Vec<LcdEngineTemplateDescriptor>>> =
            Rc::new(RefCell::new(Vec::new()));
        let sensors: Rc<RefCell<Vec<(String, Sensor)>>> = Rc::new(RefCell::new(Vec::new()));
        let param_getters: Rc<RefCell<Vec<(String, ParamGetter)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let engine_active: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));

        // ── Image Library ─────────────────────────────────────────────────
        let library_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["card"])
            .build();

        let library_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_start(16)
            .margin_end(16)
            .margin_top(16)
            .margin_bottom(16)
            .build();

        let library_heading_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let library_heading = gtk::Label::builder()
            .label("Image Library")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .css_classes(["heading"])
            .build();

        let upload_btn = gtk::Button::builder()
            .label("Upload…")
            .css_classes(["suggested-action"])
            .build();

        library_heading_row.append(&library_heading);
        library_heading_row.append(&upload_btn);
        library_inner.append(&library_heading_row);

        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(120)
            .max_content_height(240)
            .vexpand(false)
            .build();

        let library_box = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(8)
            .min_children_per_line(2)
            .column_spacing(8)
            .row_spacing(8)
            .homogeneous(true)
            .build();

        scroll.set_child(Some(&library_box));
        library_inner.append(&scroll);
        library_card.append(&library_inner);
        root.append(&library_card);

        let widget = Self {
            root,
            preview,
            preview_source,
            anim_timer,
            active_image_rotation,
            library_box,
            upload_btn: upload_btn.clone(),
            spinner,
            pending: Rc::new(RefCell::new(HashMap::new())),
            device_id: device_id.to_string(),
            store: store.clone(),
            engine_dropdown: engine_dropdown.clone(),
            engine_activate_btn: engine_activate_btn.clone(),
            engine_deactivate_btn: engine_deactivate_btn.clone(),
            engine_status_label,
            param_box,
            engine_templates,
            sensors,
            param_getters,
            engine_active,
        };

        // Wire up controls.
        widget.connect_brightness(&brightness_scale);
        widget.connect_rotation(&rotation_drop);
        widget.connect_reset(&reset_btn);
        widget.connect_upload(&upload_btn);
        widget.connect_engine_activate(&engine_activate_btn);
        widget.connect_engine_deactivate(&engine_deactivate_btn);
        widget.connect_engine_template_changed(&engine_dropdown);

        // Request library list from daemon.
        widget.request_library_refresh();

        widget
    }

    /// Load image bytes into the preview asynchronously.
    fn set_preview_from_bytes(&self, data: Vec<u8>) {
        if let Some(id) = self.anim_timer.borrow_mut().take() {
            id.remove();
        }
        let preview_source = Rc::clone(&self.preview_source);
        let anim_timer = Rc::clone(&self.anim_timer);
        let preview = self.preview.clone();
        let bytes = gtk::glib::Bytes::from_owned(data);
        let stream = gtk::gio::MemoryInputStream::from_bytes(&bytes);
        gtk::glib::MainContext::default().spawn_local(async move {
            if let Ok(anim) =
                gtk::gdk_pixbuf::PixbufAnimation::from_stream_future(&stream).await
            {
                Self::apply_animation(&preview, &preview_source, &anim_timer, anim);
            }
        });
    }

    fn clear_preview(&self) {
        if let Some(id) = self.anim_timer.borrow_mut().take() {
            id.remove();
        }
        *self.preview_source.borrow_mut() = None;
        self.preview.queue_draw();
    }

    fn connect_brightness(&self, scale: &gtk::Scale) {
        let store = self.store.clone();
        let id = self.device_id.clone();
        scale.connect_change_value(move |_, _, value| {
            store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "set_screen_brightness",
                "id": id,
                "brightness": value.round() as u8,
            })));
            gtk::glib::Propagation::Proceed
        });
    }

    fn connect_rotation(&self, drop: &gtk::DropDown) {
        let store = self.store.clone();
        let id = self.device_id.clone();
        let rot_ref = Rc::clone(&self.active_image_rotation);
        let preview = self.preview.clone();
        drop.connect_selected_notify(move |d| {
            let degrees = d.selected() * 90;
            *rot_ref.borrow_mut() = degrees;
            preview.queue_draw();
            store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "set_screen_rotation",
                "id": id,
                "degrees": degrees,
            })));
        });
    }

    fn connect_reset(&self, btn: &gtk::Button) {
        let store = self.store.clone();
        let id = self.device_id.clone();
        let anim_timer = Rc::clone(&self.anim_timer);
        let preview_source = Rc::clone(&self.preview_source);
        let preview = self.preview.clone();
        btn.connect_clicked(move |_| {
            if let Some(tid) = anim_timer.borrow_mut().take() {
                tid.remove();
            }
            *preview_source.borrow_mut() = None;
            preview.queue_draw();
            store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "set_screen_default",
                "id": id,
            })));
        });
    }

    fn connect_engine_activate(&self, btn: &gtk::Button) {
        let widget = self.clone();
        btn.connect_clicked(move |_| widget.send_set_template());
    }

    fn connect_engine_deactivate(&self, btn: &gtk::Button) {
        let store = self.store.clone();
        let id = self.device_id.clone();
        btn.connect_clicked(move |_| {
            store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "lcd_engine_deactivate",
                "device_id": id,
            })));
        });
    }

    fn connect_upload(&self, btn: &gtk::Button) {
        let store = self.store.clone();
        let id = self.device_id.clone();
        let widget = self.clone();

        btn.connect_clicked(move |btn| {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Images"));
            filter.add_mime_type("image/png");
            filter.add_mime_type("image/jpeg");
            filter.add_mime_type("image/gif");

            let filter_list = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filter_list.append(&filter);

            let dialog = gtk::FileDialog::builder()
                .title("Choose Image")
                .filters(&filter_list)
                .modal(true)
                .build();

            let store_c = store.clone();
            let id_c = id.clone();
            let widget_c = widget.clone();
            let parent = btn
                .root()
                .and_then(|r| r.downcast::<gtk::Window>().ok());
            dialog.open(parent.as_ref(), gtk::gio::Cancellable::NONE, move |res| {
                if let Ok(file) = res {
                    if let Some(path) = file.path() {
                        if let Ok(data) = std::fs::read(&path) {
                            let req_id = uuid::Uuid::new_v4().to_string();
                            let mime = if data.starts_with(b"GIF") { "image/gif" }
                                       else if data.starts_with(&[0xFF, 0xD8]) { "image/jpeg" }
                                       else { "image/png" };

                            // Disable button + show spinner; preview applied on daemon ACK.
                            widget_c.upload_btn.set_sensitive(false);
                            widget_c.show_loading();

                            let widget_t = widget_c.clone();
                            let req_id_t = req_id.clone();
                            let timeout_id = gtk::glib::timeout_add_local_once(
                                std::time::Duration::from_secs(120),
                                move || widget_t.on_request_timeout(&req_id_t),
                            );
                            widget_c.pending.borrow_mut().insert(req_id.clone(), PendingItem {
                                payload: PendingPayload::Bytes(data.clone()),
                                timeout_id,
                            });

                            store_c.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                                "type": "set_screen_image",
                                "id": id_c,
                                "request_id": req_id,
                            })));
                            store_c.ipc().send_binary(req_id, mime, data);
                        }
                    }
                }
            });
        });
    }

    fn show_loading(&self) {
        self.spinner.set_visible(true);
        self.spinner.start();
    }

    fn hide_loading(&self) {
        self.spinner.stop();
        self.spinner.set_visible(false);
    }

    fn show_toast_error(&self, msg: &str) {
        let toast = adw::Toast::new(msg);
        let mut w: gtk::Widget = self.preview.clone().upcast();
        loop {
            match w.parent() {
                Some(parent) => {
                    if let Ok(overlay) = parent.clone().downcast::<adw::ToastOverlay>() {
                        overlay.add_toast(toast);
                        return;
                    }
                    w = parent;
                }
                None => return,
            }
        }
    }

    fn on_request_timeout(&self, req_id: &str) {
        if self.pending.borrow_mut().remove(req_id).is_some() {
            self.show_toast_error("LCD image upload timed out");
        }
        if self.pending.borrow().is_empty() {
            self.upload_btn.set_sensitive(true);
            self.hide_loading();
        }
    }

    /// Called when the daemon confirms an image apply succeeded.
    pub fn on_image_uploaded(&self, req_id: &str) {
        let item = self.pending.borrow_mut().remove(req_id);
        if let Some(item) = item {
            item.timeout_id.remove();
            match item.payload {
                PendingPayload::Bytes(data) => self.set_preview_from_bytes(data),
                PendingPayload::File(Some(path)) => self.load_preview_from_file_async(path),
                PendingPayload::File(None) => {}
            }
            self.store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({ "type": "list_lcd_images" })));
        }
        if self.pending.borrow().is_empty() {
            self.upload_btn.set_sensitive(true);
            self.hide_loading();
        }
    }

    /// Called on any daemon error — cancels all in-flight timers and re-enables controls.
    pub fn on_upload_error(&self) {
        let items: Vec<PendingItem> = self.pending.borrow_mut().drain().map(|(_, v)| v).collect();
        for item in items {
            item.timeout_id.remove();
        }
        self.upload_btn.set_sensitive(true);
        self.hide_loading();
    }

    fn request_library_refresh(&self) {
        self.store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({ "type": "list_lcd_images" })));
    }

    /// Populate the library with a fresh list of image filenames from the daemon.
    pub fn update_library(&self, files: &[serde_json::Value]) {
        while let Some(child) = self.library_box.first_child() {
            self.library_box.remove(&child);
        }

        for file in files {
            let name = file["name"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            let tile = self.make_library_tile(&name);
            self.library_box.append(&tile);
        }

        if files.is_empty() {
            let empty = gtk::Label::builder()
                .label("No images yet. Upload one!")
                .css_classes(["dim-label"])
                .margin_top(8)
                .margin_bottom(8)
                .build();
            self.library_box.append(&empty);
        }
    }

    fn make_library_tile(&self, filename: &str) -> gtk::Overlay {
        let dir = Self::lcd_images_dir_client();
        let path = dir.as_ref().map(|d| d.join(filename));

        let thumb = gtk::DrawingArea::builder()
            .content_width(72)
            .content_height(72)
            .css_classes(["lcd-thumb"])
            .build();

        // Load thumbnail asynchronously so the main thread is never blocked.
        let pixbuf: Rc<RefCell<Option<gtk::gdk_pixbuf::Pixbuf>>> = Rc::new(RefCell::new(None));
        if let Some(path_clone) = path.clone() {
            let pixbuf_rc = Rc::clone(&pixbuf);
            let thumb_c = thumb.clone();
            let file = gtk::gio::File::for_path(&path_clone);
            gtk::glib::MainContext::default().spawn_local(async move {
                let Ok(stream) = file.read_future(gtk::glib::Priority::DEFAULT).await else {
                    return;
                };
                if let Ok(anim) =
                    gtk::gdk_pixbuf::PixbufAnimation::from_stream_future(&stream).await
                {
                    let pb = if anim.is_static_image() {
                        anim.static_image()
                    } else {
                        Some(anim.iter(None).pixbuf())
                    };
                    *pixbuf_rc.borrow_mut() = pb;
                    thumb_c.queue_draw();
                }
            });
        }

        {
            let pb_ref = Rc::clone(&pixbuf);
            thumb.set_draw_func(move |_, cr, w, h| {
                if let Some(ref pb) = *pb_ref.borrow() {
                    draw_thumb(cr, w, h, pb);
                } else {
                    cr.set_source_rgb(0.15, 0.15, 0.15);
                    let _ = cr.paint();
                }
            });
        }

        // Delete button overlay (top-right corner).
        let del_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["flat", "circular"])
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(2)
            .margin_end(2)
            .build();

        let overlay = gtk::Overlay::builder().child(&thumb).build();
        overlay.add_overlay(&del_btn);

        let store_del = self.store.clone();
        let fname = filename.to_string();
        let library_box = self.library_box.clone();
        let overlay_weak = overlay.downgrade();
        del_btn.connect_clicked(move |_| {
            store_del.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "delete_lcd_image",
                "filename": fname,
            })));
            if let Some(t) = overlay_weak.upgrade() {
                library_box.remove(&t);
            }
        });

        // Click thumb → send IPC command; show spinner, apply preview only on daemon ACK.
        let gesture = gtk::GestureClick::new();
        let store_apply = self.store.clone();
        let device_id = self.device_id.clone();
        let fname_apply = filename.to_string();
        let widget = self.clone();
        let dir_opt = Self::lcd_images_dir_client();
        let fname_load = filename.to_string();
        gesture.connect_pressed(move |_, _, _, _| {
            let req_id = uuid::Uuid::new_v4().to_string();
            store_apply.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "set_screen_image_from_library",
                "id": device_id,
                "filename": fname_apply,
                "request_id": req_id,
            })));

            let path = dir_opt.as_ref().map(|d| d.join(&fname_load));
            let widget_t = widget.clone();
            let req_id_t = req_id.clone();
            let timeout_id = gtk::glib::timeout_add_local_once(
                std::time::Duration::from_secs(120),
                move || widget_t.on_request_timeout(&req_id_t),
            );
            widget.pending.borrow_mut().insert(req_id, PendingItem {
                payload: PendingPayload::File(path),
                timeout_id,
            });
            widget.show_loading();
        });
        thumb.add_controller(gesture);

        overlay
    }

    fn load_preview_from_file_async(&self, path: std::path::PathBuf) {
        let preview_source = Rc::clone(&self.preview_source);
        let anim_timer = Rc::clone(&self.anim_timer);
        let preview = self.preview.clone();
        let file = gtk::gio::File::for_path(&path);
        gtk::glib::MainContext::default().spawn_local(async move {
            let Ok(stream) = file.read_future(gtk::glib::Priority::DEFAULT).await else {
                return;
            };
            if let Ok(anim) =
                gtk::gdk_pixbuf::PixbufAnimation::from_stream_future(&stream).await
            {
                Self::apply_animation(&preview, &preview_source, &anim_timer, anim);
            }
        });
    }

    fn apply_animation(
        preview: &gtk::DrawingArea,
        preview_source: &Rc<RefCell<Option<PreviewSource>>>,
        anim_timer: &Rc<RefCell<Option<gtk::glib::SourceId>>>,
        anim: gtk::gdk_pixbuf::PixbufAnimation,
    ) {
        if let Some(id) = anim_timer.borrow_mut().take() {
            id.remove();
        }
        if anim.is_static_image() {
            if let Some(pb) = anim.static_image() {
                *preview_source.borrow_mut() = Some(PreviewSource::Static(pb));
            }
        } else {
            let iter = anim.iter(None);
            *preview_source.borrow_mut() = Some(PreviewSource::Animated(iter.clone()));
            let source_ref = Rc::clone(preview_source);
            let preview_c = preview.clone();
            let timer_ref = Rc::clone(anim_timer);
            let id = gtk::glib::timeout_add_local(
                std::time::Duration::from_millis(40),
                move || {
                    let advanced = {
                        let src = source_ref.borrow();
                        if let Some(PreviewSource::Animated(ref it)) = *src {
                            it.advance(std::time::SystemTime::now())
                        } else {
                            false
                        }
                    };
                    if advanced {
                        preview_c.queue_draw();
                    }
                    gtk::glib::ControlFlow::Continue
                },
            );
            *timer_ref.borrow_mut() = Some(id);
        }
        preview.queue_draw();
    }

    fn lcd_images_dir_client() -> Option<std::path::PathBuf> {
        #[cfg(target_os = "windows")]
        {
            let appdata = std::env::var("APPDATA").ok()?;
            Some(std::path::PathBuf::from(appdata).join("halod").join("lcd_images"))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let home = std::env::var("HOME").ok()?;
            Some(
                std::path::PathBuf::from(home)
                    .join(".config")
                    .join("halod")
                    .join("lcd_images"),
            )
        }
    }

    /// Called on every state broadcast. Updates the preview if active_image changed.
    /// Does NOT update brightness slider or rotation dropdown (user-controlled inputs).
    pub fn update_live(&self, status: &LcdStatus) {
        if status.mode == LcdMode::Engine {
            // Preview is driven by on_engine_frame — don't interfere with it.
            return;
        }
        match &status.active_image {
            None => {
                let has_image = self.preview_source.borrow().is_some();
                if has_image {
                    self.clear_preview();
                }
            }
            Some(filename) => {
                // Only reload if no preview cached and no ACK pending (ACK handler sets preview).
                if self.preview_source.borrow().is_none()
                    && self.pending.borrow().is_empty()
                {
                    if let Some(dir) = Self::lcd_images_dir_client() {
                        self.load_preview_from_file_async(dir.join(filename));
                    }
                }
            }
        }
    }

    /// Called on each state broadcast to sync the engine dropdown, param
    /// widgets, and button states.
    pub fn update_engine_section(
        &self,
        engine_state: &WireLcdEngineState,
        all_sensors: &[(String, Sensor)],
    ) {
        *self.sensors.borrow_mut() = all_sensors.to_vec();

        // Rebuild the dropdown (and param controls) if the template list changed.
        let current_ids: Vec<String> =
            self.engine_templates.borrow().iter().map(|t| t.id.clone()).collect();
        let new_ids: Vec<String> =
            engine_state.available_templates.iter().map(|t| t.id.clone()).collect();
        if current_ids != new_ids {
            *self.engine_templates.borrow_mut() = engine_state.available_templates.clone();
            let names: Vec<&str> =
                engine_state.available_templates.iter().map(|t| t.name.as_str()).collect();
            let model = gtk::StringList::new(&names);
            self.engine_dropdown.set_model(Some(&model));
            self.rebuild_selected_param_controls();
        }

        let engine_active = engine_state.device_templates.contains_key(&self.device_id);
        *self.engine_active.borrow_mut() = engine_active;
        if engine_active {
            let template_id = engine_state
                .device_templates
                .get(&self.device_id)
                .map(|s| s.as_str())
                .unwrap_or("");
            let tmpl_name = engine_state
                .available_templates
                .iter()
                .find(|t| t.id == template_id)
                .map(|t| t.name.as_str())
                .unwrap_or(template_id);
            self.engine_status_label.set_label(&format!("Active: {tmpl_name}"));
            self.engine_activate_btn.set_visible(false);
            self.engine_deactivate_btn.set_visible(true);
            // Select current template in dropdown.
            let pos = self
                .engine_templates
                .borrow()
                .iter()
                .position(|t| t.id == template_id);
            if let Some(pos) = pos {
                self.engine_dropdown.set_selected(pos as u32);
            }
        } else {
            self.engine_status_label.set_label("");
            self.engine_activate_btn.set_visible(true);
            self.engine_deactivate_btn.set_visible(false);
        }
    }

    /// Called when an LCD engine frame arrives for any device; filters by device_id.
    pub fn on_engine_frame(&self, frame: &LcdEngineFrame) {
        if frame.device_id != self.device_id {
            return;
        }
        use base64::Engine as _;
        if let Ok(png) = base64::engine::general_purpose::STANDARD.decode(&frame.preview_b64) {
            self.set_preview_from_bytes(png);
        }
    }
}
