use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::state::AppRule;
use crate::store::Store;
use crate::commands::Command;

#[derive(Clone)]
pub struct AppRulesWidget {
    pub root: adw::ExpanderRow,
    store: Store,
    rows_box: gtk::Box,
    unsupported_row: adw::ActionRow,
    profiles: Rc<RefCell<Vec<String>>>,
}

impl AppRulesWidget {
    pub fn new(store: &Store) -> Self {
        let root = adw::ExpanderRow::builder()
            .title("App Rules")
            .subtitle("Switch profiles based on the foreground app")
            .build();

        let rows_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.add_row(&rows_box);

        // Row shown when the focus backend is not available on this system.
        let unsupported_row = adw::ActionRow::builder()
            .title("Not available on this system")
            .subtitle("Requires a compositor with foreign-toplevel support or AT-SPI2 accessibility")
            .css_classes(["dim-label"])
            .sensitive(false)
            .visible(false)
            .build();
        root.add_row(&unsupported_row);

        let add_row = adw::ActionRow::builder()
            .title("Add Rule")
            .activatable(true)
            .build();
        let add_icon = gtk::Image::builder()
            .icon_name("list-add-symbolic")
            .pixel_size(16)
            .build();
        add_row.add_prefix(&add_icon);
        root.add_row(&add_row);

        let profiles = Rc::new(RefCell::new(Vec::<String>::new()));

        {
            let store = store.clone();
            add_row.connect_activated(move |_| {
                store.dispatch(Command::AddAppRule {
                    process_names: vec![],
                    profile: String::new(),
                    enabled: true,
                });
            });
        }

        Self {
            root,
            store: store.clone(),
            rows_box,
            unsupported_row,
            profiles,
        }
    }

    pub fn update(&self, rules: &[AppRule], profiles: &[String], supported: bool) {
        self.unsupported_row.set_visible(!supported);

        *self.profiles.borrow_mut() = profiles.to_vec();

        while let Some(child) = self.rows_box.first_child() {
            self.rows_box.remove(&child);
        }

        for (index, rule) in rules.iter().enumerate() {
            let row = self.build_rule_row(index, rule, profiles);
            self.rows_box.append(&row);
        }
    }

    fn build_rule_row(&self, index: usize, rule: &AppRule, profiles: &[String]) -> gtk::Box {
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_start(12)
            .margin_end(12)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        let entry = gtk::Entry::builder()
            .text(rule.process_names.join(", "))
            .placeholder_text("firefox, chrome")
            .hexpand(true)
            .build();

        {
            let store = self.store.clone();
            let profile = rule.profile.clone();
            let enabled = rule.enabled;
            entry.connect_activate(move |e| {
                let names: Vec<String> = e.text()
                    .split(',')
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                store.dispatch(Command::UpdateAppRule {
                    index,
                    process_names: names,
                    profile: profile.clone(),
                    enabled,
                });
            });
        }

        let profile_strings: Vec<&str> = profiles.iter().map(|s| s.as_str()).collect();
        let model = gtk::StringList::new(&profile_strings);
        let selected = profiles.iter().position(|p| p == &rule.profile).unwrap_or(0) as u32;
        let dropdown = gtk::DropDown::builder()
            .model(&model)
            .selected(selected)
            .build();

        {
            let store = self.store.clone();
            let process_names = rule.process_names.clone();
            let profiles = profiles.to_vec();
            let enabled = rule.enabled;
            dropdown.connect_selected_notify(move |dd| {
                let selected = dd.selected() as usize;
                let profile = profiles.get(selected).cloned().unwrap_or_default();
                store.dispatch(Command::UpdateAppRule {
                    index,
                    process_names: process_names.clone(),
                    profile,
                    enabled,
                });
            });
        }

        let enabled_switch = gtk::Switch::builder()
            .active(rule.enabled)
            .valign(gtk::Align::Center)
            .build();

        {
            let store = self.store.clone();
            let process_names = rule.process_names.clone();
            let profile = rule.profile.clone();
            enabled_switch.connect_active_notify(move |sw| {
                store.dispatch(Command::UpdateAppRule {
                    index,
                    process_names: process_names.clone(),
                    profile: profile.clone(),
                    enabled: sw.is_active(),
                });
            });
        }

        let delete_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .css_classes(["flat", "destructive-action"])
            .valign(gtk::Align::Center)
            .build();

        {
            let store = self.store.clone();
            delete_btn.connect_clicked(move |_| {
                store.dispatch(Command::RemoveAppRule { index });
            });
        }

        row_box.append(&entry);
        row_box.append(&dropdown);
        row_box.append(&enabled_switch);
        row_box.append(&delete_btn);
        row_box
    }
}
