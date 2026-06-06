//! "Chains" section of the RGB widget. Rebuilt from scratch on every state
//! broadcast whose structural signature changes — see
//! [`crate::ui::widgets::rgb::RgbWidget::update_live`].

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use serde_json::json;

use crate::store::Store;
use halod_protocol::types::{ChainLinkInfo, ChainableChannelInfo, ZoneTopology};

/// Excludes link `name` — the user may be mid-edit in an inline Entry, and a
/// 250 ms broadcast must not wipe their input.
pub fn chains_signature(channels: &[ChainableChannelInfo]) -> u64 {
    let mut h = DefaultHasher::new();
    channels.len().hash(&mut h);
    for ch in channels {
        ch.channel_id.hash(&mut h);
        ch.max_leds.hash(&mut h);
        ch.link_kind.hash(&mut h);
        ch.links.len().hash(&mut h);
        for link in &ch.links {
            link.child_device_id.hash(&mut h);
            link.led_count.hash(&mut h);
            link.locked.hash(&mut h);
            // ZoneTopology has no Hash impl; round-trip via JSON instead.
            if let Ok(s) = serde_json::to_string(&link.topology) {
                s.hash(&mut h);
            }
        }
    }
    h.finish()
}

pub struct ChainsContainer {
    pub header: gtk::Label,
    pub section: gtk::Box,
}

/// Containers start hidden; [`populate_chains_section`] reveals them when the
/// device has chainable channels.
pub fn build_chains_section(root: &gtk::Box) -> ChainsContainer {
    let header = gtk::Label::builder()
        .label("CHAINS")
        .css_classes(["rgb-param-label"])
        .halign(gtk::Align::Start)
        .visible(false)
        .build();
    root.append(&header);

    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .visible(false)
        .build();
    root.append(&section);

    ChainsContainer { header, section }
}

pub fn populate_chains_section(
    container: &ChainsContainer,
    device_id: &str,
    channels: &[ChainableChannelInfo],
    store: &Store,
) {
    while let Some(child) = container.section.first_child() {
        container.section.remove(&child);
    }

    if channels.is_empty() {
        container.header.set_visible(false);
        container.section.set_visible(false);
        return;
    }

    container.header.set_visible(true);
    container.section.set_visible(true);

    for channel in channels {
        container
            .section
            .append(&build_channel_card(device_id, channel, store));
    }
}

fn build_channel_card(
    device_id: &str,
    channel: &ChainableChannelInfo,
    store: &Store,
) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .css_classes(["rgb-section-card"])
        .build();

    // ── Header row: channel name + budget label + Add button ────────────────
    let header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();

    let title = gtk::Label::builder()
        .label(&channel.name)
        .halign(gtk::Align::Start)
        .hexpand(true)
        .css_classes(["heading"])
        .build();
    header_row.append(&title);

    let used: u32 = channel.links.iter().map(|l| l.led_count).sum();
    let budget = gtk::Label::builder()
        .label(&format!("{used} / {} LEDs", channel.max_leds))
        .css_classes(["dim-label"])
        .build();
    header_row.append(&budget);

    let detect_btn = gtk::Button::builder()
        .icon_name("find-location-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Flash red to locate devices on this channel")
        .build();
    {
        let store = store.clone();
        let device_id = device_id.to_string();
        let channel_id = channel.channel_id.clone();
        detect_btn.connect_clicked(move |_| {
            store.dispatch(crate::commands::Command::CanvasOp(json!({
                "type": "rgb_chain_detect_channel",
                "id": device_id,
                "channel_id": channel_id,
            })));
        });
    }
    header_row.append(&detect_btn);

    let add_btn = gtk::Button::builder()
        .label("Add link")
        .css_classes(["suggested-action"])
        .build();
    let remaining = channel.max_leds.saturating_sub(used);
    if remaining == 0 {
        add_btn.set_sensitive(false);
        add_btn.set_tooltip_text(Some("LED budget exhausted"));
    }
    {
        let store = store.clone();
        let device_id = device_id.to_string();
        let channel_id = channel.channel_id.clone();
        let max_leds = channel.max_leds;
        add_btn.connect_clicked(move |btn| {
            // libadwaita refuses to anchor the dialog without a transient
            // parent; the widget's nearest GTK window is that anchor.
            let root = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok());
            open_add_dialog(
                root.as_ref(),
                &store,
                &device_id,
                &channel_id,
                remaining.min(max_leds),
            );
        });
    }
    header_row.append(&add_btn);

    card.append(&header_row);

    // ── Link rows ───────────────────────────────────────────────────────────
    if channel.links.is_empty() {
        let empty = gtk::Label::builder()
            .label("No links on this chain yet. Use 'Add link' to attach an accessory.")
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .build();
        card.append(&empty);
    } else {
        for link in &channel.links {
            card.append(&build_link_row(device_id, &channel.channel_id, link, store));
        }
    }

    card
}

fn build_link_row(
    device_id: &str,
    channel_id: &str,
    link: &ChainLinkInfo,
    store: &Store,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();

    if link.locked {
        let lock = gtk::Image::builder()
            .icon_name("system-lock-screen-symbolic")
            .tooltip_text("Auto-detected accessory; manage in hardware")
            .build();
        row.append(&lock);
    }

    let name_entry = gtk::Entry::builder()
        .text(&link.name)
        .hexpand(true)
        .build();
    name_entry.set_sensitive(!link.locked);

    if !link.locked {
        let store = store.clone();
        let child_device_id = link.child_device_id.clone();
        let original = link.name.clone();
        name_entry.connect_activate(move |entry| {
            let new_name = entry.text().to_string();
            if new_name == original || new_name.is_empty() {
                return;
            }
            store.dispatch(crate::commands::Command::CanvasOp(json!({
                "type": "set_device_name",
                "device_id": child_device_id,
                "name": new_name,
            })));
        });
    }
    row.append(&name_entry);

    let topology_lbl = gtk::Label::builder()
        .label(&topology_label(&link.topology))
        .css_classes(["dim-label"])
        .build();
    row.append(&topology_lbl);

    let count_lbl = gtk::Label::builder()
        .label(&format!("{} LEDs", link.led_count))
        .css_classes(["dim-label"])
        .build();
    row.append(&count_lbl);

    if !link.locked {
        let remove_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["destructive-action", "flat"])
            .tooltip_text("Remove this chain link")
            .build();
        let store = store.clone();
        let device_id = device_id.to_string();
        let channel_id = channel_id.to_string();
        let child_device_id = link.child_device_id.clone();
        remove_btn.connect_clicked(move |_| {
            store.dispatch(crate::commands::Command::CanvasOp(json!({
                "type": "rgb_chain_remove_link",
                "id": device_id,
                "channel_id": channel_id,
                "child_device_id": child_device_id,
            })));
        });
        row.append(&remove_btn);
    }

    row
}

fn topology_label(t: &ZoneTopology) -> String {
    match t {
        ZoneTopology::Linear => "Linear".to_string(),
        ZoneTopology::Ring => "Ring".to_string(),
        ZoneTopology::Rings { count } => format!("Rings ×{count}"),
        ZoneTopology::Grid => "Grid".to_string(),
        ZoneTopology::Keyboard { .. } => "Keyboard".to_string(),
    }
}

/// Topology choices offered in the Add-link dialog. `divisor` is the LED-count
/// constraint: Rings ×N requires `led_count % N == 0`. The daemon enforces the
/// same rule (see `chain::validate_led_count`); doing it client-side just
/// gives instant feedback.
const TOPOLOGY_CHOICES: &[(&str, fn() -> ZoneTopology, u32)] = &[
    ("Linear", || ZoneTopology::Linear, 1),
    ("Ring", || ZoneTopology::Ring, 1),
    ("Rings ×2", || ZoneTopology::Rings { count: 2 }, 2),
    ("Rings ×3", || ZoneTopology::Rings { count: 3 }, 3),
    ("Rings ×4", || ZoneTopology::Rings { count: 4 }, 4),
    ("Grid", || ZoneTopology::Grid, 1),
];

fn topology_for_choice(idx: u32) -> ZoneTopology {
    TOPOLOGY_CHOICES
        .get(idx as usize)
        .map(|(_, ctor, _)| ctor())
        .unwrap_or(ZoneTopology::Linear)
}

fn divisor_for_choice(idx: u32) -> u32 {
    TOPOLOGY_CHOICES
        .get(idx as usize)
        .map(|(_, _, d)| *d)
        .unwrap_or(1)
}

fn open_add_dialog(
    transient_for: Option<&gtk::Window>,
    store: &Store,
    device_id: &str,
    channel_id: &str,
    max_remaining: u32,
) {
    let dialog = gtk::Window::builder()
        .title("Add chain link")
        .modal(true)
        .default_width(360)
        .build();
    if let Some(parent) = transient_for {
        dialog.set_transient_for(Some(parent));
    }

    let v = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    dialog.set_child(Some(&v));

    let name_entry = gtk::Entry::builder()
        .placeholder_text("Name (e.g. \"Top strip\")")
        .build();
    v.append(&name_entry);

    let topo_names: Vec<&str> = TOPOLOGY_CHOICES.iter().map(|(n, _, _)| *n).collect();
    let topo_model = gtk::StringList::new(&topo_names);
    let topo_dd = gtk::DropDown::builder().model(&topo_model).build();
    let topo_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    topo_row.append(&gtk::Label::new(Some("Topology:")));
    topo_row.append(&topo_dd);
    v.append(&topo_row);

    let count_adj = gtk::Adjustment::new(
        max_remaining.min(8) as f64,
        1.0,
        max_remaining as f64,
        1.0,
        10.0,
        0.0,
    );
    let count_spin = gtk::SpinButton::new(Some(&count_adj), 1.0, 0);
    let count_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    count_row.append(&gtk::Label::new(Some("LED count:")));
    count_row.append(&count_spin);
    v.append(&count_row);

    // Inline validation hint — reveals when the current topology/LED-count
    // combo would be rejected (Rings ×N requires count % N == 0).
    let hint = gtk::Label::builder()
        .label("")
        .css_classes(["error", "caption"])
        .halign(gtk::Align::Start)
        .visible(false)
        .build();
    v.append(&hint);

    let btn_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let add = gtk::Button::builder()
        .label("Add")
        .css_classes(["suggested-action"])
        .build();
    btn_row.append(&cancel);
    btn_row.append(&add);
    v.append(&btn_row);

    // ── Live validator ──────────────────────────────────────────────────────
    // Re-runs on topology change and on every LED-count step. Disables the
    // Add button + shows the hint when invalid. Single source of truth: the
    // same rule lives daemon-side in `chain::validate_led_count`.
    let revalidate: std::rc::Rc<dyn Fn()> = {
        let topo_dd = topo_dd.clone();
        let count_spin = count_spin.clone();
        let add = add.clone();
        let hint = hint.clone();
        std::rc::Rc::new(move || {
            let divisor = divisor_for_choice(topo_dd.selected());
            let count = count_spin.value_as_int() as u32;
            if count == 0 {
                add.set_sensitive(false);
                hint.set_label("LED count must be at least 1.");
                hint.set_visible(true);
                return;
            }
            if divisor > 1 && count % divisor != 0 {
                let down = (count / divisor) * divisor;
                let up = down + divisor;
                let suggestion = if down == 0 {
                    format!("{up}")
                } else {
                    format!("{down} or {up}")
                };
                add.set_sensitive(false);
                hint.set_label(&format!(
                    "Rings ×{divisor} needs a multiple of {divisor}; try {suggestion}.",
                ));
                hint.set_visible(true);
                return;
            }
            add.set_sensitive(true);
            hint.set_visible(false);
        })
    };
    revalidate();
    {
        let revalidate = revalidate.clone();
        topo_dd.connect_selected_notify(move |_| revalidate());
    }
    {
        let revalidate = revalidate.clone();
        count_spin.connect_value_changed(move |_| revalidate());
    }

    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| dialog.close());
    }

    {
        let dialog = dialog.clone();
        let store = store.clone();
        let device_id = device_id.to_string();
        let channel_id = channel_id.to_string();
        let name_entry = name_entry.clone();
        let topo_dd = topo_dd.clone();
        let count_spin = count_spin.clone();
        add.connect_clicked(move |_| {
            let name = name_entry.text().to_string();
            if name.is_empty() {
                name_entry.add_css_class("error");
                return;
            }
            let topology = topology_for_choice(topo_dd.selected());
            let topology_json = serde_json::to_value(&topology).unwrap_or(json!("linear"));
            let led_count = count_spin.value_as_int() as u32;
            store.dispatch(crate::commands::Command::CanvasOp(json!({
                "type": "rgb_chain_add_link",
                "id": device_id,
                "channel_id": channel_id,
                "name": name,
                "topology": topology_json,
                "led_count": led_count,
            })));
            dialog.close();
        });
    }

    dialog.present();
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{ChainLinkInfo, ChainableChannelInfo, ZoneTopology};

    fn empty_channel(id: &str) -> ChainableChannelInfo {
        ChainableChannelInfo {
            channel_id: id.into(),
            name: id.into(),
            max_leds: 60,
            link_kind: "generic".into(),
            links: vec![],
        }
    }

    fn link(id: &str, topology: ZoneTopology) -> ChainLinkInfo {
        ChainLinkInfo {
            child_device_id: id.into(),
            name: id.into(),
            topology,
            led_count: 12,
            locked: false,
        }
    }

    // ── chains_signature ─────────────────────────────────────────────────────

    #[test]
    fn chains_signature_stable_for_same_input() {
        let channels = vec![empty_channel("ch0")];
        assert_eq!(chains_signature(&channels), chains_signature(&channels));
    }

    #[test]
    fn chains_signature_differs_when_channel_count_changes() {
        let a = chains_signature(&[empty_channel("ch0")]);
        let b = chains_signature(&[empty_channel("ch0"), empty_channel("ch1")]);
        assert_ne!(a, b);
    }

    #[test]
    fn chains_signature_differs_when_link_topology_changes() {
        let mut ch = empty_channel("ch0");
        ch.links = vec![link("dev", ZoneTopology::Linear)];
        let a = chains_signature(&[ch.clone()]);
        ch.links[0].topology = ZoneTopology::Ring;
        let b = chains_signature(&[ch]);
        assert_ne!(a, b);
    }

    // ── topology_label ───────────────────────────────────────────────────────

    #[test]
    fn topology_label_all_variants() {
        assert_eq!(topology_label(&ZoneTopology::Linear), "Linear");
        assert_eq!(topology_label(&ZoneTopology::Ring),   "Ring");
        assert_eq!(topology_label(&ZoneTopology::Grid),   "Grid");
        assert_eq!(topology_label(&ZoneTopology::Rings { count: 3 }), "Rings ×3");
    }

    // ── topology_for_choice / divisor_for_choice ─────────────────────────────

    #[test]
    fn topology_for_choice_matches_table_order() {
        assert!(matches!(topology_for_choice(0), ZoneTopology::Linear));
        assert!(matches!(topology_for_choice(1), ZoneTopology::Ring));
        assert!(matches!(topology_for_choice(2), ZoneTopology::Rings { count: 2 }));
        assert!(matches!(topology_for_choice(3), ZoneTopology::Rings { count: 3 }));
        assert!(matches!(topology_for_choice(4), ZoneTopology::Rings { count: 4 }));
        assert!(matches!(topology_for_choice(5), ZoneTopology::Grid));
    }

    #[test]
    fn topology_for_choice_out_of_bounds_returns_linear() {
        assert!(matches!(topology_for_choice(99), ZoneTopology::Linear));
    }

    #[test]
    fn divisor_for_choice_matches_rings_count() {
        assert_eq!(divisor_for_choice(0), 1); // Linear
        assert_eq!(divisor_for_choice(2), 2); // Rings ×2
        assert_eq!(divisor_for_choice(3), 3); // Rings ×3
        assert_eq!(divisor_for_choice(4), 4); // Rings ×4
    }

    #[test]
    fn divisor_for_choice_out_of_bounds_returns_1() {
        assert_eq!(divisor_for_choice(99), 1);
    }
}
