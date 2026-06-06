use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use super::device::icon_for_device_type;
use crate::state::AppState;
use crate::store::{NavTarget, Store};
use halod_protocol::types::{
    BatteryStatus, ConnectionType, DeviceCapability, DeviceType, DiscoveryPhase, FanCurveStatus,
    Sensor, SensorUnit, VisibilityState, WireDevice, WireFanCurve,
};

/// One discovered device drawn as a scatter label around the radar. The angle
/// and radius are derived deterministically from the device id, so a device's
/// position stays stable across redraws and across rescans.
struct DiscoveryBlip {
    angle: f64,
    radius_t: f64,
}

struct HomeCardStats {
    headline_lbl: gtk::Label,
    battery_lbl: Option<gtk::Label>,
    battery_charging_icon: Option<gtk::Image>,
    fan_lbl: Option<gtk::Label>,
    pump_lbl: Option<gtk::Label>,
    connection_type_lbl: Option<gtk::Label>,
    /// Warning icon shown when the device's fan curve has a non-Ok status.
    /// Present only for fan and pump devices; `None` for all others.
    curve_warning_icon: Option<gtk::Image>,
}

#[derive(Clone)]
pub struct HomePage {
    pub root: gtk::Stack,
    pub search_entry: gtk::SearchEntry,
    // Discovery transitional screen — full-bleed radar: concentric rings
    // pulse outward, the live device count sits at the center, and each newly
    // found device drops in as a scatter label at a stable random position
    // around the rings.
    discovery_count_lbl: gtk::Label,
    discovery_eyebrow_lbl: gtk::Label,
    discovery_phase_lbl: gtk::Label,
    discovery_dot: gtk::Box,
    discovery_ring: gtk::DrawingArea,
    discovery_blips: Rc<RefCell<HashMap<String, DiscoveryBlip>>>,
    discovery_blip_order: Rc<RefCell<Vec<String>>>,
    blip_icons_fixed: gtk::Fixed,
    blip_icon_widgets: Rc<RefCell<HashMap<String, gtk::Image>>>,
    home_content: gtk::Box,
    // Sensor section (lives at top of home view, updated on every broadcast)
    sensor_section: gtk::Box,
    sensor_flow: gtk::FlowBox,
    // sensor id → (value label, chip widget), updated in place
    sensor_labels: Rc<RefCell<HashMap<String, (gtk::Label, gtk::Box)>>>,
    // device id → live-updatable stat labels on home cards
    card_stats: Rc<RefCell<HashMap<String, HomeCardStats>>>,
    // Hotplug tracking: which device IDs are currently shown
    known_device_ids: Rc<RefCell<HashSet<String>>>,
    // type-label → (section gtk::Box, inner gtk::FlowBox)
    section_flows: Rc<RefCell<HashMap<&'static str, (gtk::Box, gtk::FlowBox)>>>,
    // device_id → (flow it lives in, FlowBoxChild wrapper) for surgical removal
    card_widgets: Rc<RefCell<HashMap<String, (gtk::FlowBox, gtk::Widget)>>>,
    // device_id → (lowercased searchable text, section key) for live filtering
    card_meta: Rc<RefCell<HashMap<String, (String, &'static str)>>>,
    // current lowercased search query
    search_query: Rc<RefCell<String>>,
    // whether to show hidden devices and sensors
    show_hidden: Rc<Cell<bool>>,
    pub show_hidden_btn: gtk::ToggleButton,
    store: Store,
}

impl HomePage {
    pub fn new(ctx: &Store) -> Self {
        let root = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(250)
            .build();

        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search devices…")
            .hexpand(true)
            .build();

        // --- Discovery screen ---
        // Full-bleed radar: a DrawingArea fills the entire view, painting
        // concentric rings pulsing outward plus a scatter of device-name
        // labels around the perimeter. A GTK Overlay layers the live count
        // and eyebrow over the center, and the phase status line over the
        // bottom. No separate device list — devices ARE the radar contacts.
        let discovery_ring = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();

        let discovery_count_lbl = gtk::Label::builder()
            .label("0")
            .halign(gtk::Align::Center)
            .css_classes(["discovery-count", "scanning"])
            .build();

        let discovery_eyebrow_lbl = gtk::Label::builder()
            .label("DEVICES FOUND")
            .halign(gtk::Align::Center)
            .css_classes(["discovery-eyebrow", "scanning"])
            .build();

        let hero_center = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .can_target(false)
            .build();
        hero_center.append(&discovery_count_lbl);
        hero_center.append(&discovery_eyebrow_lbl);

        let discovery_dot = gtk::Box::builder()
            .width_request(8)
            .height_request(8)
            .valign(gtk::Align::Center)
            .css_classes(["discovery-dot", "scanning"])
            .build();

        let discovery_phase_lbl = gtk::Label::builder()
            .label("Scanning…")
            .css_classes(["discovery-phase", "scanning"])
            .build();

        let phase_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(56)
            .can_target(false)
            .build();
        phase_row.append(&discovery_dot);
        phase_row.append(&discovery_phase_lbl);

        let blip_icons_fixed = gtk::Fixed::builder()
            .hexpand(true)
            .vexpand(true)
            .can_target(false)
            .build();

        let discovery_overlay = gtk::Overlay::new();
        discovery_overlay.set_child(Some(&discovery_ring));
        discovery_overlay.add_overlay(&hero_center);
        discovery_overlay.add_overlay(&phase_row);
        discovery_overlay.add_overlay(&blip_icons_fixed);

        let discovery_blips: Rc<RefCell<HashMap<String, DiscoveryBlip>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let discovery_blip_order: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let blip_icon_widgets: Rc<RefCell<HashMap<String, gtk::Image>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // Drive the radar animation off a shared start instant; the draw_func
        // reads the blip table on every frame so newly found devices appear
        // immediately at their pseudo-random scatter positions.
        let start = Instant::now();
        discovery_ring.set_draw_func(move |_, cr, w, h| {
            draw_discovery_radar(cr, w as f64, h as f64, start);
        });
        discovery_ring.add_tick_callback(|area, _| {
            area.queue_draw();
            gtk::glib::ControlFlow::Continue
        });

        // Reposition icon widgets whenever the drawing area is resized.
        {
            let blips_resize = discovery_blips.clone();
            let icons_resize = blip_icon_widgets.clone();
            let fixed_resize = blip_icons_fixed.clone();
            discovery_ring.connect_resize(move |_, w, h| {
                let blips = blips_resize.borrow();
                let icons = icons_resize.borrow();
                for (id, img) in icons.iter() {
                    if let Some(blip) = blips.get(id) {
                        let (px, py) = blip_icon_px(blip, w as f64, h as f64);
                        fixed_resize.move_(img, px - 8.0, py - 8.0);
                    }
                }
            });
        }

        root.add_named(&discovery_overlay, Some("discovery"));

        // --- Home view ---
        let home_content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(32)
            .margin_start(32)
            .margin_end(32)
            .margin_top(28)
            .margin_bottom(32)
            .build();

        // Sensor section — prepended to home_content so it's always at the top.
        let sensor_flow = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(10)
            .row_spacing(10)
            .max_children_per_line(12)
            .build();

        let sensor_heading = gtk::Label::builder()
            .label("SENSORS")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .css_classes(["sensors-section-label"])
            .build();

        let show_hidden_btn = gtk::ToggleButton::builder()
            .icon_name("view-conceal-symbolic")
            .css_classes(["flat", "circular", "show-hidden-btn"])
            .tooltip_text("Show hidden devices and sensors")
            .build();

        let sensor_header_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .valign(gtk::Align::Center)
            .build();
        sensor_header_row.append(&sensor_heading);
        sensor_header_row.append(&show_hidden_btn);

        let sensor_section = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .visible(false)
            .build();
        sensor_section.append(&sensor_header_row);
        sensor_section.append(&sensor_flow);
        home_content.append(&sensor_section);

        let home_scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&home_content)
            .build();

        root.add_named(&home_scroll, Some("home"));
        root.set_visible_child_name("discovery");

        let page = HomePage {
            root,
            search_entry,
            discovery_count_lbl,
            discovery_eyebrow_lbl,
            discovery_phase_lbl,
            discovery_dot,
            discovery_ring,
            discovery_blips,
            discovery_blip_order,
            blip_icons_fixed,
            blip_icon_widgets,
            home_content,
            sensor_section,
            sensor_flow,
            sensor_labels: Rc::new(RefCell::new(HashMap::new())),
            card_stats: Rc::new(RefCell::new(HashMap::new())),
            known_device_ids: Rc::new(RefCell::new(HashSet::new())),
            section_flows: Rc::new(RefCell::new(HashMap::new())),
            card_widgets: Rc::new(RefCell::new(HashMap::new())),
            card_meta: Rc::new(RefCell::new(HashMap::new())),
            search_query: Rc::new(RefCell::new(String::new())),
            show_hidden: Rc::new(Cell::new(false)),
            show_hidden_btn,
            store: ctx.clone(),
        };

        // Filter device cards live as the user types in the header search bar.
        {
            let p = page.clone();
            page.search_entry.connect_search_changed(move |e| {
                *p.search_query.borrow_mut() = e.text().to_lowercase();
                p.apply_filter();
            });
        }

        // Toggle hidden items on/off.
        {
            let p = page.clone();
            page.show_hidden_btn.connect_toggled(move |btn| {
                p.show_hidden.set(btn.is_active());
                let devices = p.store.state().devices.clone();
                p.refresh_sensors(&devices);
                p.apply_filter();
            });
        }

        page
    }

    pub fn update(&self, state: &AppState) {
        let d = &state.discovery;

        match d.phase {
            DiscoveryPhase::Complete => {
                self.sync_home_cards(&state.devices);
                self.refresh_sensors(&state.devices);
                self.refresh_home_cards(&state.devices, &state.fan_curves);
                self.apply_filter();
                self.root.set_visible_child_name("home");
            }
            _ => {
                self.root.set_visible_child_name("discovery");
            }
        }

        // Hero: count, eyebrow, phase. The count animates live as devices are
        // discovered; the eyebrow flips between SCANNING / READY / FAILED and
        // the phase line carries the daemon's status message.
        let count = state.devices.len();
        self.discovery_count_lbl.set_text(&match d.phase {
            DiscoveryPhase::Error => "!".to_string(),
            _ => count.to_string(),
        });

        let (eyebrow_text, state_class, phase_text) = match d.phase {
            DiscoveryPhase::Error => ("DISCOVERY FAILED", "error", "Check daemon logs".to_string()),
            DiscoveryPhase::Complete => (
                match count {
                    0 => "NO DEVICES",
                    1 => "DEVICE READY",
                    _ => "DEVICES READY",
                },
                "ready",
                "Here we go!".to_string(),
            ),
            _ => (
                match count {
                    1 => "DEVICE FOUND",
                    _ => "DEVICES FOUND",
                },
                "scanning",
                "Scanning USB · SMBus · Under the bed".to_string(),
            ),
        };

        self.discovery_eyebrow_lbl.set_text(eyebrow_text);
        self.discovery_phase_lbl.set_text(&phase_text);
        for c in ["scanning", "ready", "error"] {
            self.discovery_count_lbl.remove_css_class(c);
            self.discovery_eyebrow_lbl.remove_css_class(c);
            self.discovery_phase_lbl.remove_css_class(c);
            self.discovery_dot.remove_css_class(c);
        }
        self.discovery_count_lbl.add_css_class(state_class);
        self.discovery_eyebrow_lbl.add_css_class(state_class);
        self.discovery_phase_lbl.add_css_class(state_class);
        self.discovery_dot.add_css_class(state_class);

        // Sync blips: any device we haven't seen yet gets a stable position
        // hashed from its id and joins the scatter. Positions never change
        // for an existing device.
        {
            let mut blips = self.discovery_blips.borrow_mut();
            let mut order = self.discovery_blip_order.borrow_mut();
            let mut icons = self.blip_icon_widgets.borrow_mut();
            let w = self.discovery_ring.width() as f64;
            let h = self.discovery_ring.height() as f64;
            for device in &state.devices {
                if !blips.contains_key(&device.id) {
                    let (angle, radius_t) = blip_position(&device.id);
                    let blip = DiscoveryBlip { angle, radius_t };
                    let (px, py) = blip_icon_px(&blip, w, h);
                    blips.insert(device.id.clone(), blip);
                    order.push(device.id.clone());

                    let img = gtk::Image::from_icon_name(
                        icon_for_device_type(&device.device_type),
                    );
                    img.set_pixel_size(16);
                    img.add_css_class("radar-blip-icon");
                    self.blip_icons_fixed.put(&img, px - 8.0, py - 8.0);
                    icons.insert(device.id.clone(), img);
                }
            }
        }
    }

    /// Diffs the current device list against what's shown and surgically adds/removes
    /// cards. Does not touch scroll position or rebuild any existing sections.
    ///
    /// Only currently-connected devices are listed: a wireless peripheral that
    /// is powered off drops out, and powering it back on re-adds a fresh card
    /// built from its now-populated capabilities.
    fn sync_home_cards(&self, devices: &[WireDevice]) {
        let current_ids: HashSet<String> = devices
            .iter()
            .filter(|d| device_is_listable(d))
            .map(|d| d.id.clone())
            .collect();

        let mut known = self.known_device_ids.borrow_mut();
        if *known == current_ids {
            return;
        }

        // Remove cards for devices no longer present
        let removed: Vec<String> = known.difference(&current_ids).cloned().collect();
        if !removed.is_empty() {
            let mut widgets = self.card_widgets.borrow_mut();
            let mut stats = self.card_stats.borrow_mut();
            let mut meta = self.card_meta.borrow_mut();
            for id in &removed {
                if let Some((flow, child)) = widgets.remove(id) {
                    flow.remove(&child);
                }
                stats.remove(id);
                meta.remove(id);
            }
        }

        // Devices to add
        let to_add: Vec<&WireDevice> = devices
            .iter()
            .filter(|d| device_is_listable(d) && !known.contains(&d.id))
            .collect();

        if !to_add.is_empty() {
            // Create any section boxes that don't exist yet.
            // Done in a separate pass so we don't hold section_flows borrowed while
            // calling home_content.append().
            let mut new_keys: Vec<&'static str> = {
                let existing = self.section_flows.borrow();
                let mut seen = HashSet::new();
                to_add
                    .iter()
                    .map(|d| label_for_type(&d.device_type))
                    .filter(|k| !existing.contains_key(k) && seen.insert(*k))
                    .collect()
            };
            // Process in alphabetical order so simultaneous additions land correctly.
            new_keys.sort_by_key(|k| {
                SECTION_ORDER.iter().position(|&o| o == *k).unwrap_or(usize::MAX)
            });

            for key in new_keys {
                let section = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(12)
                    .build();
                let heading = gtk::Label::builder()
                    .label(key)
                    .halign(gtk::Align::Start)
                    .css_classes(["home-section-label"])
                    .build();
                let flow = gtk::FlowBox::builder()
                    .selection_mode(gtk::SelectionMode::None)
                    .homogeneous(false)
                    .column_spacing(12)
                    .row_spacing(12)
                    .max_children_per_line(8)
                    .build();
                section.append(&heading);
                section.append(&flow);

                // Insert at the correct alphabetical position relative to existing sections.
                let key_pos = SECTION_ORDER.iter().position(|&o| o == key).unwrap_or(usize::MAX);
                let sibling: Option<gtk::Box> = {
                    let flows = self.section_flows.borrow();
                    SECTION_ORDER[..key_pos.min(SECTION_ORDER.len())]
                        .iter()
                        .rev()
                        .find_map(|&k| flows.get(k).map(|(s, _)| s.clone()))
                };
                match sibling {
                    Some(ref prev) => self.home_content.insert_child_after(&section, Some(prev)),
                    None => self.home_content.insert_child_after(&section, Some(&self.sensor_section)),
                }

                self.section_flows.borrow_mut().insert(key, (section, flow));
            }

            // Now insert cards
            for device in &to_add {
                let key = label_for_type(&device.device_type);
                let flow = self
                    .section_flows
                    .borrow()
                    .get(key)
                    .map(|(_, f)| f.clone())
                    .expect("section must exist at this point");

                let (card, stats) = make_home_card(device);
                self.card_stats
                    .borrow_mut()
                    .insert(device.id.clone(), stats);
                let haystack = format!("{} {}", device.name, device.vendor).to_lowercase();
                self.card_meta
                    .borrow_mut()
                    .insert(device.id.clone(), (haystack, key));

                // Left-click: navigate to device page.
                let gesture = gtk::GestureClick::new();
                let device_id = device.id.clone();
                let ctx_nav = self.store.clone();
                gesture.connect_released(move |_, _, _, _| {
                    ctx_nav.navigate(NavTarget::Device(device_id.clone()));
                });
                card.add_controller(gesture);

                // Right-click: context menu with state-appropriate actions.
                // Buttons are pre-built; shown/hidden based on the device's current
                // active_state at the moment the user right-clicks.
                let ctx_ctx = self.store.clone();
                let device_id_rc = device.id.clone();
                let popover = gtk::Popover::new();
                let menu_box = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .build();

                let make_vis_btn = |label: &str| {
                    gtk::Button::builder()
                        .label(label)
                        .css_classes(["flat"])
                        .build()
                };

                let rename_btn = make_vis_btn("Rename Device");
                let show_btn = make_vis_btn("Show Device");
                let hide_btn = make_vis_btn("Hide Device");
                let disable_btn = make_vis_btn("Disable Device");
                let enable_btn = make_vis_btn("Enable Device");

                menu_box.append(&rename_btn);
                menu_box.append(&show_btn);
                menu_box.append(&hide_btn);
                menu_box.append(&disable_btn);
                menu_box.append(&enable_btn);
                popover.set_child(Some(&menu_box));
                popover.set_parent(&card);

                let wire_menu_btn = |btn: &gtk::Button,
                                     new_state: VisibilityState,
                                     store: Store,
                                     id: String,
                                     pop: gtk::Popover| {
                    btn.connect_clicked(move |_| {
                        pop.popdown();
                        store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
                            "type": "set_device_visibility",
                            "device_id": id,
                            "state": new_state,
                        })));
                    });
                };
                wire_menu_btn(
                    &show_btn,
                    VisibilityState::Visible,
                    ctx_ctx.clone(),
                    device_id_rc.clone(),
                    popover.clone(),
                );
                wire_menu_btn(
                    &hide_btn,
                    VisibilityState::Hidden,
                    ctx_ctx.clone(),
                    device_id_rc.clone(),
                    popover.clone(),
                );
                wire_menu_btn(
                    &disable_btn,
                    VisibilityState::Disabled,
                    ctx_ctx.clone(),
                    device_id_rc.clone(),
                    popover.clone(),
                );
                wire_menu_btn(
                    &enable_btn,
                    VisibilityState::Visible,
                    ctx_ctx.clone(),
                    device_id_rc.clone(),
                    popover.clone(),
                );

                {
                    let store = ctx_ctx.clone();
                    let id = device_id_rc.clone();
                    let pop = popover.clone();
                    let card_for_rename = card.clone();
                    rename_btn.connect_clicked(move |_| {
                        pop.popdown();
                        let current_name = store
                            .state()
                            .devices
                            .iter()
                            .find(|d| d.id == id)
                            .map(|d| d.name.clone())
                            .unwrap_or_default();
                        let parent = card_for_rename.root().and_downcast::<gtk::Window>();
                        crate::ui::widgets::rename_dialog::open_rename_dialog(
                            parent.as_ref(),
                            &store,
                            &id,
                            &current_name,
                        );
                    });
                }

                let right_click = gtk::GestureClick::new();
                right_click.set_button(3);
                right_click.connect_released(move |_, _, x, y| {
                    let state = ctx_ctx.state();
                    let active_state = state
                        .devices
                        .iter()
                        .find(|d| d.id == device_id_rc)
                        .map(|d| d.active_state.clone())
                        .unwrap_or_default();
                    drop(state);
                    let is_visible = active_state == VisibilityState::Visible;
                    let is_hidden = active_state == VisibilityState::Hidden;
                    let is_disabled = active_state == VisibilityState::Disabled;
                    show_btn.set_visible(is_hidden);
                    hide_btn.set_visible(is_visible);
                    disable_btn.set_visible(is_visible || is_hidden);
                    enable_btn.set_visible(is_disabled);
                    let rect = gdk4::Rectangle::new(x as i32, y as i32, 0, 0);
                    popover.set_pointing_to(Some(&rect));
                    popover.popup();
                });
                card.add_controller(right_click);

                flow.insert(&card, -1);

                // card.parent() is the FlowBoxChild wrapper GTK inserts automatically
                if let Some(flow_child) = card.parent() {
                    self.card_widgets
                        .borrow_mut()
                        .insert(device.id.clone(), (flow.clone(), flow_child));
                }
            }
        }

        // Hide sections that lost all their cards; show ones that gained any
        for (_, (section, flow)) in self.section_flows.borrow().iter() {
            section.set_visible(flow.first_child().is_some());
        }

        *known = current_ids;
    }

    /// Shows/hides device cards according to the current search query. A card
    /// matches if its name or vendor contains the query; a section is hidden
    /// when none of its cards match.
    fn apply_filter(&self) {
        let query = self.search_query.borrow().clone();
        let show_hidden = self.show_hidden.get();

        let vis_map: HashMap<String, VisibilityState> = self
            .store
            .state()
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.active_state.clone()))
            .collect();

        let meta = self.card_meta.borrow();
        let widgets = self.card_widgets.borrow();
        let mut section_has_match: HashMap<&'static str, bool> = HashMap::new();

        for (id, (_flow, child)) in widgets.iter() {
            let Some((haystack, key)) = meta.get(id) else {
                continue;
            };
            let active_state = vis_map.get(id).cloned().unwrap_or_default();
            let is_hidden = active_state == VisibilityState::Hidden;
            let is_disabled = active_state == VisibilityState::Disabled;
            let needs_show_hidden = is_hidden || is_disabled;
            let query_matches = query.is_empty() || haystack.contains(query.as_str());
            let visible = query_matches && (!needs_show_hidden || show_hidden);
            child.set_visible(visible);
            if visible {
                *section_has_match.entry(*key).or_insert(false) = true;
                if let Some(card) = child.first_child() {
                    if is_disabled {
                        card.add_css_class("device-disabled");
                        card.remove_css_class("device-hidden");
                    } else if is_hidden {
                        card.add_css_class("device-hidden");
                        card.remove_css_class("device-disabled");
                    } else {
                        card.remove_css_class("device-hidden");
                        card.remove_css_class("device-disabled");
                    }
                }
            }
        }
        drop(widgets);
        drop(meta);

        for (key, (section, _flow)) in self.section_flows.borrow().iter() {
            section.set_visible(*section_has_match.get(key).unwrap_or(&false));
        }
    }

    fn refresh_sensors(&self, devices: &[WireDevice]) {
        let show_hidden = self.show_hidden.get();
        let all: Vec<(&str, &Sensor)> = devices
            .iter()
            .flat_map(|d| {
                d.capabilities
                    .iter()
                    .filter_map(move |c| match c {
                        DeviceCapability::Sensors(ss) => {
                            Some(ss.iter().map(move |s| (d.name.as_str(), s)))
                        }
                        _ => None,
                    })
                    .flatten()
            })
            .filter(|(_, s)| s.visibility == VisibilityState::Visible || show_hidden)
            .collect();

        // Compare only by sensor ID so normal value updates never trigger a rebuild.
        // A rebuild only occurs when sensors are added or removed, keeping any open
        // right-click popover alive across value-only updates.
        let incoming_ids: HashSet<&str> = all.iter().map(|(_, s)| s.id.as_str()).collect();
        let mut labels = self.sensor_labels.borrow_mut();
        let existing_ids: HashSet<&str> = labels.keys().map(|k| k.as_str()).collect();

        if incoming_ids != existing_ids {
            // Sensor set changed — rebuild chips
            while let Some(child) = self.sensor_flow.first_child() {
                self.sensor_flow.remove(&child);
            }
            labels.clear();
            for (source, sensor) in &all {
                let is_hidden = sensor.visibility == VisibilityState::Hidden;
                let (chip, value_label) = make_sensor_chip(sensor, source, is_hidden);
                add_sensor_right_click(&chip, sensor.id.clone(), is_hidden, self.store.clone());
                self.sensor_flow.insert(&chip, -1);
                labels.insert(sensor.id.clone(), (value_label, chip));
            }
        } else {
            // Same set — update value, color class, and hidden CSS in place
            for (_, sensor) in &all {
                if let Some((lbl, chip)) = labels.get(&sensor.id) {
                    lbl.set_text(&format!("{:.1}", sensor.value));
                    let temp_class = if sensor.value >= 80.0 {
                        "temp-hot"
                    } else if sensor.value >= 60.0 {
                        "temp-warm"
                    } else {
                        "temp-cool"
                    };
                    lbl.set_css_classes(&["sensor-chip-value", temp_class]);
                    for c in ["temp-cool", "temp-warm", "temp-hot"] {
                        chip.remove_css_class(c);
                    }
                    chip.add_css_class(temp_class);
                    let is_hidden = sensor.visibility == VisibilityState::Hidden;
                    if is_hidden {
                        chip.add_css_class("sensor-hidden");
                    } else {
                        chip.remove_css_class("sensor-hidden");
                    }
                }
            }
        }

        self.sensor_section.set_visible(!all.is_empty());
    }

    fn refresh_home_cards(&self, devices: &[WireDevice], fan_curves: &[WireFanCurve]) {
        let stats = self.card_stats.borrow();
        let mut meta = self.card_meta.borrow_mut();
        for device in devices {
            let Some(s) = stats.get(&device.id) else {
                continue;
            };
            if s.headline_lbl.text().as_str() != device.name {
                s.headline_lbl.set_text(&device.name);
                if let Some(entry) = meta.get_mut(&device.id) {
                    entry.0 = format!("{} {}", device.name, device.vendor).to_lowercase();
                }
            }
            for cap in &device.capabilities {
                match cap {
                    DeviceCapability::Battery(batteries) => {
                        if let (Some(lbl), Some(b)) = (&s.battery_lbl, batteries.first()) {
                            lbl.set_text(&format!("{}%", b.level));
                            if let Some(icon) = &s.battery_charging_icon {
                                icon.set_visible(b.status == BatteryStatus::Charging);
                            }
                        }
                    }
                    DeviceCapability::Fan(fan) => {
                        if let Some(lbl) = &s.fan_lbl {
                            lbl.set_text(&format!("{} rpm", fan.rpm));
                        }
                    }
                    DeviceCapability::Pump(pump) => {
                        if let Some(lbl) = &s.pump_lbl {
                            lbl.set_text(&format!("{} rpm", pump.rpm));
                        }
                    }
                    _ => {}
                }
            }
            if let (Some(lbl), Some(ct)) = (&s.connection_type_lbl, &device.connection_type) {
                lbl.set_text(match ct {
                    ConnectionType::Wired => "Wired",
                    ConnectionType::Wireless => "Wireless",
                });
            }
            if let Some(icon) = &s.curve_warning_icon {
                let has_warning = fan_curves
                    .iter()
                    .any(|c| c.fan_id == device.id && c.status != FanCurveStatus::Ok);
                icon.set_visible(has_warning);
            }
        }
    }
}

fn add_sensor_right_click(chip: &gtk::Box, sensor_id: String, is_hidden: bool, store: Store) {
    let popover = gtk::Popover::new();
    let menu_btn = gtk::Button::builder()
        .label(if is_hidden {
            "Show Sensor"
        } else {
            "Hide Sensor"
        })
        .css_classes(["flat"])
        .build();
    let menu_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    menu_box.append(&menu_btn);
    popover.set_child(Some(&menu_box));
    popover.set_parent(chip);
    let new_state = if is_hidden {
        VisibilityState::Visible
    } else {
        VisibilityState::Hidden
    };
    let popover_btn = popover.clone();
    menu_btn.connect_clicked(move |_| {
        popover_btn.popdown();
        store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
            "type": "set_sensor_visibility",
            "sensor_id": sensor_id,
            "state": new_state,
        })));
    });
    let right_click = gtk::GestureClick::new();
    right_click.set_button(3);
    right_click.connect_released(move |_, _, x, y| {
        let rect = gdk4::Rectangle::new(x as i32, y as i32, 0, 0);
        popover.set_pointing_to(Some(&rect));
        popover.popup();
    });
    chip.add_controller(right_click);
}

fn make_sensor_chip(sensor: &Sensor, source: &str, is_hidden: bool) -> (gtk::Box, gtk::Label) {
    let temp_class = if sensor.value >= 80.0 {
        "temp-hot"
    } else if sensor.value >= 60.0 {
        "temp-warm"
    } else {
        "temp-cool"
    };

    let mut css_classes = vec!["sensor-chip", temp_class];
    if is_hidden {
        css_classes.push("sensor-hidden");
    }
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .css_classes(css_classes)
        .build();

    // Eyebrow: source device, tracked-caps — mirrors the home device card pattern.
    let source_label = gtk::Label::builder()
        .label(source.to_uppercase())
        .halign(gtk::Align::Start)
        .css_classes(["sensor-chip-source"])
        .build();
    card.append(&source_label);

    // Hero: value + unit on the same baseline.
    let value_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .valign(gtk::Align::Baseline)
        .build();

    let value_label = gtk::Label::builder()
        .label(format!("{:.1}", sensor.value))
        .css_classes(["sensor-chip-value", temp_class])
        .build();

    let unit_str = match sensor.unit {
        SensorUnit::Celsius => "°C",
        SensorUnit::Fahrenheit => "°F",
    };
    let unit_label = gtk::Label::builder()
        .label(unit_str)
        .css_classes(["sensor-chip-unit"])
        .valign(gtk::Align::Baseline)
        .build();

    value_row.append(&value_label);
    value_row.append(&unit_label);
    card.append(&value_row);

    // Footer: sensor name.
    let name_label = gtk::Label::builder()
        .label(&sensor.name)
        .halign(gtk::Align::Start)
        .css_classes(["sensor-chip-label"])
        .build();
    card.append(&name_label);

    (card, value_label)
}

/// Derive a stable scatter position for a device from its id, returning
/// `(angle in radians, radius_t in [0, 1])`. Two devices with the same id
/// always get the same slot, so labels don't jitter across redraws.
fn blip_position(id: &str) -> (f64, f64) {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    let seed = h.finish();
    // Keep the bottom arc (7 o'clock → 6 o'clock → 5 o'clock) free so blips
    // don't collide with the phase-row text at the bottom of the radar.
    // In Cairo coordinates (y-axis down), 6 o'clock = π/2, so the forbidden
    // arc runs from π/3 (5 o'clock) to 2π/3 (7 o'clock).  Map the raw uniform
    // value onto the remaining 300° by splitting at the gap:
    //   raw ∈ [0, π/3)  → kept as-is
    //   raw ∈ [π/3, 5π/3) → shifted up by π/3, landing in (2π/3, 2π)
    const FORBIDDEN_START: f64 = std::f64::consts::FRAC_PI_3; // π/3  (5 o'clock)
    const AVAILABLE: f64 = std::f64::consts::TAU - std::f64::consts::FRAC_PI_3; // 5π/3
    let raw = (seed & 0xFFFF) as f64 / 65536.0 * AVAILABLE;
    let angle = if raw < FORBIDDEN_START {
        raw
    } else {
        raw + FORBIDDEN_START
    };
    let radius_t = ((seed >> 16) & 0xFFFF) as f64 / 65536.0;
    (angle, radius_t)
}

/// Compute the pixel center of a blip's icon slot given the canvas dimensions.
/// Shared between the draw function (for label placement) and the resize handler
/// (for repositioning GTK icon widgets).
fn blip_icon_px(blip: &DiscoveryBlip, w: f64, h: f64) -> (f64, f64) {
    let cx = w / 2.0;
    let cy = h / 2.0;
    let max_r = (w.min(h) * 0.32).max(120.0);
    let half = w.min(h) / 2.0;
    let label_min_r = max_r + 20.0;
    let label_max_r = (half - 24.0).max(label_min_r + 60.0);
    let r = label_min_r + blip.radius_t * (label_max_r - label_min_r);
    // Guard against the widget not yet having a real allocation (w/h == 0).
    let px = if w > 160.0 {
        (cx + blip.angle.cos() * r).clamp(80.0, w - 80.0)
    } else {
        -1000.0
    };
    let py = if h > 100.0 {
        (cy + blip.angle.sin() * r).clamp(50.0, h - 50.0)
    } else {
        -1000.0
    };
    (px, py)
}

/// Render the discovery radar: concentric pulsing rings + scatter labels for
/// each discovered device. The ring layer is purely decorative; the scatter
/// labels are the device list, with each name placed at a stable random
/// offset from the rings' inner radius.
fn draw_discovery_radar(cr: &cairo::Context, w: f64, h: f64, start: Instant) {
    let cx = w / 2.0;
    let cy = h / 2.0;
    let elapsed = start.elapsed().as_secs_f64();

    // Rings — staggered phase so the pulse reads as continuous outward motion.
    const N: usize = 4;
    const PERIOD: f64 = 2.8;
    let min_r = 30.0;
    let max_r = (w.min(h) * 0.32).max(120.0);

    cr.set_source_rgba(0.21, 0.52, 0.89, 0.16);
    cr.set_line_width(1.0);
    cr.arc(cx, cy, min_r - 6.0, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();

    for i in 0..N {
        let phase = (elapsed + (i as f64) * (PERIOD / N as f64)) % PERIOD;
        let t = phase / PERIOD;
        let r = min_r + (max_r - min_r) * t;
        let alpha = (1.0 - t).powf(1.6) * 0.55;
        cr.set_source_rgba(0.21, 0.52, 0.89, alpha);
        cr.set_line_width(1.4);
        cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
    }

}

fn make_home_card(device: &WireDevice) -> (gtk::Box, HomeCardStats) {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .css_classes(["home-device-card"])
        .build();

    // Eyebrow: VENDOR · TYPE, tracked-caps.
    let eyebrow = gtk::Label::builder()
        .label(&format!(
            "{} · {}",
            device.vendor.to_uppercase(),
            singular_type_label(&device.device_type),
        ))
        .halign(gtk::Align::Start)
        .css_classes(["home-card-eyebrow"])
        .build();
    card.append(&eyebrow);

    // Headline: device name — the visual anchor of the card. Live-updated on
    // every broadcast so a rename reflects immediately without rebuilding.
    let headline_lbl = gtk::Label::builder()
        .label(device.name.as_str())
        .halign(gtk::Align::Start)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["home-card-headline"])
        .build();
    card.append(&headline_lbl);

    // Meta row: stats joined with " · " separators, trailing footer icon.
    let meta_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .valign(gtk::Align::Center)
        .css_classes(["home-card-meta-row"])
        .build();

    let stats_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(0)
        .hexpand(true)
        .build();

    let mut battery_lbl: Option<gtk::Label> = None;
    let mut battery_charging_icon: Option<gtk::Image> = None;
    let mut fan_lbl: Option<gtk::Label> = None;
    let mut pump_lbl: Option<gtk::Label> = None;
    let mut connection_type_lbl: Option<gtk::Label> = None;
    let mut first_stat = true;

    for cap in &device.capabilities {
        match cap {
            DeviceCapability::Battery(batteries) => {
                if let Some(b) = batteries.first() {
                    let (lbl, icon) =
                        append_battery_stat(&stats_box, b, &mut first_stat);
                    battery_lbl = Some(lbl);
                    battery_charging_icon = Some(icon);
                }
            }
            DeviceCapability::Fan(fan) => {
                fan_lbl = Some(append_meta_stat(
                    &stats_box,
                    &format!("{} rpm", fan.rpm),
                    &mut first_stat,
                ));
            }
            DeviceCapability::Pump(pump) => {
                pump_lbl = Some(append_meta_stat(
                    &stats_box,
                    &format!("{} rpm", pump.rpm),
                    &mut first_stat,
                ));
            }
            DeviceCapability::Children(children) => {
                append_meta_stat(
                    &stats_box,
                    &format!("{} ch", children.len()),
                    &mut first_stat,
                );
            }
            _ => {}
        }
    }

    if let Some(ct) = &device.connection_type {
        let text = match ct {
            ConnectionType::Wired => "Wired",
            ConnectionType::Wireless => "Wireless",
        };
        connection_type_lbl = Some(append_meta_stat(&stats_box, text, &mut first_stat));
    }

    meta_row.append(&stats_box);

    // Warning icon — visible when the device has a non-Ok fan curve status.
    // Only created for fan and pump devices.
    let has_fan_curve = device
        .capabilities
        .iter()
        .any(|c| matches!(c, DeviceCapability::Fan(_) | DeviceCapability::Pump(_)));
    let curve_warning_icon = if has_fan_curve {
        let icon = gtk::Image::builder()
            .icon_name("dialog-warning-symbolic")
            .pixel_size(14)
            .valign(gtk::Align::Center)
            .visible(false)
            .css_classes(["home-card-curve-warning"])
            .build();
        meta_row.append(&icon);
        Some(icon)
    } else {
        None
    };

    // Footer mark: small dim device icon, right-aligned.
    let mark = gtk::Image::builder()
        .icon_name(icon_for_device_type(&device.device_type))
        .pixel_size(16)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .css_classes(["home-card-mark"])
        .build();
    meta_row.append(&mark);

    card.append(&meta_row);

    (
        card,
        HomeCardStats {
            headline_lbl,
            battery_lbl,
            battery_charging_icon,
            fan_lbl,
            pump_lbl,
            connection_type_lbl,
            curve_warning_icon,
        },
    )
}

/// Appends a stat label to the meta row, prefixing a "·" separator after the first item.
/// Returns the value label so the caller can update its text on live broadcasts.
fn append_meta_stat(row: &gtk::Box, text: &str, first: &mut bool) -> gtk::Label {
    if !*first {
        let sep = gtk::Label::builder()
            .label("·")
            .css_classes(["home-card-meta-sep"])
            .build();
        row.append(&sep);
    }
    *first = false;
    let lbl = gtk::Label::builder()
        .label(text)
        .css_classes(["home-card-meta"])
        .build();
    row.append(&lbl);
    lbl
}

/// Appends a battery stat (charging icon + percentage label) to the meta row.
/// Returns the percentage label and the charging icon so both can be updated on live broadcasts.
fn append_battery_stat(
    row: &gtk::Box,
    battery: &halod_protocol::types::Battery,
    first: &mut bool,
) -> (gtk::Label, gtk::Image) {
    if !*first {
        let sep = gtk::Label::builder()
            .label("·")
            .css_classes(["home-card-meta-sep"])
            .build();
        row.append(&sep);
    }
    *first = false;

    let stat_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(3)
        .build();

    let icon = gtk::Image::builder()
        .icon_name("battery-good-charging-symbolic")
        .pixel_size(14)
        .valign(gtk::Align::Center)
        .visible(battery.status == BatteryStatus::Charging)
        .css_classes(["home-card-meta"])
        .build();
    stat_box.append(&icon);

    let lbl = gtk::Label::builder()
        .label(&format!("{}%", battery.level))
        .css_classes(["home-card-meta"])
        .build();
    stat_box.append(&lbl);
    row.append(&stat_box);

    (lbl, icon)
}

fn singular_type_label(t: &DeviceType) -> &'static str {
    match t {
        DeviceType::AIO => "AIO",
        DeviceType::Fan => "FAN",
        DeviceType::Hub => "CONTROLLER",
        DeviceType::Keyboard => "KEYBOARD",
        DeviceType::Mouse => "MOUSE",
        DeviceType::Headset => "HEADSET",
        DeviceType::Monitor => "MONITOR",
        DeviceType::Gpu => "GPU",
        DeviceType::LedStrip => "LED STRIP",
        DeviceType::Motherboard => "MOTHERBOARD",
        DeviceType::Ram => "RAM",
        DeviceType::Sensor => "SENSOR",
        DeviceType::Speaker => "SPEAKER",
        DeviceType::Other => "DEVICE",
    }
}

/// Whether a device belongs in the home device list: it must be currently
/// connected (a powered-off wireless peripheral drops out until it returns)
/// and not a bare sensor source (those render as chips, not cards).
fn device_is_listable(d: &WireDevice) -> bool {
    d.connected && !matches!(d.device_type, DeviceType::Sensor)
}

const SECTION_ORDER: &[&str] = &[
    "AIOs",
    "Controllers",
    "Fans",
    "GPUs",
    "Headsets",
    "Keyboards",
    "LED Strips",
    "Mice",
    "Monitors",
    "Motherboards",
    "Other",
    "RAM",
    "Speakers",
];

fn label_for_type(t: &DeviceType) -> &'static str {
    match t {
        DeviceType::AIO => "AIOs",
        DeviceType::Fan => "Fans",
        DeviceType::Gpu => "GPUs",
        DeviceType::Hub => "Controllers",
        DeviceType::Keyboard => "Keyboards",
        DeviceType::LedStrip => "LED Strips",
        DeviceType::Mouse => "Mice",
        DeviceType::Headset => "Headsets",
        DeviceType::Monitor => "Monitors",
        DeviceType::Motherboard => "Motherboards",
        DeviceType::Ram => "RAM",
        DeviceType::Sensor => "Sensors",
        DeviceType::Speaker => "Speakers",
        DeviceType::Other => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{DeviceType, WireDevice};

    // ── blip_position ─────────────────────────────────────────────────────────

    #[test]
    fn blip_position_is_deterministic() {
        assert_eq!(blip_position("dev-123"), blip_position("dev-123"));
    }

    #[test]
    fn blip_position_differs_for_different_ids() {
        assert_ne!(blip_position("mouse"), blip_position("keyboard"));
    }

    #[test]
    fn blip_position_angle_in_valid_range() {
        let (angle, _) = blip_position("any-device");
        assert!(angle >= 0.0 && angle < std::f64::consts::TAU);
    }

    #[test]
    fn blip_position_radius_t_in_unit_range() {
        let (_, r) = blip_position("any-device");
        assert!(r >= 0.0 && r <= 1.0);
    }

    // ── singular_type_label ───────────────────────────────────────────────────

    #[test]
    fn singular_type_label_spot_checks() {
        assert_eq!(singular_type_label(&DeviceType::Mouse), "MOUSE");
        assert_eq!(singular_type_label(&DeviceType::Keyboard), "KEYBOARD");
        assert_eq!(singular_type_label(&DeviceType::Hub), "CONTROLLER");
    }

    #[test]
    fn singular_type_label_every_variant_non_empty() {
        for t in [
            DeviceType::AIO,
            DeviceType::Fan,
            DeviceType::Hub,
            DeviceType::Keyboard,
            DeviceType::Mouse,
            DeviceType::Headset,
            DeviceType::Monitor,
            DeviceType::Sensor,
            DeviceType::Speaker,
            DeviceType::Other,
        ] {
            assert!(!singular_type_label(&t).is_empty(), "empty label for {t:?}");
        }
    }

    // ── label_for_type ────────────────────────────────────────────────────────

    #[test]
    fn label_for_type_spot_checks() {
        assert_eq!(label_for_type(&DeviceType::Mouse), "Mice");
        assert_eq!(label_for_type(&DeviceType::Keyboard), "Keyboards");
        assert_eq!(label_for_type(&DeviceType::Hub), "Controllers");
    }

    // ── device_is_listable ────────────────────────────────────────────────────

    fn make_device(connected: bool, device_type: DeviceType) -> WireDevice {
        WireDevice {
            id: "d".into(),
            name: "d".into(),
            connected,
            device_type,
            ..Default::default()
        }
    }

    #[test]
    fn device_is_listable_connected_non_sensor_is_true() {
        assert!(device_is_listable(&make_device(true, DeviceType::Mouse)));
        assert!(device_is_listable(&make_device(true, DeviceType::Keyboard)));
        assert!(device_is_listable(&make_device(true, DeviceType::Hub)));
    }

    #[test]
    fn device_is_listable_disconnected_is_false() {
        assert!(!device_is_listable(&make_device(false, DeviceType::Mouse)));
    }

    #[test]
    fn device_is_listable_sensor_type_is_false_even_if_connected() {
        assert!(!device_is_listable(&make_device(true, DeviceType::Sensor)));
    }
}
