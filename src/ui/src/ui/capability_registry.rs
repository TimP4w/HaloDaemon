use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::{Store, NavTarget};
use crate::state::AppState;
use halod_protocol::types::{
    Battery, DeviceCapability, DpiStatus, FanStatus, OnboardProfiles, WireDevice,
};

use crate::ui::widgets::{
    BatteryWidget, DpiProfileWidget, EqualizerWidget, FanCurveWidget, KeyRemapWidget,
    OnboardProfilesWidget, RgbWidget, SettingsWidget,
};
use crate::ui::pages::device::icon_for_device_type;

pub trait CapabilityPanel {
    fn root_widget(&self) -> gtk::Widget;
    fn tab_label(&self) -> &'static str;
    fn tab_icon(&self)  -> &'static str;
    fn tab_name(&self)  -> &'static str;
    /// Called from each panel's own Store subscription.
    /// MUST NOT update user-controlled widgets (sliders, dropdowns, switches).
    fn update_live(&self, state: &AppState);
}

pub struct CapabilityRegistry {
    builders: Vec<Box<dyn Fn(&WireDevice, &Store) -> Option<Box<dyn CapabilityPanel>>>>,
}

impl CapabilityRegistry {
    pub fn new() -> Self {
        Self { builders: Vec::new() }
    }

    pub fn register<F>(&mut self, f: F)
    where
        F: Fn(&WireDevice, &Store) -> Option<Box<dyn CapabilityPanel>> + 'static,
    {
        self.builders.push(Box::new(f));
    }

    /// LCD is managed separately by DevicePage; this returns all other panels.
    pub fn build_panels(
        &self,
        device: &WireDevice,
        store: &Store,
    ) -> Vec<Box<dyn CapabilityPanel>> {
        self.builders.iter().filter_map(|b| b(device, store)).collect()
    }

    /// Construct with all built-in panels registered.
    /// Called once in `main.rs`; the returned value is passed to `DevicePage::new`.
    pub fn default_registry() -> Self {
        let mut r = Self::new();

        // Controls tab: Battery + Choice/Range/Boolean + DPI
        r.register(|device, store| {
            let caps: Vec<&DeviceCapability> = device.capabilities.iter().filter(|c| {
                matches!(c,
                    DeviceCapability::Battery(_)
                    | DeviceCapability::Choice(_)
                    | DeviceCapability::Range(_)
                    | DeviceCapability::Boolean(_)
                    | DeviceCapability::Action(_)
                    | DeviceCapability::Dpi(_)
                    | DeviceCapability::OnboardProfiles(_)
                )
            }).collect();
            if caps.is_empty() { return None; }
            Some(Box::new(ControlsPanel::build(&device.id, store, &caps)) as Box<dyn CapabilityPanel>)
        });

        // Fan tab
        r.register(|device, store| {
            let status = device.fan()?;
            if !status.controllable { return None; }
            let state = store.state();
            let sensors = state.all_sensors();
            let curve = state.fan_curves.iter().find(|c| c.fan_id == device.id);
            let widget = FanCurveWidget::build(
                &device.id, status, curve, &sensors, &state.preset_curves, store,
            );
            Some(Box::new(FanPanel { device_id: device.id.clone(), widget, is_pump: false }) as Box<dyn CapabilityPanel>)
        });

        // Pump tab
        r.register(|device, store| {
            let ps = device.pump()?;
            if !ps.controllable { return None; }
            let state = store.state();
            let sensors = state.all_sensors();
            let curve = state.fan_curves.iter().find(|c| c.fan_id == device.id);
            let fs = FanStatus { channel: 0, rpm: ps.rpm, duty: ps.duty, controllable: ps.controllable };
            let widget = FanCurveWidget::build(
                &device.id, &fs, curve, &sensors, &state.preset_curves, store,
            );
            Some(Box::new(FanPanel { device_id: device.id.clone(), widget, is_pump: true }) as Box<dyn CapabilityPanel>)
        });

        // Lighting tab — skip hubs that publish an Rgb capability solely to
        // carry chainable-channel metadata for their children (no zones of their own).
        r.register(|device, store| {
            let status = device.rgb()?;
            if status.descriptor.zones.is_empty() { return None; }
            Some(Box::new(RgbWidget::build(&device.id, status, device, store)) as Box<dyn CapabilityPanel>)
        });

        // Buttons (key remap) tab
        r.register(|device, store| {
            let status = device.key_remap()?;
            Some(Box::new(KeyRemapWidget::build(&device.id, status, device, store)) as Box<dyn CapabilityPanel>)
        });

        // Equalizer tab
        r.register(|device, store| {
            let eq = device.equalizer()?;
            Some(Box::new(EqualizerWidget::build(&device.id, eq, store)) as Box<dyn CapabilityPanel>)
        });

        // Devices (children) tab
        r.register(|device, store| {
            let children = device.children()?;
            if children.is_empty() { return None; }
            Some(Box::new(ChildrenPanel::build(children, store)) as Box<dyn CapabilityPanel>)
        });

        r
    }
}

// ── ControlsPanel ──────────────────────────────────────────────────────────────

struct ControlsPanel {
    device_id: String,
    root:    gtk::Box,
    battery: Option<BatteryWidget>,
    settings: Option<SettingsWidget>,
    dpi:     Option<DpiProfileWidget>,
    onboard_profiles: Option<OnboardProfilesWidget>,
}

impl ControlsPanel {
    fn build(device_id: &str, store: &Store, caps: &[&DeviceCapability]) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(16)
            .build();

        let mut batteries: Vec<Battery> = Vec::new();
        let mut controls: Vec<&DeviceCapability> = Vec::new();
        let mut dpi_profile: Option<&DpiStatus> = None;
        let mut onboard: Option<&OnboardProfiles> = None;

        for cap in caps {
            match cap {
                DeviceCapability::Battery(b) => batteries.extend(b.iter().cloned()),
                DeviceCapability::Choice(_)
                | DeviceCapability::Range(_)
                | DeviceCapability::Boolean(_)
                | DeviceCapability::Action(_) => controls.push(cap),
                DeviceCapability::Dpi(p) => dpi_profile = Some(p),
                DeviceCapability::OnboardProfiles(p) => onboard = Some(p),
                _ => {}
            }
        }

        let battery = if !batteries.is_empty() {
            let w = BatteryWidget::build(&batteries);
            root.append(&w.root);
            Some(w)
        } else { None };

        let settings = if !controls.is_empty() {
            let sw = SettingsWidget::build(device_id, &controls, store);
            root.append(&sw.root);
            Some(sw)
        } else { None };

        let dpi = if let Some(p) = dpi_profile {
            let w = DpiProfileWidget::build(device_id, p, store);
            root.append(&w.root);
            Some(w)
        } else { None };

        let onboard_profiles = if let Some(p) = onboard {
            let w = OnboardProfilesWidget::build(device_id, p, store);
            root.append(&w.root);
            Some(w)
        } else { None };

        Self { device_id: device_id.to_string(), root, battery, settings, dpi, onboard_profiles }
    }
}

impl CapabilityPanel for ControlsPanel {
    fn root_widget(&self) -> gtk::Widget { self.root.clone().upcast() }
    fn tab_label(&self) -> &'static str  { "Controls" }
    fn tab_icon(&self)  -> &'static str  { "preferences-other-symbolic" }
    fn tab_name(&self)  -> &'static str  { "controls" }

    fn update_live(&self, state: &AppState) {
        let Some(device) = state.devices.iter().find(|d| d.id == self.device_id) else { return };
        if let Some(w) = &self.battery {
            let batteries = device.battery().map(|b| b.as_slice()).unwrap_or(&[]);
            w.update_live(batteries);
        }
        if let Some(w) = &self.settings {
            let caps: Vec<&DeviceCapability> = device.capabilities.iter().collect();
            w.update_live(&caps);
        }
        if let (Some(p), Some(w)) = (device.dpi(), &self.dpi) {
            w.update_live(p);
        }
        if let (Some(p), Some(w)) = (device.onboard_profiles(), &self.onboard_profiles) {
            w.update_live(p);
        }
    }
}

// ── FanPanel ───────────────────────────────────────────────────────────────────

struct FanPanel {
    device_id: String,
    widget:    FanCurveWidget,
    is_pump:   bool,
}

impl CapabilityPanel for FanPanel {
    fn root_widget(&self) -> gtk::Widget { self.widget.root.clone().upcast() }
    fn tab_label(&self) -> &'static str  { if self.is_pump { "Pump" } else { "Cooling" } }
    fn tab_icon(&self)  -> &'static str  { "fan-symbolic" }
    fn tab_name(&self)  -> &'static str  { if self.is_pump { "pump" } else { "cooling" } }

    fn update_live(&self, state: &AppState) {
        let Some(device) = state.devices.iter().find(|d| d.id == self.device_id) else { return };
        let sensors = state.all_sensors();
        let Some(curve) = state.fan_curves.iter().find(|c| c.fan_id == self.device_id) else { return };
        if self.is_pump {
            if let Some(ps) = device.pump() {
                let fs = FanStatus { channel: 0, rpm: ps.rpm, duty: ps.duty, controllable: ps.controllable };
                self.widget.apply_fan_state(curve, &fs, &sensors);
            }
        } else if let Some(fs) = device.fan() {
            self.widget.apply_fan_state(curve, fs, &sensors);
        }
    }
}

// ── ChildrenPanel ──────────────────────────────────────────────────────────────

struct ChildrenPanel { root: gtk::Box }

impl ChildrenPanel {
    fn build(children: &[WireDevice], store: &Store) -> Self {
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_top(16)
            .build();

        if children.is_empty() {
            let lbl = gtk::Label::builder()
                .label("No connected devices")
                .halign(gtk::Align::Center)
                .css_classes(["dim-label"])
                .build();
            page.append(&lbl);
            return Self { root: page };
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();

        for child in children {
            let row = adw::ActionRow::builder()
                .title(child.name.as_str())
                .subtitle(child.model.as_str())
                .activatable(true)
                .build();
            let icon = gtk::Image::builder()
                .icon_name(icon_for_device_type(&child.device_type))
                .pixel_size(20)
                .build();
            row.add_prefix(&icon);
            let chevron = gtk::Image::builder()
                .icon_name("go-next-symbolic")
                .pixel_size(16)
                .css_classes(["dim-label"])
                .build();
            row.add_suffix(&chevron);

            let store_nav = store.clone();
            let id = child.id.clone();
            row.connect_activated(move |_| {
                store_nav.navigate(NavTarget::Device(id.clone()));
            });
            list.append(&row);
        }

        page.append(&list);
        Self { root: page }
    }
}

impl CapabilityPanel for ChildrenPanel {
    fn root_widget(&self) -> gtk::Widget { self.root.clone().upcast() }
    fn tab_label(&self) -> &'static str  { "Devices" }
    fn tab_icon(&self)  -> &'static str  { "preferences-system-symbolic" }
    fn tab_name(&self)  -> &'static str  { "devices" }
    fn update_live(&self, _state: &AppState) {}
}
