use std::cell::RefCell;
use std::mem::{discriminant, Discriminant};
type DeviceSignature = (bool, Vec<Discriminant<DeviceCapability>>, bool, Option<u8>, Option<usize>, Option<usize>);
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use crate::state::Sensor;
use crate::ui::capability_registry::{CapabilityPanel, CapabilityRegistry};
use crate::ui::widgets::debug_dialog::open_device_debug_dialog;
use halod_protocol::types::{
    ConnectionType, DeviceCapability, DeviceType, WireDevice, WireFanCurve, WireLcdEngineState,
    WirePresetCurve,
};

use super::super::widgets::LcdWidget;

pub fn icon_for_device_type(t: &DeviceType) -> &'static str {
    match t {
        DeviceType::Hub => "drive-multidisk-symbolic",
        DeviceType::AIO => "aio-symbolic",
        DeviceType::Fan => "fan-symbolic",
        DeviceType::Keyboard => "input-keyboard-symbolic",
        DeviceType::Mouse => "input-mouse-symbolic",
        DeviceType::Headset => "audio-headphones-symbolic",
        DeviceType::Monitor => "video-display-symbolic",
        DeviceType::Gpu => "gpu-symbolic",
        DeviceType::LedStrip => "rgb-strip-symbolic",
        DeviceType::Motherboard => "moba-symbolic",
        DeviceType::Ram => "ram-symbolic",
        DeviceType::Sensor => "utilities-system-monitor-symbolic",
        DeviceType::Speaker => "audio-speakers-symbolic",
        DeviceType::Other => "applications-other-symbolic",
    }
}

#[derive(Clone)]
pub struct DevicePage {
    pub root: gtk::Box,
    tab_bar: gtk::Box,
    scroll: gtk::ScrolledWindow,
    store: Store,
    current_id: Rc<RefCell<Option<String>>>,
    /// Panel structure of the device as last rendered — connection state plus
    /// the kinds of capabilities present. Compared on every broadcast to decide
    /// whether the panels need a full rebuild or just a value refresh.
    last_sig: Rc<RefCell<Option<DeviceSignature>>>,
    panels: Rc<RefCell<Vec<Box<dyn CapabilityPanel>>>>,
    active_lcd: Rc<RefCell<Option<LcdWidget>>>,
    /// Header name label of the currently-shown device. Live-updated in
    /// `refresh` so a rename reflects without rebuilding the page.
    name_lbl: Rc<RefCell<Option<gtk::Label>>>,
    registry: Rc<CapabilityRegistry>,
}

impl DevicePage {
    pub fn new(store: &Store, registry: Rc<CapabilityRegistry>) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let tab_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .visible(false)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        root.append(&tab_bar);
        root.append(&scroll);

        Self {
            root,
            tab_bar,
            scroll,
            store: store.clone(),
            current_id: Rc::new(RefCell::new(None)),
            last_sig: Rc::new(RefCell::new(None)),
            panels: Rc::new(RefCell::new(Vec::new())),
            active_lcd: Rc::new(RefCell::new(None)),
            name_lbl: Rc::new(RefCell::new(None)),
            registry,
        }
    }

    pub fn current_id(&self) -> Option<String> {
        self.current_id.borrow().clone()
    }

    /// True when `device`'s panel structure differs from what is rendered — its
    /// capability set or connection state changed, so the panels must be
    /// rebuilt via `show_device` rather than value-refreshed via `refresh`.
    /// This is what makes a device that just came online show its controls
    /// (and one that went offline drop them) without the user re-navigating.
    pub fn structure_changed(&self, device: &WireDevice) -> bool {
        let sig = device_signature(device);
        self.last_sig.borrow().as_ref() != Some(&sig)
    }

    pub fn lcd_widget(&self) -> std::cell::Ref<'_, Option<LcdWidget>> {
        self.active_lcd.borrow()
    }

    /// Called on every state broadcast; updates live values without rebuilding.
    pub fn refresh(
        &self,
        device: &WireDevice,
        _fan_curves: &[WireFanCurve],
        all_sensors: &[(String, Sensor)],
        lcd_engine: &WireLcdEngineState,
    ) {
        if let Some(lbl) = self.name_lbl.borrow().as_ref() {
            if lbl.text().as_str() != device.name {
                lbl.set_text(&device.name);
            }
        }
        let state = self.store.state();
        for p in self.panels.borrow().iter() {
            p.update_live(&state);
        }
        if let Some(lcd) = self.active_lcd.borrow().as_ref() {
            for cap in &device.capabilities {
                if let DeviceCapability::Lcd(status) = cap {
                    lcd.update_live(status);
                    lcd.update_engine_section(lcd_engine, all_sensors);
                    break;
                }
            }
        }
    }

    pub fn show_device(
        &self,
        device: &WireDevice,
        _all_devices: &[WireDevice],
        _fan_curves: &[WireFanCurve],
        _preset_curves: &[WirePresetCurve],
        _all_sensors: &[(String, Sensor)],
    ) {
        *self.current_id.borrow_mut() = Some(device.id.clone());
        *self.last_sig.borrow_mut() = Some(device_signature(device));
        *self.panels.borrow_mut() = Vec::new();
        *self.active_lcd.borrow_mut() = None;

        while let Some(child) = self.tab_bar.first_child() {
            self.tab_bar.remove(&child);
        }

        let stack = adw::ViewStack::new();
        let mut tab_entries: Vec<(String, String)> = Vec::new();

        // Build panels via registry (all except LCD)
        let panels = self.registry.build_panels(device, &self.store);
        for p in &panels {
            stack.add_titled_with_icon(
                &p.root_widget(),
                Some(p.tab_name()),
                p.tab_label(),
                p.tab_icon(),
            );
            tab_entries.push((p.tab_label().into(), p.tab_name().into()));
        }

        // LCD: handled separately to keep a typed reference for out-of-band messages
        for cap in &device.capabilities {
            if let DeviceCapability::Lcd(status) = cap {
                let lcd = LcdWidget::build(&device.id, status, &self.store);
                stack.add_titled_with_icon(
                    &lcd.root.clone().upcast::<gtk::Widget>(),
                    Some("lcd"),
                    "LCD",
                    "display-projector-symbolic",
                );
                tab_entries.push(("LCD".into(), "lcd".into()));
                let lcd2 = lcd.clone();
                let lcd3 = lcd.clone();
                let lcd4 = lcd.clone();
                self.store.set_lcd_callbacks(
                    move |files| lcd2.update_library(files),
                    move |req_id| lcd3.on_image_uploaded(req_id),
                    move || lcd4.on_upload_error(),
                );
                *self.active_lcd.borrow_mut() = Some(lcd);
                break;
            }
        }

        let n_pages = tab_entries.len() as u32;
        *self.panels.borrow_mut() = panels;

        if n_pages == 0 {
            let empty = gtk::Label::builder()
                .label(if device.connected {
                    "No controls available for this device"
                } else {
                    "Device is offline"
                })
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .vexpand(true)
                .css_classes(["dim-label"])
                .build();
            stack.add_titled_with_icon(&empty, Some("overview"), "Overview", "view-grid-symbolic");
        }

        self.tab_bar.set_visible(false);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(32)
            .margin_end(32)
            .margin_top(8)
            .margin_bottom(32)
            .build();

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();

        let (info_row, name_lbl) = make_info_row(device, &self.store);
        info_row.set_hexpand(true);
        header.append(&info_row);
        *self.name_lbl.borrow_mut() = Some(name_lbl);

        let debug_btn = gtk::Button::builder()
            .icon_name("dialog-information-symbolic")
            .css_classes(["flat", "circular"])
            .tooltip_text("Show device debug info")
            .valign(gtk::Align::Center)
            .build();
        {
            let store = self.store.clone();
            let device_id = device.id.clone();
            debug_btn.connect_clicked(move |btn| {
                let win = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok());
                open_device_debug_dialog(win.as_ref(), &store, &device_id);
            });
        }
        header.append(&debug_btn);

        if n_pages >= 2 {
            let tab_bar = make_pill_tab_bar(&tab_entries, &stack);
            tab_bar.set_valign(gtk::Align::Center);
            header.append(&tab_bar);
        }

        content.append(&header);
        content.append(&stack);
        self.scroll.set_child(Some(&content));
    }
}

/// Signature of a device's panel structure: connection state, capability kinds,
/// fan controllability, onboard profile active slot, and report-rate selection.
///
/// Two snapshots with the same signature render the identical set of panels and
/// values for profile-sensitive controls, so a value refresh suffices; a different
/// signature means the panels must be rebuilt.
///
/// `active_slot` and `report_rate_selected` are included because these values
/// change when a software or onboard profile is switched, but they do not change
/// during the IPC round-trip for user-initiated edits of those same controls.
/// Adding them here triggers a full rebuild when a profile switch changes them,
/// which re-creates the widgets with the correct initial values — without causing
/// the mid-drag flicker that updating live widgets in update_live() would cause.
fn device_signature(device: &WireDevice) -> DeviceSignature {
    let fan_controllable = device
        .capabilities
        .iter()
        .any(|c| matches!(c, DeviceCapability::Fan(s) if s.controllable));
    let active_slot = device.capabilities.iter().find_map(|c| {
        if let DeviceCapability::OnboardProfiles(p) = c { Some(p.active_slot) } else { None }
    });
    let report_rate_selected = device.capabilities.iter().find_map(|c| {
        if let DeviceCapability::Choice(choices) = c {
            choices.iter().find(|ch| ch.key == "report_rate").map(|ch| ch.selected)
        } else {
            None
        }
    });
    let eq_selected_preset = device.capabilities.iter().find_map(|c| {
        if let DeviceCapability::Equalizer(eq) = c { Some(eq.selected_preset) } else { None }
    });
    (
        device.connected,
        device.capabilities.iter().map(discriminant).collect(),
        fan_controllable,
        active_slot,
        report_rate_selected,
        eq_selected_preset,
    )
}

fn make_info_row(
    device: &WireDevice,
    store: &Store,
) -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .css_classes(["device-info-row"])
        .build();

    let icon = gtk::Image::builder()
        .icon_name(icon_for_device_type(&device.device_type))
        .pixel_size(40)
        .valign(gtk::Align::Center)
        .build();

    let text_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();

    let name_lbl = gtk::Label::builder()
        .label(&device.name)
        .halign(gtk::Align::Start)
        .css_classes(["device-info-name"])
        .build();

    let name_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .valign(gtk::Align::Center)
        .build();
    name_row.append(&name_lbl);

    let rename_btn = gtk::Button::builder()
        .icon_name("document-edit-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Rename device")
        .valign(gtk::Align::Center)
        .build();
    {
        let store = store.clone();
        let device_id = device.id.clone();
        rename_btn.connect_clicked(move |btn| {
            let win = btn.root().and_downcast::<gtk::Window>();
            let current_name = store
                .state()
                .devices
                .iter()
                .find(|d| d.id == device_id)
                .map(|d| d.name.clone())
                .unwrap_or_default();
            crate::ui::widgets::rename_dialog::open_rename_dialog(
                win.as_ref(),
                &store,
                &device_id,
                &current_name,
            );
        });
    }
    name_row.append(&rename_btn);

    if let Some(ct) = &device.connection_type {
        let (icon_name, label) = match ct {
            ConnectionType::Wired => ("network-wired-symbolic", "Wired"),
            ConnectionType::Wireless => ("network-wireless-symbolic", "Wireless"),
        };
        let pill = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .valign(gtk::Align::Center)
            .css_classes(["connection-type-pill"])
            .build();
        let pill_icon = gtk::Image::builder()
            .icon_name(icon_name)
            .pixel_size(12)
            .build();
        let pill_lbl = gtk::Label::builder().label(label).build();
        pill.append(&pill_icon);
        pill.append(&pill_lbl);
        name_row.append(&pill);
    }

    let sub_lbl = gtk::Label::builder()
        .label(format!("{} · {}", device.vendor, device.model))
        .halign(gtk::Align::Start)
        .css_classes(["device-info-sub"])
        .build();

    text_box.append(&name_row);
    text_box.append(&sub_lbl);

    if let Some(serial) = &device.serial_number {
        let serial_lbl = gtk::Label::builder()
            .label(format!("S/N: {serial}"))
            .halign(gtk::Align::Start)
            .css_classes(["device-info-sub", "dim-label"])
            .build();
        text_box.append(&serial_lbl);
    }
    row.append(&icon);
    row.append(&text_box);

    (row, name_lbl)
}

fn make_pill_tab_bar(entries: &[(String, String)], stack: &adw::ViewStack) -> gtk::Box {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .halign(gtk::Align::End)
        .css_classes(["tab-pill-bar"])
        .build();

    let mut first_btn: Option<gtk::ToggleButton> = None;
    for (label, page_name) in entries {
        let btn = gtk::ToggleButton::builder()
            .label(label.as_str())
            .css_classes(["tab-pill", "flat"])
            .build();

        if let Some(ref first) = first_btn {
            btn.set_group(Some(first));
        } else {
            btn.set_active(true);
            first_btn = Some(btn.clone());
        }

        let stack_clone = stack.clone();
        let pname = page_name.clone();
        btn.connect_toggled(move |b| {
            if b.is_active() {
                stack_clone.set_visible_child_name(&pname);
            }
        });

        outer.append(&btn);
    }

    outer
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{DeviceCapability, DeviceType, FanStatus, WireDevice};

    // ── icon_for_device_type ──────────────────────────────────────────────────

    #[test]
    fn icon_for_device_type_spot_checks() {
        assert_eq!(
            icon_for_device_type(&DeviceType::Mouse),
            "input-mouse-symbolic"
        );
        assert_eq!(
            icon_for_device_type(&DeviceType::Keyboard),
            "input-keyboard-symbolic"
        );
        assert_eq!(
            icon_for_device_type(&DeviceType::Headset),
            "audio-headphones-symbolic"
        );
        assert_eq!(
            icon_for_device_type(&DeviceType::Monitor),
            "video-display-symbolic"
        );
        assert_eq!(
            icon_for_device_type(&DeviceType::Hub),
            "drive-multidisk-symbolic"
        );
    }

    #[test]
    fn icon_for_device_type_every_variant_is_non_empty() {
        for t in [
            DeviceType::Hub,
            DeviceType::AIO,
            DeviceType::Fan,
            DeviceType::Keyboard,
            DeviceType::Mouse,
            DeviceType::Headset,
            DeviceType::Monitor,
            DeviceType::Motherboard,
            DeviceType::Sensor,
            DeviceType::Speaker,
            DeviceType::Other,
        ] {
            assert!(!icon_for_device_type(&t).is_empty(), "empty icon for {t:?}");
        }
    }

    // ── device_signature ─────────────────────────────────────────────────────

    fn device(connected: bool, caps: Vec<DeviceCapability>) -> WireDevice {
        WireDevice {
            id: "d".into(),
            name: "d".into(),
            connected,
            capabilities: caps,
            ..Default::default()
        }
    }

    #[test]
    fn device_signature_reflects_connected_state() {
        let (c, _, _, _, _, _) = device_signature(&device(true, vec![]));
        let (d, _, _, _, _, _) = device_signature(&device(false, vec![]));
        assert!(c);
        assert!(!d);
    }

    #[test]
    fn device_signature_same_caps_equal() {
        let fan_cap = DeviceCapability::Fan(FanStatus::default());
        let (_, a, _, _, _, _) = device_signature(&device(true, vec![fan_cap.clone()]));
        let (_, b, _, _, _, _) = device_signature(&device(true, vec![fan_cap]));
        assert_eq!(a, b);
    }

    #[test]
    fn device_signature_different_cap_count_differs() {
        let (_, zero, _, _, _, _) = device_signature(&device(true, vec![]));
        let (_, one, _, _, _, _) = device_signature(&device(
            true,
            vec![DeviceCapability::Fan(FanStatus::default())],
        ));
        assert_ne!(zero, one);
    }

    #[test]
    fn device_signature_detects_controllable_change() {
        let uncontrollable = DeviceCapability::Fan(FanStatus {
            controllable: false,
            ..Default::default()
        });
        let controllable = DeviceCapability::Fan(FanStatus {
            controllable: true,
            ..Default::default()
        });
        let (_, _, before, _, _, _) = device_signature(&device(true, vec![uncontrollable]));
        let (_, _, after, _, _, _) = device_signature(&device(true, vec![controllable]));
        assert!(!before);
        assert!(after);
    }

    #[test]
    fn device_signature_detects_onboard_profile_slot_change() {
        use halod_protocol::types::{OnboardProfiles, OnboardProfileSlot};
        let profiles_slot1 = OnboardProfiles {
            active_slot: 1,
            slots: vec![
                OnboardProfileSlot { index: 1, enabled: true, active: true, has_rom_default: true },
                OnboardProfileSlot { index: 2, enabled: true, active: false, has_rom_default: true },
            ],
        };
        let profiles_slot2 = OnboardProfiles {
            active_slot: 2,
            slots: vec![
                OnboardProfileSlot { index: 1, enabled: true, active: false, has_rom_default: true },
                OnboardProfileSlot { index: 2, enabled: true, active: true, has_rom_default: true },
            ],
        };
        let sig1 = device_signature(&device(true, vec![DeviceCapability::OnboardProfiles(profiles_slot1)]));
        let sig2 = device_signature(&device(true, vec![DeviceCapability::OnboardProfiles(profiles_slot2)]));
        assert_ne!(sig1, sig2, "switching onboard profile slot must change signature");
    }

    #[test]
    fn device_signature_detects_report_rate_selection_change() {
        use halod_protocol::types::{Choice, ChoiceDisplay, ChoiceOption};
        fn rate_cap(selected: usize) -> DeviceCapability {
            DeviceCapability::Choice(vec![Choice {
                key: "report_rate".into(),
                label: "Report Rate".into(),
                options: vec![
                    ChoiceOption { id: "1".into(), label: "1ms".into() },
                    ChoiceOption { id: "2".into(), label: "2ms".into() },
                ],
                selected,
                category: String::new(),
                display: ChoiceDisplay::List,
            }])
        }
        let sig1 = device_signature(&device(true, vec![rate_cap(0)]));
        let sig2 = device_signature(&device(true, vec![rate_cap(1)]));
        assert_ne!(sig1, sig2, "changing report_rate selection must change signature");
    }

    #[test]
    fn device_signature_detects_eq_preset_change() {
        use halod_protocol::types::{ChoiceOption, EqBand, Equalizer};

        fn eq_cap(selected_preset: usize) -> DeviceCapability {
            DeviceCapability::Equalizer(Equalizer {
                presets: vec![
                    ChoiceOption { id: "0".into(), label: "Flat".into() },
                    ChoiceOption { id: "1".into(), label: "Bass Boost".into() },
                ],
                selected_preset,
                bands: vec![
                    EqBand { index: 0, label: "100 Hz".into(), min: -10.0, max: 10.0, step: 0.5, value: 0.0 };
                    10
                ],
            })
        }

        let sig1 = device_signature(&device(true, vec![eq_cap(0)]));
        let sig2 = device_signature(&device(true, vec![eq_cap(1)]));
        assert_ne!(sig1, sig2, "changing EQ selected_preset must change signature");
    }
}
