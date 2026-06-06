use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use halod_protocol::types::{EffectParamValue, ParamKind, RgbColor, RgbDescriptor};

use super::{WState, gdk_rgba};

pub(super) fn build_effect_content(
    panel: &gtk::Box,
    effect_id: &str,
    descriptor: &RgbDescriptor,
    state: &Rc<RefCell<WState>>,
) {
    let params = descriptor
        .native_effects
        .iter()
        .find(|e| e.id == effect_id)
        .map(|e| e.params.as_slice());

    let Some(params) = params else {
        let lbl = gtk::Label::builder()
            .label("No parameters for this effect")
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .build();
        panel.append(&lbl);
        return;
    };

    if params.is_empty() {
        let lbl = gtk::Label::builder()
            .label("No configurable parameters")
            .css_classes(["dim-label"])
            .halign(gtk::Align::Start)
            .build();
        panel.append(&lbl);
        return;
    }

    for param in params {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();

        let lbl = gtk::Label::builder()
            .label(&param.label)
            .halign(gtk::Align::Start)
            .css_classes(["rgb-param-label"])
            .build();
        row.append(&lbl);

        match &param.kind {
            ParamKind::Range { min, max, step } => {
                let init = match state.borrow().effect_params.get(&param.id) {
                    Some(EffectParamValue::Float(v)) => *v,
                    _ => match &param.default {
                        EffectParamValue::Float(v) => *v,
                        _ => *min,
                    },
                };
                let adj = gtk::Adjustment::new(init, *min, *max, *step, *step * 10.0, 0.0);
                let slider = gtk::Scale::builder().adjustment(&adj).hexpand(true).build();
                if *step >= 1.0 {
                    slider.set_digits(0);
                } else {
                    slider.set_digits(1);
                }
                {
                    let state = state.clone();
                    let pid = param.id.clone();
                    slider.connect_value_changed(move |s| {
                        state
                            .borrow_mut()
                            .effect_params
                            .insert(pid.clone(), EffectParamValue::Float(s.value()));
                    });
                }
                row.append(&slider);
            }
            ParamKind::Color => {
                let init = match state.borrow().effect_params.get(&param.id) {
                    Some(EffectParamValue::Color(c)) => *c,
                    _ => match &param.default {
                        EffectParamValue::Color(c) => *c,
                        _ => RgbColor { r: 255, g: 255, b: 255 },
                    },
                };
                let dialog = gtk::ColorDialog::new();
                let btn = gtk::ColorDialogButton::new(Some(dialog));
                btn.set_rgba(&gdk_rgba(init));
                {
                    let state = state.clone();
                    let pid = param.id.clone();
                    btn.connect_rgba_notify(move |b| {
                        let rgba = b.rgba();
                        state.borrow_mut().effect_params.insert(
                            pid.clone(),
                            EffectParamValue::Color(RgbColor {
                                r: (rgba.red() * 255.0) as u8,
                                g: (rgba.green() * 255.0) as u8,
                                b: (rgba.blue() * 255.0) as u8,
                            }),
                        );
                    });
                }
                row.append(&btn);
            }
            ParamKind::Enum { options } => {
                let opts: Vec<&str> = options.iter().map(|s| s.as_str()).collect();
                let model = gtk::StringList::new(&opts);
                let dd = gtk::DropDown::builder().model(&model).hexpand(true).build();
                let init = match state.borrow().effect_params.get(&param.id) {
                    Some(EffectParamValue::Str(s)) => {
                        options.iter().position(|o| o == s).unwrap_or(0)
                    }
                    _ => 0,
                };
                dd.set_selected(init as u32);
                {
                    let state = state.clone();
                    let pid = param.id.clone();
                    let options = options.clone();
                    dd.connect_selected_notify(move |d| {
                        if let Some(opt) = options.get(d.selected() as usize) {
                            state
                                .borrow_mut()
                                .effect_params
                                .insert(pid.clone(), EffectParamValue::Str(opt.clone()));
                        }
                    });
                }
                row.append(&dd);
            }
            ParamKind::Boolean => {
                let init = match state.borrow().effect_params.get(&param.id) {
                    Some(EffectParamValue::Bool(b)) => *b,
                    _ => match &param.default {
                        EffectParamValue::Bool(b) => *b,
                        _ => false,
                    },
                };
                let sw = gtk::Switch::builder().active(init).halign(gtk::Align::Start).build();
                {
                    let state = state.clone();
                    let pid = param.id.clone();
                    sw.connect_active_notify(move |s| {
                        state
                            .borrow_mut()
                            .effect_params
                            .insert(pid.clone(), EffectParamValue::Bool(s.is_active()));
                    });
                }
                row.append(&sw);
            }
            // Text/Sensor params are not used by RGB device effects; render a
            // plain text entry so the match stays exhaustive.
            ParamKind::Text | ParamKind::Sensor => {
                let init = match state.borrow().effect_params.get(&param.id) {
                    Some(EffectParamValue::Str(s)) => s.clone(),
                    _ => match &param.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => String::new(),
                    },
                };
                let entry = gtk::Entry::builder().text(&init).hexpand(true).build();
                {
                    let state = state.clone();
                    let pid = param.id.clone();
                    entry.connect_changed(move |e| {
                        state
                            .borrow_mut()
                            .effect_params
                            .insert(pid.clone(), EffectParamValue::Str(e.text().to_string()));
                    });
                }
                row.append(&entry);
            }
        }

        panel.append(&row);
    }
}
