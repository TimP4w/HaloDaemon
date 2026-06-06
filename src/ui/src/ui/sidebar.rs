use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::state::AppState;
use crate::store::{NavTarget, Store};
use halod_protocol::types::DiscoveryPhase;

#[derive(Clone)]
pub struct Sidebar {
    pub root: gtk::Box,
    home_btn: gtk::Button,
    canvas_btn: gtk::Button,
    cooling_btn: gtk::Button,
    lighting_btn: gtk::Button,
    rules_btn: gtk::Button,
    settings_btn: gtk::Button,
    home_bar: gtk::Box,
    canvas_bar: gtk::Box,
    cooling_bar: gtk::Box,
    lighting_bar: gtk::Box,
    rules_bar: gtk::Box,
    settings_bar: gtk::Box,
    conn_pill: gtk::Label,
}

impl Sidebar {
    pub fn new(store: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(64)
            .css_classes(["icon-sidebar"])
            .build();

        root.append(&logo_image());

        let home_btn = gtk::Button::builder()
            .icon_name("go-home-symbolic")
            .tooltip_text("Home")
            .css_classes(["flat", "icon-nav-btn", "active"])
            .build();
        let store_h = store.clone();
        home_btn.connect_clicked(move |_| store_h.navigate(NavTarget::Home));

        let home_bar = gtk::Box::builder()
            .css_classes(["sidebar-active-bar"])
            .valign(gtk::Align::Center)
            .build();

        let home_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        home_row.append(&home_btn);
        home_row.append(&home_bar);
        root.append(&home_row);

        let canvas_btn = gtk::Button::builder()
            .icon_name("applications-graphics-symbolic")
            .tooltip_text("Canvas")
            .css_classes(["flat", "icon-nav-btn"])
            .build();
        let store_c = store.clone();
        canvas_btn.connect_clicked(move |_| store_c.navigate(NavTarget::Canvas));

        let canvas_bar = gtk::Box::builder()
            .css_classes(["sidebar-active-bar", "hidden"])
            .valign(gtk::Align::Center)
            .build();

        let canvas_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        canvas_row.append(&canvas_btn);
        canvas_row.append(&canvas_bar);
        root.append(&canvas_row);

        let cooling_btn = gtk::Button::builder()
            .icon_name("fan-symbolic")
            .tooltip_text("Cooling")
            .css_classes(["flat", "icon-nav-btn"])
            .build();
        let store_co = store.clone();
        cooling_btn.connect_clicked(move |_| store_co.navigate(NavTarget::Cooling));

        let cooling_bar = gtk::Box::builder()
            .css_classes(["sidebar-active-bar", "hidden"])
            .valign(gtk::Align::Center)
            .build();

        let cooling_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        cooling_row.append(&cooling_btn);
        cooling_row.append(&cooling_bar);
        root.append(&cooling_row);

        let lighting_btn = gtk::Button::builder()
            .icon_name("rgb-strip-symbolic")
            .tooltip_text("Lighting")
            .css_classes(["flat", "icon-nav-btn"])
            .build();
        let store_l = store.clone();
        lighting_btn.connect_clicked(move |_| store_l.navigate(NavTarget::Lighting));

        let lighting_bar = gtk::Box::builder()
            .css_classes(["sidebar-active-bar", "hidden"])
            .valign(gtk::Align::Center)
            .build();

        let lighting_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        lighting_row.append(&lighting_btn);
        lighting_row.append(&lighting_bar);
        root.append(&lighting_row);

        let rules_btn = gtk::Button::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text("App Rules")
            .css_classes(["flat", "icon-nav-btn"])
            .build();
        let store_r = store.clone();
        rules_btn.connect_clicked(move |_| store_r.navigate(NavTarget::AppRules));

        let rules_bar = gtk::Box::builder()
            .css_classes(["sidebar-active-bar", "hidden"])
            .valign(gtk::Align::Center)
            .build();

        let rules_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        rules_row.append(&rules_btn);
        rules_row.append(&rules_bar);
        root.append(&rules_row);

        // Spacer pushes the settings button to the bottom.
        root.append(&gtk::Box::builder().vexpand(true).build());

        let settings_btn = gtk::Button::builder()
            .icon_name("preferences-system-symbolic")
            .tooltip_text("Settings")
            .css_classes(["flat", "icon-nav-btn"])
            .build();
        let store_s = store.clone();
        settings_btn.connect_clicked(move |_| store_s.navigate(NavTarget::Settings));

        let settings_bar = gtk::Box::builder()
            .css_classes(["sidebar-active-bar", "hidden"])
            .valign(gtk::Align::Center)
            .build();

        let settings_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        settings_row.append(&settings_btn);
        settings_row.append(&settings_bar);
        root.append(&settings_row);

        let conn_pill = gtk::Label::builder()
            .label("···")
            .css_classes(["conn-pill", "connecting"])
            .margin_bottom(12)
            .build();
        root.append(&conn_pill);

        Sidebar {
            root,
            home_btn,
            canvas_btn,
            cooling_btn,
            lighting_btn,
            rules_btn,
            settings_btn,
            home_bar,
            canvas_bar,
            cooling_bar,
            lighting_bar,
            rules_bar,
            settings_bar,
            conn_pill,
        }
    }

    pub fn set_active_page(&self, page: &str) {
        for (btn, bar, name) in [
            (&self.home_btn, &self.home_bar, "home"),
            (&self.canvas_btn, &self.canvas_bar, "canvas"),
            (&self.cooling_btn, &self.cooling_bar, "cooling"),
            (&self.lighting_btn, &self.lighting_bar, "lighting"),
            (&self.rules_btn, &self.rules_bar, "app_rules"),
            (&self.settings_btn, &self.settings_bar, "settings"),
        ] {
            if name == page {
                btn.add_css_class("active");
                bar.remove_css_class("hidden");
            } else {
                btn.remove_css_class("active");
                bar.add_css_class("hidden");
            }
        }
    }

    pub fn update(&self, state: &AppState, connected: bool) {
        if connected {
            self.conn_pill.remove_css_class("disconnected");
            self.conn_pill.remove_css_class("connecting");
            self.conn_pill.add_css_class("connected");
            self.conn_pill.set_label("●");
        } else {
            self.conn_pill.remove_css_class("connected");
            self.conn_pill.remove_css_class("connecting");
            self.conn_pill.add_css_class("disconnected");
            self.conn_pill.set_label("●");
        }

        let navigable = matches!(state.discovery.phase, DiscoveryPhase::Complete);
        self.canvas_btn.set_sensitive(navigable);
        self.cooling_btn.set_sensitive(navigable);
        self.lighting_btn.set_sensitive(navigable);
        self.rules_btn.set_sensitive(navigable);
        self.settings_btn.set_sensitive(navigable);
    }
}

fn logo_image() -> gtk::DrawingArea {
    use cairo::Format;
    use resvg::{tiny_skia, usvg};

    const PX: u32 = 40;
    // Render at 3× physical pixels so the image is sharp on HiDPI displays.
    // Cairo's draw_func receives logical-pixel dimensions and already accounts
    // for the device scale, so drawing 0..PX in user space is always correct.
    const RENDER_PX: u32 = PX * 3;

    let svg = include_bytes!("../../../../assets/icon.svg");
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg, &opt).expect("valid SVG");

    let svg_w = tree.size().to_int_size().width() as f32;
    let scale = RENDER_PX as f32 / svg_w;
    let mut pixmap = tiny_skia::Pixmap::new(RENDER_PX, RENDER_PX).expect("pixmap");
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );

    // tiny_skia outputs RGBA; Cairo ARgb32 is native-endian ARGB (= BGRA on LE).
    let stride = Format::ARgb32.stride_for_width(RENDER_PX).expect("stride") as usize;
    let mut cairo_data = vec![0u8; stride * RENDER_PX as usize];
    for (y, row) in pixmap.pixels().chunks(RENDER_PX as usize).enumerate() {
        for (x, px) in row.iter().enumerate() {
            let o = y * stride + x * 4;
            cairo_data[o] = px.blue();
            cairo_data[o + 1] = px.green();
            cairo_data[o + 2] = px.red();
            cairo_data[o + 3] = px.alpha();
        }
    }

    let surface = cairo::ImageSurface::create_for_data(
        cairo_data,
        Format::ARgb32,
        RENDER_PX as i32,
        RENDER_PX as i32,
        stride as i32,
    )
    .expect("cairo surface");

    let da = gtk::DrawingArea::new();
    da.set_content_width(PX as i32);
    da.set_content_height(PX as i32);
    da.set_halign(gtk::Align::Center);
    da.set_valign(gtk::Align::Center);
    da.set_margin_top(20);
    da.set_margin_bottom(12);

    da.set_draw_func(move |_, cr, width, height| {
        let sx = width as f64 / RENDER_PX as f64;
        let sy = height as f64 / RENDER_PX as f64;
        cr.scale(sx, sy);
        let pattern = cairo::SurfacePattern::create(&surface);
        pattern.set_filter(cairo::Filter::Good);
        cr.set_source(&pattern).unwrap();
        cr.paint().unwrap();
    });

    da
}
