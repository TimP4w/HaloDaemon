use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use halod_protocol::types::{Action, Boolean, Choice, ChoiceDisplay, DeviceCapability, Range};

pub struct SettingsWidget {
    pub root: gtk::Box,
    bool_labels: Vec<(String, gtk::Label)>,
    range_adjs: Vec<(String, gtk::Adjustment)>,
}

enum OwnedItem {
    Choice(Choice),
    Range(Range),
    Boolean(Boolean),
    Action(Action),
}

impl OwnedItem {
    fn category(&self) -> &str {
        match self {
            OwnedItem::Choice(c) => &c.category,
            OwnedItem::Range(r) => &r.category,
            OwnedItem::Boolean(b) => &b.category,
            OwnedItem::Action(a) => &a.category,
        }
    }
}

impl SettingsWidget {
    pub fn build(device_id: &str, caps: &[&DeviceCapability], store: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(16)
            .build();

        let mut bool_labels: Vec<(String, gtk::Label)> = Vec::new();
        let mut range_adjs: Vec<(String, gtk::Adjustment)> = Vec::new();

        // Flatten all control items in declaration order
        let mut all_items: Vec<OwnedItem> = Vec::new();
        for cap in caps {
            match cap {
                DeviceCapability::Choice(choices) => {
                    for c in choices {
                        all_items.push(OwnedItem::Choice(c.clone()));
                    }
                }
                DeviceCapability::Range(ranges) => {
                    for r in ranges {
                        all_items.push(OwnedItem::Range(r.clone()));
                    }
                }
                DeviceCapability::Boolean(booleans) => {
                    for b in booleans {
                        all_items.push(OwnedItem::Boolean(b.clone()));
                    }
                }
                DeviceCapability::Action(actions) => {
                    for a in actions {
                        all_items.push(OwnedItem::Action(a.clone()));
                    }
                }
                _ => {}
            }
        }

        // Collect unique categories in first-seen order
        let mut category_order: Vec<String> = Vec::new();
        for item in &all_items {
            let cat = item.category().to_string();
            if !category_order.contains(&cat) {
                category_order.push(cat);
            }
        }

        // Render one PreferencesGroup per category
        for category in &category_order {
            let group = adw::PreferencesGroup::new();
            if !category.is_empty() {
                group.set_title(category.as_str());
            }

            for item in all_items.iter().filter(|i| i.category() == category.as_str()) {
                match item {
                    OwnedItem::Choice(choice) => match choice.display {
                        ChoiceDisplay::List => {
                            let row = build_choice_row(device_id, choice, store.clone());
                            group.add(&row);
                        }
                        ChoiceDisplay::Inline => {
                            let row = build_inline_choice_row(device_id, choice, store.clone());
                            group.add(&row);
                        }
                        ChoiceDisplay::Toggle => {
                            let row = build_toggle_choice_row(device_id, choice, store.clone());
                            group.add(&row);
                        }
                    },
                    OwnedItem::Range(range) => {
                        if range.read_only {
                            let (row, adj) = build_range_display_row(range);
                            group.add(&row);
                            range_adjs.push((range.key.clone(), adj));
                        } else {
                            let row = build_range_row(device_id, range, store.clone());
                            group.add(&row);
                        }
                    }
                    OwnedItem::Boolean(boolean) => {
                        if boolean.read_only {
                            let (row, lbl) = build_boolean_label_row(boolean);
                            group.add(&row);
                            bool_labels.push((boolean.key.clone(), lbl));
                        } else {
                            let row = build_boolean_switch_row(device_id, boolean, store.clone());
                            group.add(&row);
                        }
                    }
                    OwnedItem::Action(action) => {
                        let row = build_action_row(device_id, action, store.clone());
                        group.add(&row);
                    }
                }
            }

            root.append(&group);
        }

        Self { root, bool_labels, range_adjs }
    }

    /// Only read-only status labels are refreshed on broadcasts. User-controlled inputs
    /// (Choice dropdowns, Range sliders, Boolean switches) are never overwritten — doing so
    /// would cause them to jump back while the user is interacting.
    pub fn update_live(&self, caps: &[&DeviceCapability]) {
        for cap in caps {
            match cap {
                DeviceCapability::Boolean(booleans) => {
                    for boolean in booleans {
                        if let Some((_, lbl)) =
                            self.bool_labels.iter().find(|(k, _)| k == &boolean.key)
                        {
                            lbl.set_text(bool_status_text(boolean));
                        }
                    }
                }
                DeviceCapability::Range(ranges) => {
                    for range in ranges {
                        if range.read_only {
                            if let Some((_, adj)) =
                                self.range_adjs.iter().find(|(k, _)| k == &range.key)
                            {
                                adj.set_value(range.value as f64);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn build_choice_row(device_id: &str, choice: &Choice, store: Store) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(choice.label.as_str())
        .activatable(false)
        .build();

    let model = gtk::StringList::new(
        &choice.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(),
    );
    let drop = gtk::DropDown::builder()
        .model(&model)
        .selected(choice.selected as u32)
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&drop);

    let id = device_id.to_string();
    let key = choice.key.clone();
    drop.connect_selected_notify(move |d| {
        store.dispatch(crate::commands::Command::SetChoice {
            device_id: id.clone(),
            key: key.clone(),
            selected: d.selected() as usize,
        });
    });

    row
}

fn build_inline_choice_row(device_id: &str, choice: &Choice, store: Store) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(choice.label.as_str())
        .activatable(false)
        .build();

    let btn_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .valign(gtk::Align::Center)
        .build();

    let mut first_btn: Option<gtk::ToggleButton> = None;
    for (i, option) in choice.options.iter().enumerate() {
        let btn = gtk::ToggleButton::builder()
            .label(option.label.as_str())
            .active(i == choice.selected)
            .build();

        if let Some(ref first) = first_btn {
            btn.set_group(Some(first));
        } else {
            first_btn = Some(btn.clone());
        }

        let id = device_id.to_string();
        let key = choice.key.clone();
        let store_clone = store.clone();
        btn.connect_toggled(move |b| {
            if b.is_active() {
                store_clone.dispatch(crate::commands::Command::SetChoice {
                    device_id: id.clone(),
                    key: key.clone(),
                    selected: i,
                });
            }
        });

        btn_box.append(&btn);
    }

    row.add_suffix(&btn_box);
    row
}

fn build_toggle_choice_row(device_id: &str, choice: &Choice, store: Store) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(choice.label.as_str())
        .activatable(false)
        .build();

    let sw = gtk::Switch::builder()
        .active(choice.selected == 1)
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&sw);

    let id = device_id.to_string();
    let key = choice.key.clone();
    sw.connect_active_notify(move |s| {
        store.dispatch(crate::commands::Command::SetChoice {
            device_id: id.clone(),
            key: key.clone(),
            selected: if s.is_active() { 1usize } else { 0usize },
        });
    });

    row
}

fn build_range_row(device_id: &str, range: &Range, store: Store) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(range.label.as_str())
        .activatable(false)
        .build();

    let adj = gtk::Adjustment::new(
        range.value as f64,
        range.min as f64,
        range.max as f64,
        range.step as f64,
        range.step as f64 * 5.0,
        0.0,
    );
    let step = range.step as f64;
    let scale = gtk::Scale::builder()
        .adjustment(&adj)
        .orientation(gtk::Orientation::Horizontal)
        .draw_value(true)
        .value_pos(gtk::PositionType::Right)
        .width_request(200)
        .valign(gtk::Align::Center)
        .build();
    scale.set_digits(0);
    if step > 1.0 {
        let mut v = range.min as f64;
        while v <= range.max as f64 {
            scale.add_mark(v, gtk::PositionType::Bottom, None);
            v += step;
        }
    }
    row.add_suffix(&scale);

    let id = device_id.to_string();
    let key = range.key.clone();
    scale.connect_value_changed(move |s| {
        let raw = s.value();
        let snapped = (raw / step).round() * step;
        if (raw - snapped).abs() > 1e-6 {
            s.set_value(snapped);
            return;
        }
        store.dispatch(crate::commands::Command::SetRange {
            device_id: id.clone(),
            key: key.clone(),
            value: snapped as i32,
        });
    });

    row
}

fn build_boolean_switch_row(
    device_id: &str,
    boolean: &Boolean,
    store: Store,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(boolean.label.as_str())
        .activatable(false)
        .build();

    let sw = gtk::Switch::builder()
        .active(boolean.value)
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&sw);

    let id = device_id.to_string();
    let key = boolean.key.clone();
    sw.connect_active_notify(move |s| {
        store.dispatch(crate::commands::Command::SetBoolean {
            device_id: id.clone(),
            key: key.clone(),
            value: s.is_active(),
        });
    });

    row
}

fn build_range_display_row(range: &Range) -> (adw::ActionRow, gtk::Adjustment) {
    let row = adw::ActionRow::builder()
        .title(range.label.as_str())
        .activatable(false)
        .build();

    let adj = gtk::Adjustment::new(
        range.value as f64,
        range.min as f64,
        range.max as f64,
        range.step as f64,
        range.step as f64 * 5.0,
        0.0,
    );
    let scale = gtk::Scale::builder()
        .adjustment(&adj)
        .orientation(gtk::Orientation::Horizontal)
        .draw_value(false)
        .width_request(200)
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();

    if range.start_label.is_some() || range.end_label.is_some() {
        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        if let Some(ref lbl) = range.start_label {
            container.append(
                &gtk::Label::builder()
                    .label(lbl.as_str())
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        container.append(&scale);
        if let Some(ref lbl) = range.end_label {
            container.append(
                &gtk::Label::builder()
                    .label(lbl.as_str())
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        row.add_suffix(&container);
    } else {
        row.add_suffix(&scale);
    }

    (row, adj)
}

fn build_boolean_label_row(boolean: &Boolean) -> (adw::ActionRow, gtk::Label) {
    let row = adw::ActionRow::builder()
        .title(boolean.label.as_str())
        .activatable(false)
        .build();

    let lbl = gtk::Label::builder()
        .label(bool_status_text(boolean))
        .css_classes(["dim-label"])
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&lbl);

    (row, lbl)
}

fn bool_status_text(boolean: &Boolean) -> &'static str {
    if boolean.value { "Active" } else { "Inactive" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::Boolean;

    fn boolean(value: bool) -> Boolean {
        Boolean { key: "k".into(), label: "L".into(), value, read_only: true, category: "".into() }
    }

    #[test]
    fn bool_status_text_active_when_true() {
        assert_eq!(bool_status_text(&boolean(true)), "Active");
    }

    #[test]
    fn bool_status_text_inactive_when_false() {
        assert_eq!(bool_status_text(&boolean(false)), "Inactive");
    }
}

fn build_action_row(device_id: &str, action: &Action, store: Store) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(action.label.as_str())
        .activatable(false)
        .build();

    let btn = gtk::Button::builder()
        .label("Run")
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&btn);

    let id = device_id.to_string();
    let key = action.key.clone();
    btn.connect_clicked(move |_| {
        store.dispatch(crate::commands::Command::TriggerAction {
            device_id: id.clone(),
            key: key.clone(),
        });
    });

    row
}
