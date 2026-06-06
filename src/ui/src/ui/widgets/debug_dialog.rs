//! Debug dialogs for the device page and the settings page.
//!
//! Both render the same `DebugInfo` payload (returned by the daemon's
//! `get_debug_info` command). The device-page dialog filters to a single device;
//! the settings dialog shows everything.
//!
//! The widgets are intentionally read-only. They register a one-shot callback
//! through `Store::request_debug_info`; the snapshot is captured at the
//! moment of the request and never auto-refreshes.

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use halod_protocol::debug_info::{
    DebugInfo, DeviceDebugInfo, HidEntryDebugInfo, SmbusBusDebugInfo, SystemDebugInfo,
};

use crate::store::Store;

/// Open the per-device debug dialog. The dialog opens immediately with a
/// "Loading…" placeholder; the actual content is populated once the next
/// `debug_info` response arrives.
pub fn open_device_debug_dialog(parent: Option<&gtk::Window>, store: &Store, device_id: &str) {
    let window = adw::Window::builder()
        .modal(true)
        .title(&format!("Debug — {device_id}"))
        .default_width(560)
        .default_height(620)
        .build();
    if let Some(p) = parent {
        window.set_transient_for(Some(p));
    }

    let header = adw::HeaderBar::new();
    let close_btn = gtk::Button::with_label("Close");
    header.pack_start(&close_btn);

    let copy_btn = gtk::Button::with_label("Copy JSON");
    copy_btn.add_css_class("flat");
    header.pack_end(&copy_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    body.append(&loading_label());
    scroll.set_child(Some(&body));
    toolbar.set_content(Some(&scroll));
    window.set_content(Some(&toolbar));

    {
        let window = window.clone();
        close_btn.connect_clicked(move |_| window.close());
    }

    let body_for_cb = body.clone();
    let device_id = device_id.to_string();
    let copy_payload = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let copy_payload_for_cb = copy_payload.clone();
    let copy_btn_clone = copy_btn.clone();

    store.request_debug_info(move |info| {
        while let Some(child) = body_for_cb.first_child() {
            body_for_cb.remove(&child);
        }
        let device = info.devices.iter().find(|d| d.id == device_id);
        if let Some(d) = device {
            populate_single_device(&body_for_cb, d);
            *copy_payload_for_cb.borrow_mut() = serde_json::to_string_pretty(d).unwrap_or_default();
            copy_btn_clone.set_sensitive(true);
        } else {
            let lbl = gtk::Label::builder()
                .label("Device not found in daemon debug snapshot.")
                .css_classes(["dim-label"])
                .halign(gtk::Align::Start)
                .wrap(true)
                .build();
            body_for_cb.append(&lbl);
            copy_btn_clone.set_sensitive(false);
        }
    });

    let copy_payload_for_click = copy_payload.clone();
    copy_btn.connect_clicked(move |btn| {
        btn.display()
            .clipboard()
            .set_text(&copy_payload_for_click.borrow());
    });

    window.present();
}

/// Open the system-wide debug dialog: every registered device, every HID
/// interface the OS reports, and the runtime environment summary.
pub fn open_system_debug_dialog(parent: Option<&gtk::Window>, store: &Store) {
    let window = adw::Window::builder()
        .modal(true)
        .title("Debug — System")
        .default_width(720)
        .default_height(680)
        .build();
    if let Some(p) = parent {
        window.set_transient_for(Some(p));
    }

    let header = adw::HeaderBar::new();
    let close_btn = gtk::Button::with_label("Close");
    header.pack_start(&close_btn);
    let copy_btn = gtk::Button::with_label("Copy JSON");
    copy_btn.add_css_class("flat");
    header.pack_end(&copy_btn);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(20)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();
    body.append(&loading_label());
    scroll.set_child(Some(&body));
    toolbar.set_content(Some(&scroll));
    window.set_content(Some(&toolbar));

    {
        let window = window.clone();
        close_btn.connect_clicked(move |_| window.close());
    }

    let body_for_cb = body.clone();
    let copy_payload = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
    let copy_payload_for_cb = copy_payload.clone();
    let copy_btn_clone = copy_btn.clone();

    store.request_debug_info(move |info| {
        while let Some(child) = body_for_cb.first_child() {
            body_for_cb.remove(&child);
        }
        populate_system(&body_for_cb, info);
        *copy_payload_for_cb.borrow_mut() = serde_json::to_string_pretty(info).unwrap_or_default();
        copy_btn_clone.set_sensitive(true);
    });

    let copy_payload_for_click = copy_payload.clone();
    copy_btn.connect_clicked(move |btn| {
        btn.display()
            .clipboard()
            .set_text(&copy_payload_for_click.borrow());
    });

    window.present();
}

// ── Renderers ────────────────────────────────────────────────────────────────

fn populate_single_device(body: &gtk::Box, d: &DeviceDebugInfo) {
    body.append(&section_label("Device"));
    let list = boxed_list();
    list.append(&kv_row("ID", &d.id));
    list.append(&kv_row("Name", &d.name));
    list.append(&kv_row("Vendor", &d.vendor));
    list.append(&kv_row("Model", &d.model));
    list.append(&kv_row("Transport", &d.transport));
    list.append(&kv_row("Connected", if d.connected { "yes" } else { "no" }));
    body.append(&list);

    if !d.fields.is_empty() {
        body.append(&section_label("Detection / driver state"));
        let list = boxed_list();
        for (k, v) in &d.fields {
            list.append(&kv_row(k, v));
        }
        body.append(&list);
    }
}

fn populate_system(body: &gtk::Box, info: &DebugInfo) {
    body.append(&section_label("System"));
    body.append(&system_list(&info.system));

    body.append(&section_label(&format!(
        "Registered devices ({})",
        info.devices.len()
    )));
    if info.devices.is_empty() {
        body.append(&dim_text("No devices registered."));
    } else {
        for d in &info.devices {
            body.append(&device_summary(d));
        }
    }

    let unmatched: Vec<&HidEntryDebugInfo> = info
        .hid_entries
        .iter()
        .filter(|e| e.matched_device_id.is_none())
        .collect();
    body.append(&section_label(&format!(
        "Unmatched HID interfaces ({})",
        unmatched.len()
    )));
    if unmatched.is_empty() {
        body.append(&dim_text(
            "Every HID device the OS reports has a HaloDaemon driver bound.",
        ));
    } else {
        body.append(&dim_text(
            "These HID interfaces are visible to the OS but no HaloDaemon driver claims them.",
        ));
        for e in &unmatched {
            body.append(&hid_entry_summary(e));
        }
    }

    let matched_count = info.hid_entries.len() - unmatched.len();
    body.append(&section_label(&format!(
        "Matched HID interfaces ({matched_count})",
    )));
    let matched: Vec<&HidEntryDebugInfo> = info
        .hid_entries
        .iter()
        .filter(|e| e.matched_device_id.is_some())
        .collect();
    if matched.is_empty() {
        body.append(&dim_text("No HID devices currently in use."));
    } else {
        for e in &matched {
            body.append(&hid_entry_summary(e));
        }
    }

    body.append(&section_label(&format!(
        "SMBus controllers ({})",
        info.smbus_buses.len()
    )));
    if info.smbus_buses.is_empty() {
        body.append(&dim_text(
            "No SMBus controllers enumerated. On Windows this means PawnIO is \
             not installed or this process isn't elevated; on Linux it usually \
             means the `i2c-dev` module isn't loaded or `/dev/i2c-*` is not \
             readable.",
        ));
    } else {
        for b in &info.smbus_buses {
            body.append(&smbus_bus_summary(b, &info.devices));
        }
    }
}

fn smbus_bus_summary(b: &SmbusBusDebugInfo, devices: &[DeviceDebugInfo]) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["card"])
        .margin_top(2)
        .margin_bottom(2)
        .build();

    let header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    let title = gtk::Label::builder()
        .label(&format!("Bus {} · {}", b.bus_number, b.kind))
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    header_row.append(&title);

    // Devices claiming this bus, found by scanning each device's `bus` debug
    // field (set by the ENE driver). Anything else is left blank.
    let bus_str = b.bus_number.to_string();
    let claimers: Vec<&str> = devices
        .iter()
        .filter(|d| d.fields.iter().any(|(k, v)| k == "bus" && v == &bus_str))
        .map(|d| d.id.as_str())
        .collect();
    if !claimers.is_empty() {
        let chip = gtk::Label::builder()
            .label(&format!("→ {}", claimers.join(", ")))
            .css_classes(["dim-label", "caption"])
            .build();
        header_row.append(&chip);
    }
    card.append(&header_row);

    let detail = format!(
        "{} · PCI {:04x}:{:04x}",
        b.adapter_name, b.pci_vendor, b.pci_device
    );
    let detail_lbl = gtk::Label::builder()
        .label(&detail)
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::Start)
        .selectable(true)
        .wrap(true)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(8)
        .build();
    card.append(&detail_lbl);
    card
}

fn system_list(s: &SystemDebugInfo) -> gtk::ListBox {
    let list = boxed_list();
    list.append(&kv_row("OS", &s.os));
    if !s.os_version.is_empty() {
        list.append(&kv_row("OS version", &s.os_version));
    }
    list.append(&kv_row(
        "Elevated",
        if s.running_elevated { "yes" } else { "no" },
    ));
    if let Some(p) = s.pawnio_present {
        list.append(&kv_row("PawnIO", if p { "installed" } else { "missing" }));
    }
    if let Some(u) = s.udev_rules_present {
        list.append(&kv_row(
            "udev rules",
            if u { "installed" } else { "not installed" },
        ));
    }
    list.append(&kv_row("Daemon version", &s.daemon_version));
    list.append(&kv_row("Daemon build", &s.daemon_build));
    list
}

fn device_summary(d: &DeviceDebugInfo) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .css_classes(["card"])
        .build();
    let title_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    let title = gtk::Label::builder()
        .label(&d.id)
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    title_row.append(&title);
    let status = gtk::Label::builder()
        .label(if d.connected {
            "● online"
        } else {
            "○ offline"
        })
        .css_classes([if d.connected {
            "connected"
        } else {
            "disconnected"
        }])
        .build();
    title_row.append(&status);
    card.append(&title_row);

    let sub = gtk::Label::builder()
        .label(&format!(
            "{} · {} · transport: {}",
            d.vendor, d.model, d.transport
        ))
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::Start)
        .margin_start(12)
        .margin_end(12)
        .build();
    card.append(&sub);

    if !d.fields.is_empty() {
        let grid = gtk::Grid::builder()
            .row_spacing(2)
            .column_spacing(12)
            .margin_top(6)
            .margin_bottom(10)
            .margin_start(12)
            .margin_end(12)
            .build();
        for (i, (k, v)) in d.fields.iter().enumerate() {
            let key = gtk::Label::builder()
                .label(k)
                .css_classes(["dim-label", "caption"])
                .halign(gtk::Align::Start)
                .build();
            let val = gtk::Label::builder()
                .label(v)
                .selectable(true)
                .halign(gtk::Align::Start)
                .wrap(true)
                .build();
            grid.attach(&key, 0, i as i32, 1, 1);
            grid.attach(&val, 1, i as i32, 1, 1);
        }
        card.append(&grid);
    }
    card
}

fn hid_entry_summary(e: &HidEntryDebugInfo) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["card"])
        .margin_top(2)
        .margin_bottom(2)
        .build();

    let title = format!("{:04x}:{:04x}", e.vid, e.pid);
    let header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    let title_lbl = gtk::Label::builder()
        .label(&title)
        .css_classes(["heading"])
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    header_row.append(&title_lbl);
    if let Some(id) = &e.matched_device_id {
        let chip = gtk::Label::builder()
            .label(&format!("→ {id}"))
            .css_classes(["dim-label", "caption"])
            .build();
        header_row.append(&chip);
    }
    card.append(&header_row);

    let mut subtitle = String::new();
    if !e.product.is_empty() {
        subtitle.push_str(&e.product);
    }
    if !e.manufacturer.is_empty() {
        if !subtitle.is_empty() {
            subtitle.push_str(" · ");
        }
        subtitle.push_str(&e.manufacturer);
    }
    if !subtitle.is_empty() {
        let sub = gtk::Label::builder()
            .label(&subtitle)
            .css_classes(["caption"])
            .halign(gtk::Align::Start)
            .margin_start(12)
            .margin_end(12)
            .wrap(true)
            .build();
        card.append(&sub);
    }

    let detail = format!(
        "iface {} · usage {:04x}:{:04x}{}{}\npath: {}",
        e.interface,
        e.usage_page,
        e.usage,
        if e.serial.is_empty() {
            ""
        } else {
            " · serial "
        },
        e.serial,
        e.path
    );
    let detail_lbl = gtk::Label::builder()
        .label(&detail)
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::Start)
        .selectable(true)
        .wrap(true)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(8)
        .build();
    card.append(&detail_lbl);
    card
}

// ── Small helpers ────────────────────────────────────────────────────────────

fn section_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .halign(gtk::Align::Start)
        .css_classes(["heading"])
        .build()
}

fn dim_text(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::Start)
        .wrap(true)
        .build()
}

fn loading_label() -> gtk::Label {
    gtk::Label::builder()
        .label("Loading…")
        .css_classes(["dim-label"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .vexpand(true)
        .build()
}

fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn kv_row(key: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(key)
        .activatable(false)
        .build();
    let lbl = gtk::Label::builder()
        .label(value)
        .css_classes(["dim-label"])
        .valign(gtk::Align::Center)
        .selectable(true)
        .wrap(true)
        .max_width_chars(48)
        .build();
    row.add_suffix(&lbl);
    row
}
