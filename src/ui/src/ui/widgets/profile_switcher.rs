use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use crate::store::Store;
use halod_protocol::types::DEFAULT_PROFILE_NAME;

#[derive(Clone)]
pub struct ProfileSwitcher {
    pub root: gtk::MenuButton,
    label: gtk::Label,
    spinner: gtk::Spinner,
    chevron: gtk::Image,
    profile_list: gtk::ListBox,
    rename_row: adw::ActionRow,
    delete_row: adw::ActionRow,
    store: Store,
    active: Rc<RefCell<String>>,
    profile_rows: Rc<RefCell<Vec<(String, adw::ActionRow)>>>,
    popover: gtk::Popover,
    pending: Rc<RefCell<bool>>,
}

impl ProfileSwitcher {
    pub fn new(store: &Store) -> Self {
        // Header button label + chevron
        let label = gtk::Label::builder()
            .label(DEFAULT_PROFILE_NAME)
            .css_classes(["profile-label"])
            .build();

        let spinner = gtk::Spinner::new();
        spinner.set_visible(false);

        let btn_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .build();
        let chevron = gtk::Image::builder()
            .icon_name("pan-down-symbolic")
            .pixel_size(12)
            .build();
        btn_box.append(&label);
        btn_box.append(&spinner);
        btn_box.append(&chevron);

        // Profile list (rebuilt on every update)
        let profile_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_start(8)
            .margin_end(8)
            .margin_top(8)
            .build();

        // Action rows
        let new_row = adw::ActionRow::builder()
            .title("New Profile…")
            .activatable(true)
            .build();
        let new_icon = gtk::Image::builder()
            .icon_name("list-add-symbolic")
            .pixel_size(16)
            .build();
        new_row.add_prefix(&new_icon);

        let rename_row = adw::ActionRow::builder()
            .title("Rename…")
            .activatable(true)
            .build();
        let rename_icon = gtk::Image::builder()
            .icon_name("document-edit-symbolic")
            .pixel_size(16)
            .build();
        rename_row.add_prefix(&rename_icon);

        let delete_row = adw::ActionRow::builder()
            .title("Delete")
            .activatable(true)
            .css_classes(["error"])
            .build();
        let delete_icon = gtk::Image::builder()
            .icon_name("user-trash-symbolic")
            .pixel_size(16)
            .build();
        delete_row.add_prefix(&delete_icon);

        let actions_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_start(8)
            .margin_end(8)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        actions_list.append(&new_row);
        actions_list.append(&rename_row);
        actions_list.append(&delete_row);

        let popover_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(220)
            .build();
        popover_box.append(&profile_list);
        popover_box.append(&actions_list);

        let popover = gtk::Popover::builder()
            .child(&popover_box)
            .build();

        let root = gtk::MenuButton::builder()
            .popover(&popover)
            .child(&btn_box)
            .css_classes(["flat", "profile-btn"])
            .build();

        let active = Rc::new(RefCell::new(DEFAULT_PROFILE_NAME.to_string()));
        let pending = Rc::new(RefCell::new(false));

        // --- New Profile action ---
        let store_new = store.clone();
        let popover_new = popover.clone();
        new_row.connect_activated(move |_| {
            popover_new.popdown();
            show_name_dialog("New Profile", "Create", {
                let store = store_new.clone();
                move |name| {
                    store.dispatch(crate::commands::Command::AddProfile { name });
                }
            });
        });

        // --- Rename action ---
        let store_rename = store.clone();
        let active_rename = active.clone();
        let popover_rename = popover.clone();
        rename_row.connect_activated(move |_| {
            popover_rename.popdown();
            let old_name = active_rename.borrow().clone();
            show_name_dialog("Rename Profile", "Rename", {
                let store = store_rename.clone();
                move |new_name| {
                    store.dispatch(crate::commands::Command::RenameProfile { old_name: old_name.clone(), new_name });
                }
            });
        });

        // --- Delete action ---
        let store_del = store.clone();
        let active_del = active.clone();
        let popover_del = popover.clone();
        delete_row.connect_activated(move |_| {
            popover_del.popdown();
            let name = active_del.borrow().clone();
            show_confirm_dialog(
                &format!("Delete profile \"{}\"?", name),
                "Delete",
                {
                    let store = store_del.clone();
                    move || {
                        store.dispatch(crate::commands::Command::RemoveProfile { name: name.clone() });
                    }
                },
            );
        });

        ProfileSwitcher {
            root,
            label,
            spinner,
            chevron,
            profile_list,
            rename_row,
            delete_row,
            store: store.clone(),
            active,
            profile_rows: Rc::new(RefCell::new(Vec::new())),
            popover,
            pending,
        }
    }

    pub fn update(&self, active: &str, profiles: &[String]) {
        *self.pending.borrow_mut() = false;
        self.spinner.stop();
        self.spinner.set_visible(false);
        self.chevron.set_visible(true);

        *self.active.borrow_mut() = active.to_string();
        self.label.set_text(active);

        let is_default = active == DEFAULT_PROFILE_NAME;
        self.rename_row.set_sensitive(!is_default);
        self.delete_row.set_sensitive(!is_default);

        // Remove old profile rows
        {
            let mut rows = self.profile_rows.borrow_mut();
            for (_, row) in rows.drain(..) {
                self.profile_list.remove(&row);
            }
        }

        // Add current profile rows
        let mut new_rows = Vec::new();
        for name in profiles {
            let row = adw::ActionRow::builder()
                .title(name.as_str())
                .activatable(true)
                .build();

            if name == active {
                let check = gtk::Image::builder()
                    .icon_name("object-select-symbolic")
                    .pixel_size(16)
                    .build();
                row.add_suffix(&check);
            }

            let store = self.store.clone();
            let profile_name = name.clone();
            let popover = self.popover.clone();
            let pending = self.pending.clone();
            let spinner = self.spinner.clone();
            let chevron = self.chevron.clone();
            let current_active = self.active.clone();
            row.connect_activated(move |_| {
                popover.popdown();
                if profile_name != *current_active.borrow() {
                    *pending.borrow_mut() = true;
                    spinner.start();
                    spinner.set_visible(true);
                    chevron.set_visible(false);
                }
                store.dispatch(crate::commands::Command::SwitchProfile { name: profile_name.clone() });
            });

            self.profile_list.append(&row);
            new_rows.push((name.clone(), row));
        }
        *self.profile_rows.borrow_mut() = new_rows;
    }
}

fn show_name_dialog(title: &str, action_label: &str, on_confirm: impl Fn(String) + 'static) {
    let dialog = adw::AlertDialog::builder()
        .heading(title)
        .build();

    let entry = gtk::Entry::builder()
        .placeholder_text("Profile name")
        .activates_default(true)
        .margin_top(8)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();

    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", action_label);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");

    dialog.connect_response(None, move |_, response| {
        if response == "ok" {
            let name = entry.text().to_string();
            let name = name.trim().to_string();
            if !name.is_empty() {
                on_confirm(name);
            }
        }
    });

    dialog.present(None::<&gtk::Widget>);
}

fn show_confirm_dialog(heading: &str, action_label: &str, on_confirm: impl Fn() + 'static) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .build();

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("ok", action_label);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    dialog.connect_response(None, move |_, response| {
        if response == "ok" {
            on_confirm();
        }
    });

    dialog.present(None::<&gtk::Widget>);
}
