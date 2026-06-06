use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gtk4::{self as gtk, prelude::*};

use crate::store::Store;
use crate::state::AppState;
use halod_protocol::types::{DeviceCapability, RgbColor, RgbState, VisibilityState};

fn rgba_to_rgb(rgba: &gtk::gdk::RGBA) -> RgbColor {
    RgbColor {
        r: (rgba.red() * 255.0).round() as u8,
        g: (rgba.green() * 255.0).round() as u8,
        b: (rgba.blue() * 255.0).round() as u8,
    }
}

fn fallback_led_color(state: &Option<RgbState>, zone_id: &str, led_id: u32) -> RgbColor {
    match state {
        Some(RgbState::Static { color }) => *color,
        Some(RgbState::PerLed { zones }) => zones
            .get(zone_id)
            .and_then(|m| m.get(&led_id.to_string()))
            .copied()
            .unwrap_or(RgbColor { r: 0, g: 0, b: 0 }),
        _ => RgbColor { r: 0, g: 0, b: 0 },
    }
}

fn count_targeted(selected: &HashMap<String, HashSet<String>>) -> usize {
    selected.values().filter(|z| !z.is_empty()).count()
}

fn apply_label(n: usize) -> String {
    if n == 1 {
        "Apply to 1 device".to_string()
    } else {
        format!("Apply to {} devices", n)
    }
}

#[derive(Clone)]
pub struct LightingPage {
    pub root: gtk::Box,
}

impl LightingPage {
    pub fn new(ctx: &Store) -> Self {
        let selected: Rc<RefCell<HashMap<String, HashSet<String>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let all_zones: Rc<RefCell<HashMap<String, Vec<String>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let device_ids: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let zone_btns: Rc<RefCell<HashMap<String, HashMap<String, gtk::ToggleButton>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let device_cbs: Rc<RefCell<HashMap<String, gtk::ToggleButton>>> =
            Rc::new(RefCell::new(HashMap::new()));

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(16)
            .margin_end(16)
            .spacing(12)
            .build();

        // ── Color picker ──────────────────────────────────────────────────────────
        let color_dialog = gtk::ColorDialog::builder().with_alpha(false).build();
        let color_btn = gtk::ColorDialogButton::builder()
            .dialog(&color_dialog)
            .rgba(&gtk::gdk::RGBA::new(1.0, 0.0, 0.0, 1.0))
            .halign(gtk::Align::Center)
            .build();
        root.append(&color_btn);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // ── Devices header ────────────────────────────────────────────────────────
        let devices_lbl = gtk::Label::builder()
            .label("Devices & Zones")
            .css_classes(["heading"])
            .halign(gtk::Align::Start)
            .hexpand(true)
            .build();
        let toggle_all_btn = gtk::Button::builder()
            .label("toggle all")
            .css_classes(["flat"])
            .build();
        let devices_header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        devices_header.append(&devices_lbl);
        devices_header.append(&toggle_all_btn);
        root.append(&devices_header);

        // ── Device list ───────────────────────────────────────────────────────────
        let device_list_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&device_list_box)
            .build();
        root.append(&scroll);

        // ── Apply button ──────────────────────────────────────────────────────────
        let apply_btn = gtk::Button::builder()
            .label("Apply to 0 devices")
            .css_classes(["suggested-action"])
            .halign(gtk::Align::End)
            .sensitive(false)
            .build();
        root.append(&apply_btn);

        // ── Toggle all ────────────────────────────────────────────────────────────
        {
            let selected = selected.clone();
            let all_zones = all_zones.clone();
            let zone_btns = zone_btns.clone();
            let device_cbs = device_cbs.clone();
            let apply_btn = apply_btn.clone();
            toggle_all_btn.connect_clicked(move |_| {
                let any_on = selected.borrow().values().any(|z| !z.is_empty());
                {
                    let mut sel = selected.borrow_mut();
                    let az = all_zones.borrow();
                    if any_on {
                        for z in sel.values_mut() {
                            z.clear();
                        }
                    } else {
                        for (dev_id, zones) in az.iter() {
                            sel.entry(dev_id.clone())
                                .or_default()
                                .extend(zones.iter().cloned());
                        }
                    }
                }
                let sel = selected.borrow();
                let zbtns = zone_btns.borrow();
                let dcbs = device_cbs.borrow();
                for dev_id in all_zones.borrow().keys() {
                    let sel_zones = sel.get(dev_id).cloned().unwrap_or_default();
                    if let Some(dev_btns) = zbtns.get(dev_id) {
                        for (zid, zbtn) in dev_btns {
                            zbtn.set_active(sel_zones.contains(zid));
                        }
                    }
                    if let Some(cb) = dcbs.get(dev_id) {
                        cb.set_active(!sel_zones.is_empty());
                    }
                }
                drop(dcbs);
                drop(zbtns);
                drop(sel);
                let n = count_targeted(&selected.borrow());
                apply_btn.set_label(&apply_label(n));
                apply_btn.set_sensitive(n > 0);
            });
        }

        // ── Apply ─────────────────────────────────────────────────────────────────
        {
            let selected = selected.clone();
            let color_btn = color_btn.clone();
            let store_apply = ctx.clone();
            apply_btn.connect_clicked(move |_| {
                let sel = selected.borrow();
                let chosen = rgba_to_rgb(&color_btn.rgba());
                let state = store_apply.state();

                for dev in &state.devices {
                    let sel_zones = match sel.get(&dev.id) {
                        Some(z) if !z.is_empty() => z,
                        _ => continue,
                    };
                    let rgb_status = dev.capabilities.iter().find_map(|cap| {
                        if let DeviceCapability::Rgb(s) = cap { Some(s) } else { None }
                    });
                    let rgb_status = match rgb_status {
                        Some(s) => s,
                        None => continue,
                    };

                    let all_zone_ids: HashSet<String> = rgb_status
                        .descriptor
                        .zones
                        .iter()
                        .map(|z| z.id.clone())
                        .collect();

                    let rgb_state = if sel_zones == &all_zone_ids {
                        RgbState::Static { color: chosen }
                    } else {
                        let mut zones_map: HashMap<String, HashMap<String, RgbColor>> =
                            HashMap::new();
                        for zone in &rgb_status.descriptor.zones {
                            let led_map: HashMap<String, RgbColor> = zone
                                .leds
                                .iter()
                                .map(|led| {
                                    let c = if sel_zones.contains(&zone.id) {
                                        chosen
                                    } else {
                                        fallback_led_color(
                                            &rgb_status.state,
                                            &zone.id,
                                            led.id,
                                        )
                                    };
                                    (led.id.to_string(), c)
                                })
                                .collect();
                            zones_map.insert(zone.id.clone(), led_map);
                        }
                        RgbState::PerLed { zones: zones_map }
                    };

                    match serde_json::to_value(&rgb_state) {
                        Ok(state_json) => store_apply.dispatch(crate::commands::Command::RgbApply {
                            device_id: dev.id.clone(),
                            state: state_json,
                        }),
                        Err(e) => log::error!("rgb_apply serialize: {e}"),
                    }
                }
            });
        }

        // ── State subscription ────────────────────────────────────────────────────
        {
            let selected = selected.clone();
            let all_zones = all_zones.clone();
            let device_ids = device_ids.clone();
            let zone_btns = zone_btns.clone();
            let device_cbs = device_cbs.clone();
            let apply_btn = apply_btn.clone();
            let device_list_box = device_list_box.clone();
            ctx.subscribe(
                |state| crate::store::sel_hash(&state.devices),
                move |state| {
                    rebuild_if_changed(
                        state,
                        &device_list_box,
                        &selected,
                        &all_zones,
                        &device_ids,
                        &zone_btns,
                        &device_cbs,
                        &apply_btn,
                    );
                },
            );
        }

        LightingPage { root }
    }
}

fn rebuild_if_changed(
    state: &AppState,
    device_list_box: &gtk::Box,
    selected: &Rc<RefCell<HashMap<String, HashSet<String>>>>,
    all_zones: &Rc<RefCell<HashMap<String, Vec<String>>>>,
    device_ids: &Rc<RefCell<Vec<String>>>,
    zone_btns: &Rc<RefCell<HashMap<String, HashMap<String, gtk::ToggleButton>>>>,
    device_cbs: &Rc<RefCell<HashMap<String, gtk::ToggleButton>>>,
    apply_btn: &gtk::Button,
) {
    let rgb_devs: Vec<_> = state
        .devices
        .iter()
        .filter(|dev| dev.active_state == VisibilityState::Visible)
        .filter_map(|dev| {
            dev.capabilities.iter().find_map(|cap| {
                if let DeviceCapability::Rgb(s) = cap {
                    if !s.descriptor.zones.is_empty() {
                        Some((dev, s))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
        })
        .collect();

    let new_ids: Vec<String> = rgb_devs.iter().map(|(d, _)| d.id.clone()).collect();
    if *device_ids.borrow() == new_ids {
        return;
    }
    *device_ids.borrow_mut() = new_ids.clone();

    {
        let mut sel = selected.borrow_mut();
        let mut az = all_zones.borrow_mut();
        for (dev, status) in &rgb_devs {
            let zids: Vec<String> =
                status.descriptor.zones.iter().map(|z| z.id.clone()).collect();
            az.insert(dev.id.clone(), zids.clone());
            sel.entry(dev.id.clone())
                .or_insert_with(|| zids.into_iter().collect());
        }
        sel.retain(|id, _| new_ids.contains(id));
        az.retain(|id, _| new_ids.contains(id));
    }

    while let Some(child) = device_list_box.first_child() {
        device_list_box.remove(&child);
    }
    zone_btns.borrow_mut().clear();
    device_cbs.borrow_mut().clear();

    for (dev, status) in &rgb_devs {
        let device_id = dev.id.clone();

        let frame = gtk::Frame::builder().css_classes(["card"]).build();
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(10)
            .margin_end(10)
            .build();
        frame.set_child(Some(&inner));

        let header_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        let sel_initial = selected.borrow().get(&device_id)
            .map(|z| !z.is_empty()).unwrap_or(true);
        let device_cb = gtk::ToggleButton::builder()
            .label(&dev.name)
            .active(sel_initial)
            .css_classes(["flat", "zone-device-toggle"])
            .hexpand(true)
            .halign(gtk::Align::Start)
            .build();
        header_row.append(&device_cb);
        inner.append(&header_row);

        let zones_flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .row_spacing(4)
            .column_spacing(4)
            .build();

        let sel_zones = selected
            .borrow()
            .get(&device_id)
            .cloned()
            .unwrap_or_default();

        let mut dev_zone_btns: HashMap<String, gtk::ToggleButton> = HashMap::new();

        for zone in &status.descriptor.zones {
            let zone_id = zone.id.clone();
            let zone_btn = gtk::ToggleButton::builder()
                .label(&zone.name)
                .active(sel_zones.contains(&zone_id))
                .css_classes(["zone-chip"])
                .build();

            {
                let selected = selected.clone();
                let device_cbs = device_cbs.clone();
                let apply_btn = apply_btn.clone();
                let device_id = device_id.clone();
                let zone_id = zone_id.clone();
                zone_btn.connect_toggled(move |btn| {
                    let active = btn.is_active();
                    let already = selected.borrow().get(&device_id)
                        .map(|z| z.contains(&zone_id)).unwrap_or(false);
                    if already == active { return; }
                    {
                        let mut sel = selected.borrow_mut();
                        let zones = sel.entry(device_id.clone()).or_default();
                        if active {
                            zones.insert(zone_id.clone());
                        } else {
                            zones.remove(&zone_id);
                        }
                    }
                    let zone_count =
                        selected.borrow().get(&device_id).map(|z| z.len()).unwrap_or(0);
                    if let Some(cb) = device_cbs.borrow().get(&device_id) {
                        cb.set_active(zone_count > 0);
                    }
                    let n = count_targeted(&selected.borrow());
                    apply_btn.set_label(&apply_label(n));
                    apply_btn.set_sensitive(n > 0);
                });
            }

            zones_flow.append(&zone_btn);
            dev_zone_btns.insert(zone_id, zone_btn);
        }

        inner.append(&zones_flow);
        device_list_box.append(&frame);

        zone_btns
            .borrow_mut()
            .insert(device_id.clone(), dev_zone_btns);
        device_cbs
            .borrow_mut()
            .insert(device_id.clone(), device_cb.clone());

        {
            let selected = selected.clone();
            let all_zones = all_zones.clone();
            let zone_btns = zone_btns.clone();
            let apply_btn = apply_btn.clone();
            let device_id = device_id.clone();
            device_cb.connect_toggled(move |cb| {
                let active = cb.is_active();
                let already = selected.borrow().get(&device_id)
                    .map(|z| !z.is_empty()).unwrap_or(false);
                if already == active { return; }
                {
                    let mut sel = selected.borrow_mut();
                    let az = all_zones.borrow();
                    let zones = sel.entry(device_id.clone()).or_default();
                    if active {
                        if let Some(all) = az.get(&device_id) {
                            for z in all {
                                zones.insert(z.clone());
                            }
                        }
                    } else {
                        zones.clear();
                    }
                }
                let dev_sel_zones = selected
                    .borrow()
                    .get(&device_id)
                    .cloned()
                    .unwrap_or_default();
                if let Some(dev_btns) = zone_btns.borrow().get(&device_id) {
                    for (zid, zbtn) in dev_btns {
                        zbtn.set_active(dev_sel_zones.contains(zid));
                    }
                }
                let n = count_targeted(&selected.borrow());
                apply_btn.set_label(&apply_label(n));
                apply_btn.set_sensitive(n > 0);
            });
        }
    }

    let n = count_targeted(&selected.borrow());
    apply_btn.set_label(&apply_label(n));
    apply_btn.set_sensitive(n > 0);
}
