// Engine panel sub-module — the per-template parameter controls of LcdWidget.
//
// The engine card itself (dropdown + activate/deactivate buttons) is built
// inline in LcdWidget::build() in mod.rs. This file owns the dynamic parameter
// widgets: it reads each template's `EffectParamDescriptor`s and renders one
// widget per parameter, mirroring the canvas page's param controls.

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use halod_protocol::types::{EffectParamValue, LcdEngineTemplateDescriptor, ParamKind};

use super::{LcdWidget, ParamGetter};

impl LcdWidget {
    /// Connect the template dropdown so changing the selection rebuilds the
    /// parameter widgets below it.
    pub(super) fn connect_engine_template_changed(&self, dropdown: &gtk::DropDown) {
        let widget = self.clone();
        dropdown.connect_selected_notify(move |_| {
            widget.rebuild_selected_param_controls();
        });
    }

    /// Rebuild the param controls for whatever template the dropdown currently
    /// points at.
    pub(super) fn rebuild_selected_param_controls(&self) {
        let sel = self.engine_dropdown.selected() as usize;
        let descriptor = self.engine_templates.borrow().get(sel).cloned();
        match descriptor {
            Some(desc) => self.rebuild_param_controls(&desc),
            None => self.clear_param_controls(),
        }
    }

    fn clear_param_controls(&self) {
        while let Some(child) = self.param_box.first_child() {
            self.param_box.remove(&child);
        }
        self.param_getters.borrow_mut().clear();
    }

    /// A change handler shared by every param widget: pushes a live update to
    /// the daemon whenever the engine is already active for this device.
    fn param_change_handler<W>(&self) -> impl Fn(&W) + 'static {
        let lcd = self.clone();
        move |_: &W| {
            if *lcd.engine_active.borrow() {
                lcd.send_set_template();
            }
        }
    }

    /// Replace the param widgets with ones matching `descriptor`'s parameter
    /// schema. Each widget gets a getter (read into the IPC message) and a
    /// change handler that pushes live updates while the engine is active.
    fn rebuild_param_controls(&self, descriptor: &LcdEngineTemplateDescriptor) {
        self.clear_param_controls();

        for param in &descriptor.params {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(12)
                .build();
            let label = gtk::Label::builder()
                .label(&param.label)
                .halign(gtk::Align::Start)
                .width_request(110)
                .css_classes(["dim-label"])
                .build();
            row.append(&label);

            let getter: ParamGetter = match &param.kind {
                ParamKind::Color => {
                    let default = match &param.default {
                        EffectParamValue::Color(c) => gtk::gdk::RGBA::new(
                            c.r as f32 / 255.0,
                            c.g as f32 / 255.0,
                            c.b as f32 / 255.0,
                            1.0,
                        ),
                        _ => gtk::gdk::RGBA::new(1.0, 1.0, 1.0, 1.0),
                    };
                    let dialog = gtk::ColorDialog::builder().with_alpha(false).build();
                    let btn = gtk::ColorDialogButton::builder()
                        .dialog(&dialog)
                        .rgba(&default)
                        .build();
                    btn.connect_rgba_notify(self.param_change_handler());
                    let getter_btn = btn.clone();
                    row.append(&btn);
                    Box::new(move || {
                        let c = getter_btn.rgba();
                        serde_json::json!({
                            "r": (c.red() * 255.0).round() as u8,
                            "g": (c.green() * 255.0).round() as u8,
                            "b": (c.blue() * 255.0).round() as u8,
                        })
                    })
                }
                ParamKind::Range { min, max, step } => {
                    let default = match &param.default {
                        EffectParamValue::Float(f) => *f,
                        _ => *min,
                    };
                    let digits = if *step < 1.0 { 2 } else { 0 };
                    let adj =
                        gtk::Adjustment::new(default, *min, *max, *step, *step * 10.0, 0.0);
                    let spin = gtk::SpinButton::builder()
                        .adjustment(&adj)
                        .digits(digits)
                        .hexpand(true)
                        .build();
                    spin.connect_value_changed(self.param_change_handler());
                    let getter_spin = spin.clone();
                    row.append(&spin);
                    Box::new(move || serde_json::json!(getter_spin.value()))
                }
                ParamKind::Enum { options } => {
                    let default = match &param.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => String::new(),
                    };
                    let opts: Vec<&str> = options.iter().map(String::as_str).collect();
                    let dd = gtk::DropDown::builder()
                        .model(&gtk::StringList::new(&opts))
                        .hexpand(true)
                        .build();
                    let init = options.iter().position(|o| *o == default).unwrap_or(0);
                    dd.set_selected(init as u32);
                    dd.connect_selected_notify(self.param_change_handler());
                    let options = options.clone();
                    let getter_dd = dd.clone();
                    row.append(&dd);
                    Box::new(move || {
                        let i = getter_dd.selected() as usize;
                        serde_json::json!(options.get(i).cloned().unwrap_or_default())
                    })
                }
                ParamKind::Sensor => {
                    let default = match &param.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => String::new(),
                    };
                    let sensors = self.sensors.borrow();
                    let names: Vec<&str> =
                        sensors.iter().map(|(_, s)| s.name.as_str()).collect();
                    let ids: Vec<String> = sensors.iter().map(|(_, s)| s.id.clone()).collect();
                    let dd = gtk::DropDown::builder()
                        .model(&gtk::StringList::new(&names))
                        .hexpand(true)
                        .build();
                    if let Some(pos) = ids.iter().position(|i| *i == default) {
                        dd.set_selected(pos as u32);
                    }
                    drop(sensors);
                    dd.connect_selected_notify(self.param_change_handler());
                    let getter_dd = dd.clone();
                    row.append(&dd);
                    Box::new(move || {
                        let i = getter_dd.selected() as usize;
                        serde_json::json!(ids.get(i).cloned().unwrap_or_default())
                    })
                }
                ParamKind::Text => {
                    let default = match &param.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => String::new(),
                    };
                    let entry = gtk::Entry::builder().text(&default).hexpand(true).build();
                    entry.connect_changed(self.param_change_handler());
                    let getter_entry = entry.clone();
                    row.append(&entry);
                    Box::new(move || serde_json::json!(getter_entry.text().to_string()))
                }
                ParamKind::Boolean => {
                    let default = matches!(&param.default, EffectParamValue::Bool(true));
                    let sw = gtk::Switch::builder()
                        .active(default)
                        .halign(gtk::Align::Start)
                        .hexpand(true)
                        .build();
                    sw.connect_active_notify(self.param_change_handler());
                    let getter_sw = sw.clone();
                    row.append(&sw);
                    Box::new(move || serde_json::json!(getter_sw.is_active()))
                }
            };

            self.param_getters.borrow_mut().push((param.id.clone(), getter));
            self.param_box.append(&row);
        }
    }

    /// Send `lcd_engine_set_template` with the current template + param values.
    /// Used by the "Use Engine" button and by live param edits.
    pub(super) fn send_set_template(&self) {
        let sel = self.engine_dropdown.selected() as usize;
        let template_id = match self.engine_templates.borrow().get(sel) {
            Some(t) => t.id.clone(),
            None => return,
        };
        let mut params = serde_json::Map::new();
        for (id, getter) in self.param_getters.borrow().iter() {
            params.insert(id.clone(), getter());
        }
        self.store.dispatch(crate::commands::Command::CanvasOp(serde_json::json!({
            "type": "lcd_engine_set_template",
            "device_id": self.device_id,
            "template_id": template_id,
            "params": params,
        })));
    }
}
