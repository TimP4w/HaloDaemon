mod chains;
mod zone_editor;
mod effect_params;

use zone_editor::{led_pos, draw_leds, LED_R};
use effect_params::build_effect_content;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use halod_protocol::types::{
    DeviceCapability, EffectParamValue, LedId, RgbColor, RgbDescriptor, RgbState, RgbStatus,
    RgbZone, WireDevice, ZoneTopology,
};
use halod_protocol::zone_transform::ZoneContentTransform;

pub(super) fn gdk_rgba(color: RgbColor) -> gtk::gdk::RGBA {
    gtk::gdk::RGBA::new(
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
        1.0,
    )
}

// ── Internal mode enum ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub(super) enum Mode {
    Static,
    PerLed,
    Effect { id: String },
    Engine,
}

fn mode_eq(a: &Mode, b: &Mode) -> bool {
    match (a, b) {
        (Mode::Static, Mode::Static) => true,
        (Mode::PerLed, Mode::PerLed) => true,
        (Mode::Engine, Mode::Engine) => true,
        (Mode::Effect { id: a }, Mode::Effect { id: b }) => a == b,
        _ => false,
    }
}

/// Mode dropdown entries. Native firmware effects are only listed while
/// `host_mode` is true — the firmware rejects `SetEffect` outside host mode,
/// so offering them then would just produce errors.
fn build_mode_entries(descriptor: &RgbDescriptor, host_mode: bool) -> Vec<(String, Mode)> {
    let mut entries: Vec<(String, Mode)> = vec![
        ("Static".into(), Mode::Static),
        ("Per LED".into(), Mode::PerLed),
    ];
    if host_mode {
        for e in &descriptor.native_effects {
            entries.push((e.name.clone(), Mode::Effect { id: e.id.clone() }));
        }
    }
    entries
}

fn mode_string_list(entries: &[(String, Mode)]) -> gtk::StringList {
    let list = gtk::StringList::new(&[]);
    for (name, _) in entries {
        list.append(name);
    }
    list
}

/// Whether native effects may be offered for this device. A device with a
/// `host_mode` boolean must be in host mode; one without the toggle (non-HID++
/// devices) is always allowed.
pub(super) fn effects_allowed(device: &WireDevice) -> bool {
    device
        .capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Boolean(bs) => {
                bs.iter().find(|b| b.key == "host_mode").map(|b| b.value)
            }
            _ => None,
        })
        .unwrap_or(true)
}

// ── Widget state ────────────────────────────────────────────────────────────────

pub(super) struct WState {
    pub(super) mode: Mode,
    /// Per-zone LED colors: zone_id → led_id → color.
    pub(super) zone_led_colors: HashMap<String, HashMap<LedId, RgbColor>>,
    /// Which zone is shown in the canvas (index into descriptor.zones).
    pub(super) selected_zone_idx: usize,
    pub(super) selected: HashSet<LedId>,
    pub(super) color: RgbColor,
    pub(super) effect_params: HashMap<String, EffectParamValue>,
    pub(super) rubber_start: Option<(f64, f64)>,
    pub(super) rubber_end: (f64, f64),
    /// Per-zone LED-content transforms, keyed by zone id.
    pub(super) zone_transforms: HashMap<String, ZoneContentTransform>,
}

impl WState {
    pub(super) fn current_rgb(&self) -> RgbColor {
        self.color
    }

    pub(super) fn current_zone<'a>(&self, descriptor: &'a RgbDescriptor) -> Option<&'a RgbZone> {
        descriptor.zones.get(self.selected_zone_idx)
    }

    pub(super) fn current_zone_colors(&self, descriptor: &RgbDescriptor) -> Option<&HashMap<LedId, RgbColor>> {
        let zone_id = &self.current_zone(descriptor)?.id;
        self.zone_led_colors.get(zone_id)
    }

    fn build_ipc_state(&self, descriptor: &RgbDescriptor) -> RgbState {
        match &self.mode {
            Mode::Static => RgbState::Static { color: self.current_rgb() },
            Mode::PerLed => {
                let mut zones = HashMap::new();
                for zone in &descriptor.zones {
                    let string_map: HashMap<String, RgbColor> = self
                        .zone_led_colors
                        .get(&zone.id)
                        .map(|m| m.iter().map(|(id, c)| (id.to_string(), *c)).collect())
                        .unwrap_or_default();
                    zones.insert(zone.id.clone(), string_map);
                }
                RgbState::PerLed { zones }
            }
            Mode::Effect { id } => {
                RgbState::NativeEffect { id: id.clone(), params: self.effect_params.clone() }
            }
            Mode::Engine => RgbState::Engine,
        }
    }
}

// ── Public widget ───────────────────────────────────────────────────────────────

pub struct RgbWidget {
    pub root: gtk::Box,
    /// Rebuilds the mode dropdown for a new host-mode state. Invoked from
    /// `update_live` only when host mode actually transitions.
    apply_host_mode: Box<dyn Fn(bool)>,
    last_host_mode: RefCell<bool>,
    chains_container: chains::ChainsContainer,
    /// Comparing structural signatures avoids rebuilding mid-edit and wiping
    /// half-typed inline Entry text on every 250 ms broadcast.
    last_chains_sig: RefCell<u64>,
    device_id: String,
    store: Store,
}

impl RgbWidget {
    pub fn build(device_id: &str, status: &RgbStatus, device: &WireDevice, store: &Store) -> Self {
        let descriptor = Rc::new(status.descriptor.clone());
        let device_id = Rc::new(device_id.to_string());
        let host_mode = effects_allowed(device);

        // ── Initialise per-zone LED colors ───────────────────────────────────
        let mut zone_led_colors: HashMap<String, HashMap<LedId, RgbColor>> = HashMap::new();
        for zone in &descriptor.zones {
            let mut leds = HashMap::new();
            for led in &zone.leds {
                leds.insert(led.id, RgbColor { r: 0, g: 0, b: 0 });
            }
            zone_led_colors.insert(zone.id.clone(), leds);
        }

        let mut init_mode = Mode::Static;
        let mut init_color = RgbColor { r: 255, g: 0, b: 0 };
        let mut init_params: HashMap<String, EffectParamValue> = HashMap::new();

        if let Some(daemon_state) = &status.state {
            match daemon_state {
                RgbState::Static { color } => {
                    for led_map in zone_led_colors.values_mut() {
                        for v in led_map.values_mut() {
                            *v = *color;
                        }
                    }
                    init_color = *color;
                }
                RgbState::PerLed { zones } => {
                    init_mode = Mode::PerLed;
                    for (zone_id, colors) in zones {
                        if let Some(led_map) = zone_led_colors.get_mut(zone_id) {
                            for (id_str, c) in colors {
                                if let Ok(id) = id_str.parse::<LedId>() {
                                    led_map.insert(id, *c);
                                }
                            }
                        }
                    }
                }
                RgbState::NativeEffect { id, params } => {
                    init_mode = Mode::Effect { id: id.clone() };
                    init_params = params.clone();
                }
                RgbState::Engine => {
                    init_mode = Mode::Engine;
                }
            }
        }

        let state = Rc::new(RefCell::new(WState {
            mode: init_mode.clone(),
            zone_led_colors,
            selected_zone_idx: 0,
            selected: HashSet::new(),
            color: init_color,
            effect_params: init_params,
            rubber_start: None,
            rubber_end: (0.0, 0.0),
            zone_transforms: status.zone_transforms.clone(),
        }));

        // ── Build mode entry list ─────────────────────────────────────────────
        // Shared so update_live can rebuild it when host mode changes.
        let mode_entries: Rc<RefCell<Vec<(String, Mode)>>> =
            Rc::new(RefCell::new(build_mode_entries(&descriptor, host_mode)));

        let mode_model = mode_string_list(&mode_entries.borrow());
        let mode_dropdown = gtk::DropDown::builder().model(&mode_model).build();

        let init_idx = mode_entries
            .borrow()
            .iter()
            .position(|(_, m)| mode_eq(&init_mode, m))
            .unwrap_or(0) as u32;
        mode_dropdown.set_selected(init_idx);

        // ── Root layout ───────────────────────────────────────────────────────
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(16)
            .build();

        let mode_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        let mode_lbl = gtk::Label::builder()
            .label("MODE")
            .css_classes(["rgb-param-label"])
            .halign(gtk::Align::Start)
            .build();
        mode_row.append(&mode_lbl);
        mode_row.append(&mode_dropdown);
        root.append(&mode_row);

        let zone_dropdown_opt: Option<gtk::DropDown> = if descriptor.zones.len() > 1 {
            let zone_names: Vec<&str> =
                descriptor.zones.iter().map(|z| z.name.as_str()).collect();
            let zone_model = gtk::StringList::new(&zone_names);
            let zone_dropdown = gtk::DropDown::builder().model(&zone_model).build();

            let zone_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .build();
            let zone_lbl = gtk::Label::builder()
                .label("ZONE")
                .css_classes(["rgb-param-label"])
                .halign(gtk::Align::Start)
                .build();
            zone_row.append(&zone_lbl);
            zone_row.append(&zone_dropdown);
            root.append(&zone_row);
            Some(zone_dropdown)
        } else {
            None
        };

        // ── Per-zone LED-content transform controls ───────────────────────────
        // Rebuilt on zone change; never refreshed from daemon broadcasts.
        let transform_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();
        root.append(&transform_box);

        let rebuild_transform: Rc<dyn Fn()> = Rc::new({
            let transform_box = transform_box.clone();
            let state = state.clone();
            let descriptor = descriptor.clone();
            let device_id = device_id.clone();
            let store = store.clone();
            move || {
                populate_transform_controls(&transform_box, &state, &descriptor, &device_id, &store);
            }
        });
        rebuild_transform();

        let content_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(20)
            .build();
        root.append(&content_row);

        const CANVAS_PAD_I: i32 = 18;
        let (canvas_w_min, canvas_h_request) = match descriptor.zones.first().map(|z| &z.topology) {
            Some(ZoneTopology::Keyboard { .. }) => (680, 210),
            Some(ZoneTopology::Linear) => (400, 100),
            Some(ZoneTopology::Rings { count }) => {
                let h = 280i32;
                let w = (*count as i32 * (h - CANVAS_PAD_I * 2) + CANVAS_PAD_I * 2).max(280);
                (w, h)
            }
            _ => (280, 280),
        };

        let canvas = gtk::DrawingArea::builder()
            .width_request(canvas_w_min)
            .height_request(canvas_h_request)
            .hexpand(true)
            .css_classes(["rgb-led-canvas"])
            .build();

        let canvas_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .css_classes(["rgb-section-card"])
            .build();
        canvas_card.append(&canvas);
        content_row.append(&canvas_card);

        let right_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["rgb-section-card"])
            .build();
        let right = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        right_card.append(&right);
        content_row.append(&right_card);

        let apply_btn = gtk::Button::builder()
            .label("Apply")
            .css_classes(["suggested-action"])
            .halign(gtk::Align::End)
            .build();
        root.append(&apply_btn);

        if matches!(init_mode, Mode::Engine) {
            mode_dropdown.set_sensitive(false);
            apply_btn.set_sensitive(false);
        }

        // ── Wire: zone dropdown ───────────────────────────────────────────────
        if let Some(zone_dropdown) = zone_dropdown_opt {
            let state_z = state.clone();
            let canvas_z = canvas.clone();
            let rebuild_transform_z = rebuild_transform.clone();
            zone_dropdown.connect_selected_notify(move |dd| {
                let mut st = state_z.borrow_mut();
                st.selected_zone_idx = dd.selected() as usize;
                st.selected.clear();
                drop(st);
                rebuild_transform_z();
                canvas_z.queue_draw();
            });
        }

        // ── Wire: canvas draw ─────────────────────────────────────────────────
        {
            let state = state.clone();
            let descriptor = descriptor.clone();
            canvas.set_draw_func(move |_da, cr, w, h| {
                draw_leds(cr, w, h, &state.borrow(), &descriptor);
            });
        }

        // ── Wire: canvas drag — click toggles LED, drag rubber-bands ─────────
        {
            let drag = gtk::GestureDrag::new();

            {
                let state = state.clone();
                let canvas_b = canvas.clone();
                drag.connect_drag_begin(move |_, sx, sy| {
                    let mut st = state.borrow_mut();
                    if !matches!(st.mode, Mode::PerLed) { return; }
                    st.rubber_start = Some((sx, sy));
                    st.rubber_end = (sx, sy);
                    drop(st);
                    canvas_b.grab_focus();
                });
            }

            {
                let state = state.clone();
                let canvas_u = canvas.clone();
                drag.connect_drag_update(move |g, dx, dy| {
                    let mut st = state.borrow_mut();
                    if !matches!(st.mode, Mode::PerLed) { return; }
                    if let Some((sx, sy)) = g.start_point() {
                        st.rubber_end = (sx + dx, sy + dy);
                    }
                    drop(st);
                    canvas_u.queue_draw();
                });
            }

            {
                let state = state.clone();
                let descriptor = descriptor.clone();
                let canvas_e = canvas.clone();
                drag.connect_drag_end(move |g, dx, dy| {
                    let mut st = state.borrow_mut();
                    if !matches!(st.mode, Mode::PerLed) { return; }

                    let Some((sx, sy)) = g.start_point() else {
                        st.rubber_start = None;
                        return;
                    };
                    let ex = sx + dx;
                    let ey = sy + dy;
                    let dist = (dx * dx + dy * dy).sqrt();
                    let w = canvas_e.width() as f64;
                    let h = canvas_e.height() as f64;

                    let zone = descriptor.zones.get(st.selected_zone_idx);

                    if dist < 5.0 {
                        let mut hit = false;
                        if let Some(zone) = zone {
                            for led in &zone.leds {
                                let (lx, ly) = led_pos(led.x as f64, led.y as f64, w, h, &zone.topology);
                                if ((sx - lx).powi(2) + (sy - ly).powi(2)).sqrt() <= LED_R + 6.0 {
                                    if st.selected.contains(&led.id) {
                                        st.selected.remove(&led.id);
                                    } else {
                                        st.selected.insert(led.id);
                                    }
                                    hit = true;
                                    break;
                                }
                            }
                        }
                        if !hit {
                            st.selected.clear();
                        }
                    } else {
                        let min_x = sx.min(ex);
                        let max_x = sx.max(ex);
                        let min_y = sy.min(ey);
                        let max_y = sy.max(ey);
                        if let Some(zone) = zone {
                            for led in &zone.leds {
                                let (lx, ly) = led_pos(led.x as f64, led.y as f64, w, h, &zone.topology);
                                if lx >= min_x && lx <= max_x && ly >= min_y && ly <= max_y {
                                    st.selected.insert(led.id);
                                }
                            }
                        }
                    }

                    st.rubber_start = None;
                    drop(st);
                    canvas_e.queue_draw();
                });
            }

            canvas.add_controller(drag);
        }

        // ── Wire: keyboard — Ctrl+A selects all, Escape deselects ────────────
        {
            canvas.set_focusable(true);
            let state_key = state.clone();
            let descriptor_key = descriptor.clone();
            let canvas_key = canvas.clone();
            let key_ctrl = gtk::EventControllerKey::new();
            key_ctrl.connect_key_pressed(move |_, key, _, modifiers| {
                let mut st = state_key.borrow_mut();
                if !matches!(st.mode, Mode::PerLed) {
                    return gtk::glib::Propagation::Proceed;
                }
                use gtk::gdk::Key;
                if key == Key::Escape {
                    st.selected.clear();
                    drop(st);
                    canvas_key.queue_draw();
                    return gtk::glib::Propagation::Stop;
                }
                if key == Key::a && modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                    if let Some(zone) = descriptor_key.zones.get(st.selected_zone_idx) {
                        for led in &zone.leds {
                            st.selected.insert(led.id);
                        }
                    }
                    drop(st);
                    canvas_key.queue_draw();
                    return gtk::glib::Propagation::Stop;
                }
                gtk::glib::Propagation::Proceed
            });
            canvas.add_controller(key_ctrl);
        }

        // ── Wire: apply button ────────────────────────────────────────────────
        {
            let state = state.clone();
            let descriptor = descriptor.clone();
            let device_id = device_id.clone();
            let store = store.clone();
            apply_btn.connect_clicked(move |_| {
                let rgb_state = state.borrow().build_ipc_state(&descriptor);
                match serde_json::to_value(&rgb_state) {
                    Ok(state_json) => {
                        store.dispatch(crate::commands::Command::RgbApply {
                            device_id: (*device_id).clone(),
                            state: state_json,
                        });
                    }
                    Err(e) => log::error!("rgb_apply serialize: {e}"),
                }
            });
        }

        setup_right_panel(&right, &state, &descriptor, &canvas, mode_dropdown.clone(), mode_entries.clone());
        let _ = &right_card;

        // ── Host-mode reactivity ──────────────────────────────────────────────
        // Rebuilds the mode dropdown so native effects appear/disappear with
        // host mode, preserving the current selection where it still exists.
        let apply_host_mode: Box<dyn Fn(bool)> = {
            let mode_dropdown = mode_dropdown.clone();
            let mode_entries = mode_entries.clone();
            let descriptor = descriptor.clone();
            Box::new(move |host_mode: bool| {
                let cur_mode = {
                    let entries = mode_entries.borrow();
                    entries.get(mode_dropdown.selected() as usize).map(|(_, m)| m.clone())
                };
                let entries = build_mode_entries(&descriptor, host_mode);
                let new_idx = cur_mode
                    .as_ref()
                    .and_then(|cm| entries.iter().position(|(_, m)| mode_eq(m, cm)))
                    .unwrap_or(0) as u32;
                let model = mode_string_list(&entries);
                *mode_entries.borrow_mut() = entries;
                mode_dropdown.set_model(Some(&model));
                mode_dropdown.set_selected(new_idx);
            })
        };

        let chains_container = chains::build_chains_section(&root);
        chains::populate_chains_section(
            &chains_container,
            &device_id,
            &status.chainable_channels,
            store,
        );
        let initial_chains_sig = chains::chains_signature(&status.chainable_channels);

        let device_id_str = (*device_id).clone();
        Self {
            root,
            apply_host_mode,
            last_host_mode: RefCell::new(host_mode),
            chains_container,
            last_chains_sig: RefCell::new(initial_chains_sig),
            device_id: device_id_str,
            store: store.clone(),
        }
    }
}

// ── Right panel setup ───────────────────────────────────────────────────────────

fn setup_right_panel(
    right: &gtk::Box,
    state: &Rc<RefCell<WState>>,
    descriptor: &Rc<RgbDescriptor>,
    canvas: &gtk::DrawingArea,
    mode_dropdown: gtk::DropDown,
    mode_entries: Rc<RefCell<Vec<(String, Mode)>>>,
) {
    let panel = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .hexpand(true)
        .build();
    right.append(&panel);

    let rebuild: Rc<dyn Fn()> = Rc::new({
        let panel = panel.clone();
        let state = state.clone();
        let descriptor = descriptor.clone();
        let canvas = canvas.clone();
        move || {
            while let Some(child) = panel.first_child() {
                panel.remove(&child);
            }
            match state.borrow().mode.clone() {
                Mode::Static | Mode::PerLed => {
                    build_color_content(&panel, &state, &descriptor, &canvas);
                }
                Mode::Effect { id } => {
                    build_effect_content(&panel, &id, &descriptor, &state);
                }
                Mode::Engine => {
                    let lbl = gtk::Label::builder()
                        .label("This device is controlled by the RGB Canvas Engine.\nConfigure it in the Canvas tab.")
                        .css_classes(["dim-label"])
                        .halign(gtk::Align::Start)
                        .wrap(true)
                        .build();
                    panel.append(&lbl);
                }
            }
        }
    });

    rebuild();

    {
        let state = state.clone();
        let descriptor = descriptor.clone();
        let canvas = canvas.clone();
        let rebuild = Rc::clone(&rebuild);

        mode_dropdown.connect_selected_notify(move |dd| {
            let idx = dd.selected() as usize;
            let new_mode = match mode_entries.borrow().get(idx) {
                Some((_, m)) => m.clone(),
                None => return,
            };

            if let Mode::Effect { id } = &new_mode {
                let params_slice = descriptor
                    .native_effects
                    .iter()
                    .find(|e| &e.id == id)
                    .map(|e| e.params.as_slice())
                    .unwrap_or(&[]);
                let mut st = state.borrow_mut();
                st.effect_params.clear();
                for p in params_slice {
                    st.effect_params.insert(p.id.clone(), p.default.clone());
                }
            }

            state.borrow_mut().mode = new_mode;
            rebuild();
            canvas.queue_draw();
        });
    }
}

// ── Color panel ─────────────────────────────────────────────────────────────────

fn build_color_content(
    panel: &gtk::Box,
    state: &Rc<RefCell<WState>>,
    descriptor: &Rc<RgbDescriptor>,
    canvas: &gtk::DrawingArea,
) {
    let color_lbl = gtk::Label::builder()
        .label("COLOR")
        .css_classes(["rgb-param-label"])
        .halign(gtk::Align::Start)
        .build();
    panel.append(&color_lbl);

    // A plain ColorDialogButton only emits `rgba-notify` when the colour
    // actually changes — confirming the dialog with the unchanged colour
    // never fires, so selected LEDs would not get painted. Drive the dialog
    // manually instead: `choose_rgba`'s callback runs on every confirmation.
    let dialog = Rc::new({
        let d = gtk::ColorDialog::new();
        d.set_with_alpha(false);
        d
    });

    let swatch = gtk::DrawingArea::builder()
        .width_request(28)
        .height_request(28)
        .build();
    {
        let state = state.clone();
        swatch.set_draw_func(move |_, cr, w, h| {
            let c = state.borrow().color;
            cr.set_source_rgb(
                c.r as f64 / 255.0,
                c.g as f64 / 255.0,
                c.b as f64 / 255.0,
            );
            cr.rectangle(0.0, 0.0, w as f64, h as f64);
            let _ = cr.fill();
        });
    }

    let btn_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    btn_content.append(&swatch);
    btn_content.append(&gtk::Label::new(Some("Choose color\u{2026}")));

    let btn = gtk::Button::builder()
        .halign(gtk::Align::Start)
        .child(&btn_content)
        .build();
    {
        let state = state.clone();
        let descriptor = descriptor.clone();
        let canvas_c = canvas.clone();
        let dialog = dialog.clone();
        let swatch = swatch.clone();
        btn.connect_clicked(move |b| {
            let parent = b.root().and_downcast::<gtk::Window>();
            let initial = gdk_rgba(state.borrow().color);
            let state = state.clone();
            let descriptor = descriptor.clone();
            let canvas_c = canvas_c.clone();
            let swatch = swatch.clone();
            dialog.choose_rgba(
                parent.as_ref(),
                Some(&initial),
                gtk::gio::Cancellable::NONE,
                move |res| {
                    let Ok(rgba) = res else { return };
                    let color = RgbColor {
                        r: (rgba.red() * 255.0) as u8,
                        g: (rgba.green() * 255.0) as u8,
                        b: (rgba.blue() * 255.0) as u8,
                    };
                    let mut st = state.borrow_mut();
                    st.color = color;
                    if matches!(st.mode, Mode::PerLed) && !st.selected.is_empty() {
                        let selected: Vec<LedId> = st.selected.iter().copied().collect();
                        if let Some(zone) = descriptor.zones.get(st.selected_zone_idx) {
                            let map = st.zone_led_colors.entry(zone.id.clone()).or_default();
                            for id in selected {
                                map.insert(id, color);
                            }
                        }
                    }
                    drop(st);
                    swatch.queue_draw();
                    canvas_c.queue_draw();
                },
            );
        });
    }
    panel.append(&btn);

    if matches!(state.borrow().mode, Mode::PerLed) {
        let hint = gtk::Label::builder()
            .label("Click or drag to select LEDs · Ctrl+A selects all · Esc deselects")
            .css_classes(["dim-label", "caption"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .build();
        panel.append(&hint);
    }
}

// ── Per-zone transform controls ─────────────────────────────────────────────────

/// Rebuild the transform controls for the currently selected zone. Controls are
/// topology-dependent and send `rgb_set_zone_transform` immediately on change.
/// Called at build time and on every zone-dropdown change — never from a
/// daemon broadcast (the controls are user-controlled inputs).
fn populate_transform_controls(
    container: &gtk::Box,
    state: &Rc<RefCell<WState>>,
    descriptor: &Rc<RgbDescriptor>,
    device_id: &Rc<String>,
    store: &Store,
) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
    let idx = state.borrow().selected_zone_idx;
    let Some(zone) = descriptor.zones.get(idx) else {
        return;
    };
    let zone_id = zone.id.clone();
    let t = state
        .borrow()
        .zone_transforms
        .get(&zone_id)
        .copied()
        .unwrap_or_default();

    let lbl = gtk::Label::builder()
        .label("TRANSFORM")
        .css_classes(["rgb-param-label"])
        .halign(gtk::Align::Start)
        .build();
    container.append(&lbl);

    // Persist the mutated transform for this zone and push it to the daemon.
    let send = {
        let state = state.clone();
        let device_id = device_id.clone();
        let store = store.clone();
        let zone_id = zone_id.clone();
        move |mutate: &dyn Fn(&mut ZoneContentTransform)| {
            let updated = {
                let mut st = state.borrow_mut();
                let entry = st.zone_transforms.entry(zone_id.clone()).or_default();
                mutate(entry);
                *entry
            };
            store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                "type": "rgb_set_zone_transform",
                "id": device_id.as_str(),
                "zone_id": zone_id.as_str(),
                "flip_h": updated.flip_h,
                "flip_v": updated.flip_v,
                "reverse": updated.reverse,
                "led_offset": updated.led_offset,
            })));
        }
    };

    match zone.topology {
        // Ring topologies: reverse direction + cyclic LED-index offset.
        ZoneTopology::Ring | ZoneTopology::Rings { .. } => {
            let reverse = gtk::ToggleButton::builder()
                .label("Reverse")
                .active(t.reverse)
                .tooltip_text("Reverse LED direction within each ring")
                .build();
            {
                let send = send.clone();
                reverse.connect_toggled(move |b| {
                    let active = b.is_active();
                    send(&|t| t.reverse = active);
                });
            }
            let rotate_lbl = gtk::Label::builder()
                .label("Rotate")
                .css_classes(["dim-label"])
                .build();
            let adj = gtk::Adjustment::new(t.led_offset as f64, -999.0, 999.0, 1.0, 5.0, 0.0);
            let spin = gtk::SpinButton::builder()
                .adjustment(&adj)
                .digits(0)
                .tooltip_text("Cyclic LED-index offset applied to each ring")
                .build();
            {
                let send = send.clone();
                spin.connect_value_changed(move |s| {
                    let offset = s.value() as i32;
                    send(&|t| t.led_offset = offset);
                });
            }
            container.append(&reverse);
            container.append(&rotate_lbl);
            container.append(&spin);
        }
        // Non-ring topologies: geometric flip.
        ZoneTopology::Linear | ZoneTopology::Grid | ZoneTopology::Keyboard { .. } => {
            let flip_h = gtk::ToggleButton::builder()
                .label("Flip H")
                .active(t.flip_h)
                .tooltip_text("Mirror LED content horizontally")
                .build();
            {
                let send = send.clone();
                flip_h.connect_toggled(move |b| {
                    let active = b.is_active();
                    send(&|t| t.flip_h = active);
                });
            }
            let flip_v = gtk::ToggleButton::builder()
                .label("Flip V")
                .active(t.flip_v)
                .tooltip_text("Mirror LED content vertically")
                .build();
            {
                let send = send.clone();
                flip_v.connect_toggled(move |b| {
                    let active = b.is_active();
                    send(&|t| t.flip_v = active);
                });
            }
            container.append(&flip_h);
            container.append(&flip_v);
        }
    }
}

// ── CapabilityPanel impl ────────────────────────────────────────────────────────

use crate::ui::capability_registry::CapabilityPanel;
use crate::state::AppState;

impl CapabilityPanel for RgbWidget {
    fn root_widget(&self) -> gtk::Widget { self.root.clone().upcast() }
    fn tab_label(&self) -> &'static str  { "Lighting" }
    fn tab_icon(&self)  -> &'static str  { "rgb-strip-symbolic" }
    fn tab_name(&self)  -> &'static str  { "lighting" }

    fn update_live(&self, state: &AppState) {
        let Some(device) = state.devices.iter().find(|d| d.id == *self.device_id) else { return };
        // Only touches the (user-controlled) mode dropdown when host mode
        // actually flips — not on every 250 ms broadcast — so it never fights
        // the user's selection.
        let host_mode = effects_allowed(device);
        if host_mode != *self.last_host_mode.borrow() {
            *self.last_host_mode.borrow_mut() = host_mode;
            (self.apply_host_mode)(host_mode);
        }

        // Skip the rebuild on no-op broadcasts so a mid-edit inline Entry
        // isn't wiped — see `chains_signature` for the exclusion rules.
        let chainable = device
            .capabilities
            .iter()
            .find_map(|c| match c {
                DeviceCapability::Rgb(s) => Some(s.chainable_channels.as_slice()),
                _ => None,
            })
            .unwrap_or(&[]);
        let new_sig = chains::chains_signature(chainable);
        if new_sig != *self.last_chains_sig.borrow() {
            *self.last_chains_sig.borrow_mut() = new_sig;
            chains::populate_chains_section(
                &self.chains_container,
                &self.device_id,
                chainable,
                &self.store,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{Boolean, DeviceType, NativeEffect};

    fn descriptor_with_effects() -> RgbDescriptor {
        RgbDescriptor {
            zones: vec![],
            native_effects: vec![
                NativeEffect { id: "ripple".into(), name: "Ripple".into(), params: vec![] },
                NativeEffect { id: "color_wave".into(), name: "Color Wave".into(), params: vec![] },
            ],
        }
    }

    fn device_with_host_mode(value: Option<bool>) -> WireDevice {
        let capabilities = match value {
            Some(v) => vec![DeviceCapability::Boolean(vec![Boolean {
                key: "host_mode".into(),
                label: "Host Mode".into(),
                value: v,
                read_only: false,
                category: "Profiles".into(),
            }])],
            None => vec![],
        };
        WireDevice {
            id: "dev".into(),
            name: "dev".into(),
            vendor: "v".into(),
            model: "m".into(),
            device_type: DeviceType::Keyboard,
            connected: true,
            capabilities,
            connection_type: None,
            serial_number: None,
            ..Default::default()
        }
    }

    #[test]
    fn build_mode_entries_lists_effects_only_in_host_mode() {
        let desc = descriptor_with_effects();

        let off = build_mode_entries(&desc, false);
        assert_eq!(off.len(), 2, "only Static + Per LED when not in host mode");
        assert!(off.iter().all(|(_, m)| !matches!(m, Mode::Effect { .. })));

        let on = build_mode_entries(&desc, true);
        let effect_ids: Vec<&str> = on
            .iter()
            .filter_map(|(_, m)| match m {
                Mode::Effect { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(effect_ids, ["ripple", "color_wave"], "effects appended in host mode");
    }

    #[test]
    fn effects_allowed_tracks_host_mode_boolean() {
        assert!(effects_allowed(&device_with_host_mode(Some(true))));
        assert!(!effects_allowed(&device_with_host_mode(Some(false))));
        // No host_mode toggle (non-HID++ device) → effects always allowed.
        assert!(effects_allowed(&device_with_host_mode(None)));
    }

    #[test]
    fn mode_eq_matches_variant_and_effect_id() {
        assert!(mode_eq(&Mode::Static, &Mode::Static));
        assert!(mode_eq(
            &Mode::Effect { id: "ripple".into() },
            &Mode::Effect { id: "ripple".into() },
        ));
        assert!(!mode_eq(
            &Mode::Effect { id: "ripple".into() },
            &Mode::Effect { id: "color_wave".into() },
        ));
        assert!(!mode_eq(&Mode::Static, &Mode::PerLed));
    }
}
