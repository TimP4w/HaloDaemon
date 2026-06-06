use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use halod_protocol::types::RunningApp;
use libadwaita as adw;

use crate::commands::Command;
use crate::state::AppRule;
use crate::store::Store;

#[derive(Clone)]
pub struct AppRulesPage {
    pub root: gtk::Box,
    store: Store,
    rules_group: adw::PreferencesGroup,
    rule_rows: Rc<RefCell<Vec<adw::PreferencesRow>>>,
    draft_row: Rc<RefCell<Option<adw::PreferencesRow>>>,
    add_row: adw::ActionRow,
    unsupported_group: adw::PreferencesGroup,
    profiles: Rc<RefCell<Vec<String>>>,
}

impl AppRulesPage {
    pub fn new(store: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let clamp = adw::Clamp::builder().maximum_size(700).build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_start(32)
            .margin_end(32)
            .margin_top(28)
            .margin_bottom(32)
            .build();

        let unsupported_group = adw::PreferencesGroup::new();
        let unsupported_row = adw::ActionRow::builder()
            .title("Focus tracking not available")
            .subtitle("Install and enable the HaloDaemon GNOME Shell extension")
            .sensitive(false)
            .build();
        unsupported_group.add(&unsupported_row);
        unsupported_group.set_visible(false);

        let rules_group = adw::PreferencesGroup::builder()
            .title("App Rules")
            .description("Automatically switch profiles based on the foreground application")
            .build();

        let add_row = adw::ActionRow::builder()
            .title("Add Rule")
            .activatable(true)
            .build();
        let add_icon = gtk::Image::builder()
            .icon_name("list-add-symbolic")
            .pixel_size(16)
            .build();
        add_row.add_prefix(&add_icon);
        rules_group.add(&add_row);

        content.append(&unsupported_group);
        content.append(&rules_group);
        clamp.set_child(Some(&content));
        scroll.set_child(Some(&clamp));
        root.append(&scroll);

        let rule_rows: Rc<RefCell<Vec<adw::PreferencesRow>>> = Rc::new(RefCell::new(Vec::new()));
        let draft_row: Rc<RefCell<Option<adw::PreferencesRow>>> = Rc::new(RefCell::new(None));
        let profiles: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

        {
            let store_c = store.clone();
            let rules_group_c = rules_group.clone();
            let add_row_c = add_row.clone();
            let draft_row_c = draft_row.clone();
            let profiles_c = profiles.clone();
            add_row.connect_activated(move |_| {
                if draft_row_c.borrow().is_some() {
                    return;
                }
                let prf = profiles_c.borrow().clone();
                let draft =
                    build_draft_row(&store_c, &rules_group_c, &add_row_c, &draft_row_c, &prf);
                rules_group_c.remove(&add_row_c);
                rules_group_c.add(&draft);
                rules_group_c.add(&add_row_c);
                *draft_row_c.borrow_mut() = Some(draft);
            });
        }

        AppRulesPage {
            root,
            store: store.clone(),
            rules_group,
            rule_rows,
            draft_row,
            add_row,
            unsupported_group,
            profiles,
        }
    }

    pub fn update(&self, rules: &[AppRule], profiles: &[String], supported: bool) {
        self.unsupported_group.set_visible(!supported);
        *self.profiles.borrow_mut() = profiles.to_vec();

        {
            let mut rows = self.rule_rows.borrow_mut();
            for row in rows.drain(..) {
                self.rules_group.remove(&row);
            }
        }

        if let Some(draft) = self.draft_row.borrow_mut().take() {
            self.rules_group.remove(&draft);
        }

        self.rules_group.remove(&self.add_row);

        {
            let mut rows = self.rule_rows.borrow_mut();
            for (index, rule) in rules.iter().enumerate() {
                let row = build_rule_row(index, rule, profiles, &self.store);
                self.rules_group.add(&row);
                rows.push(row);
            }
        }

        self.rules_group.add(&self.add_row);
    }
}

fn add_process_tag(flow: &gtk::FlowBox, name: &str, on_remove: impl Fn() + 'static) {
    let btn = gtk::Button::new();
    btn.set_widget_name(name);
    btn.add_css_class("process-tag");

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .valign(gtk::Align::Center)
        .build();
    let label = gtk::Label::new(Some(name));
    label.set_valign(gtk::Align::Center);
    inner.append(&label);
    let x_icon = gtk::Image::from_icon_name("window-close-symbolic");
    x_icon.set_pixel_size(10);
    x_icon.set_valign(gtk::Align::Center);
    inner.append(&x_icon);
    btn.set_child(Some(&inner));

    let flow_weak = flow.downgrade();
    btn.connect_clicked(move |btn| {
        if let Some(f) = flow_weak.upgrade() {
            if let Some(parent) = btn.parent() {
                f.remove(&parent);
            }
        }
        on_remove();
    });

    let child = gtk::FlowBoxChild::new();
    child.set_focusable(false);
    child.set_child(Some(&btn));
    flow.append(&child);
}

fn collect_flow_names(flow: &gtk::FlowBox) -> Vec<String> {
    let mut names = Vec::new();
    let mut w = flow.first_child();
    while let Some(widget) = w {
        let next = widget.next_sibling();
        if let Some(fbc) = widget.downcast_ref::<gtk::FlowBoxChild>() {
            if let Some(btn) = fbc.child().and_then(|c| c.downcast::<gtk::Button>().ok()) {
                let n = btn.widget_name().to_string();
                if !n.is_empty() {
                    names.push(n);
                }
            }
        }
        w = next;
    }
    names
}

fn build_rule_row(
    index: usize,
    rule: &AppRule,
    profiles: &[String],
    store: &Store,
) -> adw::PreferencesRow {
    let row = adw::PreferencesRow::new();

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(false)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .row_spacing(4)
        .column_spacing(4)
        .build();

    for name in &rule.process_names {
        let store_r = store.clone();
        let profile_r = rule.profile.clone();
        let enabled_r = rule.enabled;
        let flow_r = flow.clone();
        add_process_tag(&flow, name, move || {
            let names = collect_flow_names(&flow_r);
            if !names.is_empty() {
                store_r.dispatch(Command::UpdateAppRule {
                    index,
                    process_names: names,
                    profile: profile_r.clone(),
                    enabled: enabled_r,
                });
            }
        });
    }

    let profile_strings: Vec<&str> = profiles.iter().map(|s| s.as_str()).collect();
    let model = gtk::StringList::new(&profile_strings);
    let selected = profiles
        .iter()
        .position(|p| p == &rule.profile)
        .unwrap_or(0) as u32;
    let dropdown = gtk::DropDown::builder()
        .model(&model)
        .selected(selected)
        .valign(gtk::Align::Center)
        .build();

    {
        let store = store.clone();
        let process_names = rule.process_names.clone();
        let profiles = profiles.to_vec();
        let enabled = rule.enabled;
        dropdown.connect_selected_notify(move |dd| {
            let sel = dd.selected() as usize;
            let profile = profiles.get(sel).cloned().unwrap_or_default();
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
        let store = store.clone();
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
        let store = store.clone();
        delete_btn.connect_clicked(move |_| {
            store.dispatch(Command::RemoveAppRule { index });
        });
    }

    let pick_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text("Add process")
        .build();

    {
        let flow_p = flow.clone();
        let store_p = store.clone();
        let profile_p = rule.profile.clone();
        let enabled_p = rule.enabled;
        pick_btn.connect_clicked(move |_| {
            let flow = flow_p.clone();
            let store = store_p.clone();
            let store_picker = store_p.clone();
            let profile = profile_p.clone();
            show_process_picker(store_picker.clone(), move |name| {
                if collect_flow_names(&flow).contains(&name) {
                    return;
                }
                let flow_r = flow.clone();
                let store_r = store.clone();
                let profile_r = profile.clone();
                add_process_tag(&flow, &name, move || {
                    let names = collect_flow_names(&flow_r);
                    if !names.is_empty() {
                        store_r.dispatch(Command::UpdateAppRule {
                            index,
                            process_names: names,
                            profile: profile_r.clone(),
                            enabled: enabled_p,
                        });
                    }
                });
                let names = collect_flow_names(&flow);
                store.dispatch(Command::UpdateAppRule {
                    index,
                    process_names: names,
                    profile: profile.clone(),
                    enabled: enabled_p,
                });
            });
        });
    }

    row_box.append(&flow);
    row_box.append(&pick_btn);
    row_box.append(&dropdown);
    row_box.append(&enabled_switch);
    row_box.append(&delete_btn);
    row.set_child(Some(&row_box));
    row
}

fn build_draft_row(
    store: &Store,
    rules_group: &adw::PreferencesGroup,
    add_row: &adw::ActionRow,
    draft_slot: &Rc<RefCell<Option<adw::PreferencesRow>>>,
    profiles: &[String],
) -> adw::PreferencesRow {
    let row = adw::PreferencesRow::new();

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(false)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .row_spacing(4)
        .column_spacing(4)
        .build();

    let profile_strings: Vec<&str> = profiles.iter().map(|s| s.as_str()).collect();
    let model = gtk::StringList::new(&profile_strings);
    let dropdown = gtk::DropDown::builder()
        .model(&model)
        .selected(0)
        .valign(gtk::Align::Center)
        .build();

    let confirm_btn = gtk::Button::builder()
        .icon_name("object-select-symbolic")
        .css_classes(["flat", "suggested-action"])
        .valign(gtk::Align::Center)
        .tooltip_text("Add rule")
        .build();

    let discard_btn = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text("Discard")
        .build();

    {
        let store = store.clone();
        let rules_group = rules_group.clone();
        let add_row = add_row.clone();
        let draft_slot = draft_slot.clone();
        let flow_c = flow.clone();
        let dropdown_c = dropdown.clone();
        let profiles = profiles.to_vec();
        confirm_btn.connect_clicked(move |_| {
            let names = collect_flow_names(&flow_c);
            if names.is_empty() {
                return;
            }
            let sel = dropdown_c.selected() as usize;
            let profile = profiles.get(sel).cloned().unwrap_or_default();
            store.dispatch(Command::AddAppRule {
                process_names: names,
                profile,
                enabled: true,
            });
            if let Some(draft) = draft_slot.borrow_mut().take() {
                rules_group.remove(&draft);
                rules_group.remove(&add_row);
                rules_group.add(&add_row);
            }
        });
    }

    {
        let rules_group = rules_group.clone();
        let add_row = add_row.clone();
        let draft_slot = draft_slot.clone();
        discard_btn.connect_clicked(move |_| {
            if let Some(draft) = draft_slot.borrow_mut().take() {
                rules_group.remove(&draft);
                rules_group.remove(&add_row);
                rules_group.add(&add_row);
            }
        });
    }

    let pick_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text("Add process")
        .build();

    {
        let flow_p = flow.clone();
        let store_p = store.clone();
        pick_btn.connect_clicked(move |_| {
            let flow = flow_p.clone();
            show_process_picker(store_p.clone(), move |name| {
                if !collect_flow_names(&flow).contains(&name) {
                    add_process_tag(&flow, &name, || {});
                }
            });
        });
    }

    row_box.append(&flow);
    row_box.append(&pick_btn);
    row_box.append(&dropdown);
    row_box.append(&confirm_btn);
    row_box.append(&discard_btn);
    row.set_child(Some(&row_box));
    row
}

fn show_process_picker(store: Store, on_select: impl Fn(String) + 'static) {
    let on_select = Rc::new(on_select);
    store.request_running_apps(move |apps| {
        open_picker_dialog(apps, on_select.clone());
    });
}

fn matches_filter(query: &str, title: &str, subtitle: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    title.to_lowercase().contains(&q) || subtitle.to_lowercase().contains(&q)
}

fn open_picker_dialog(entries: Vec<RunningApp>, on_select: Rc<dyn Fn(String)>) {
    let search = gtk::SearchEntry::new();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .build();

    let search_c = search.clone();
    list.set_filter_func(move |row| {
        let q = search_c.text();
        row.downcast_ref::<adw::ActionRow>()
            .map(|r| {
                let sub = r.subtitle().map(|s| s.to_string()).unwrap_or_default();
                matches_filter(&q, &r.title(), &sub)
            })
            .unwrap_or(true)
    });
    let list_f = list.clone();
    search.connect_search_changed(move |_| list_f.invalidate_filter());

    let dialog = adw::AlertDialog::builder().heading("Pick Process").build();
    dialog.add_response("cancel", "Cancel");
    dialog.set_close_response("cancel");

    let dialog_weak = dialog.downgrade();

    for entry in &entries {
        let row = adw::ActionRow::builder()
            .title(&entry.display_name)
            .activatable(true)
            .build();

        if entry.display_name != entry.process_name {
            row.set_subtitle(&entry.process_name);
        }

        let img = if entry.icon_name.is_empty() {
            gtk::Image::from_icon_name("application-x-executable-symbolic")
        } else if std::path::Path::new(&entry.icon_name).is_absolute() {
            let file = gtk::gio::File::for_path(&entry.icon_name);
            let gicon = gtk::gio::FileIcon::new(&file);
            gtk::Image::from_gicon(&gicon)
        } else {
            gtk::Image::from_icon_name(&entry.icon_name)
        };
        img.set_pixel_size(32);
        row.add_prefix(&img);

        let on_select_c = on_select.clone();
        let proc_name = entry.process_name.clone();
        let dialog_weak_c = dialog_weak.clone();
        row.connect_activated(move |_| {
            on_select_c(proc_name.clone());
            if let Some(d) = dialog_weak_c.upgrade() {
                d.close();
            }
        });

        list.append(&row);
    }

    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(200)
        .max_content_height(350)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    scroll.set_child(Some(&list));

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(4)
        .build();
    vbox.append(&search);
    vbox.append(&scroll);

    dialog.set_extra_child(Some(&vbox));

    let parent = gtk::gio::Application::default()
        .and_then(|a| a.downcast::<gtk::Application>().ok())
        .and_then(|a| a.active_window());
    dialog.present(parent.as_ref().map(|w| w.upcast_ref::<gtk::Widget>()));
}

#[cfg(test)]
mod tests {
    use super::matches_filter;

    #[test]
    fn empty_query_always_matches() {
        assert!(matches_filter("", "Firefox", "firefox"));
    }

    #[test]
    fn matches_title_case_insensitive() {
        assert!(matches_filter("fire", "Firefox", ""));
    }

    #[test]
    fn matches_subtitle_case_insensitive() {
        assert!(matches_filter("fox", "", "firefox-esr"));
    }

    #[test]
    fn no_match_returns_false() {
        assert!(!matches_filter("chrome", "Firefox", "firefox"));
    }
}
