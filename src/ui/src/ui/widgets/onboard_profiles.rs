use std::cell::RefCell;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use halod_protocol::types::OnboardProfiles;

/// Onboard (on-device) profile management: switch the active profile, restore a
/// slot to ROM factory defaults, and enable/disable slots.
///
/// Every control is a plain button — there are no user-controlled live widgets
/// (sliders/dropdowns/switches), so `update_live` is free to rebuild the row
/// list when the device reports a changed profile state.
pub struct OnboardProfilesWidget {
    pub root: gtk::Box,
    list: gtk::ListBox,
    device_id: String,
    store: Store,
    last: RefCell<OnboardProfiles>,
}

impl OnboardProfilesWidget {
    pub fn build(device_id: &str, profiles: &OnboardProfiles, store: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(8)
            .build();

        let title = gtk::Label::builder()
            .label("Onboard Profiles")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        root.append(&title);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        root.append(&list);

        populate(&list, device_id, store, profiles);
        // Host mode (active_slot == 0): the device is not running any onboard
        // profile, so the editor is meaningless — hide it until the user
        // switches back to onboard mode.
        root.set_visible(profiles.active_slot != 0);

        Self {
            root,
            list,
            device_id: device_id.to_string(),
            store: store.clone(),
            last: RefCell::new(profiles.clone()),
        }
    }

    /// Rebuild the slot list when the device reports a changed profile state.
    pub fn update_live(&self, profiles: &OnboardProfiles) {
        self.root.set_visible(profiles.active_slot != 0);
        if *self.last.borrow() == *profiles {
            return;
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        populate(&self.list, &self.device_id, &self.store, profiles);
        *self.last.borrow_mut() = profiles.clone();
    }
}

/// Append one `adw::ActionRow` per profile slot to `list`.
fn populate(list: &gtk::ListBox, device_id: &str, store: &Store, profiles: &OnboardProfiles) {
    for slot in &profiles.slots {
        let subtitle = if slot.active {
            "Active"
        } else if slot.enabled {
            "Enabled"
        } else {
            "Disabled"
        };
        let row = adw::ActionRow::builder()
            .title(format!("Profile {}", slot.index))
            .subtitle(subtitle)
            .build();
        // A disabled slot reads as greyed out across the whole row; the action
        // buttons stay clickable so it can still be added/restored.
        if !slot.enabled {
            row.add_css_class("dim-label");
        }

        // Status dot: a small filled circle. Accent-coloured for the active
        // profile, plain (white) for an enabled slot, dimmed via the row when
        // the slot is disabled — same three-step hierarchy as the row text.
        let dot = gtk::Image::builder()
            .icon_name("media-record-symbolic")
            .pixel_size(12)
            .build();
        if slot.active {
            dot.add_css_class("accent");
        }
        row.add_prefix(&dot);

        // Switch — make this slot the active profile.
        let switch_btn = gtk::Button::builder()
            .label("Switch")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("Make this the active profile")
            .sensitive(slot.enabled && !slot.active)
            .build();
        {
            let store = store.clone();
            let dev = device_id.to_string();
            let idx = slot.index;
            switch_btn.connect_clicked(move |_| {
                store.dispatch(crate::commands::Command::OnboardProfileSwitch(
                    serde_json::json!({
                        "type": "onboard_profile_switch",
                        "id": dev,
                        "slot": idx,
                    })
                ));
            });
        }
        row.add_suffix(&switch_btn);

        // Restore — overwrite this slot with the device's ROM defaults. Only
        // offered for slots backed by a factory ROM profile, and only after the
        // user confirms (it discards the slot's current settings).
        if slot.has_rom_default {
            let restore_btn = gtk::Button::builder()
                .icon_name("edit-undo-symbolic")
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .tooltip_text("Restore factory defaults from device ROM")
                .build();
            {
                let store = store.clone();
                let dev = device_id.to_string();
                let idx = slot.index;
                restore_btn.connect_clicked(move |btn| {
                    let dialog = adw::AlertDialog::new(
                        Some("Restore profile?"),
                        Some(&format!(
                            "Restore Profile {idx} to factory defaults? \
                             This overwrites the slot's current settings."
                        )),
                    );
                    dialog.add_response("cancel", "Cancel");
                    dialog.add_response("restore", "Restore");
                    dialog.set_response_appearance(
                        "restore",
                        adw::ResponseAppearance::Destructive,
                    );
                    dialog.set_default_response(Some("cancel"));
                    dialog.set_close_response("cancel");
                    {
                        let store = store.clone();
                        let dev = dev.clone();
                        dialog.connect_response(None, move |_, resp| {
                            if resp == "restore" {
                                store.dispatch(crate::commands::Command::OnboardProfileRestore(
                                    serde_json::json!({
                                        "type": "onboard_profile_restore",
                                        "id": dev,
                                        "slot": idx,
                                    })
                                ));
                            }
                        });
                    }
                    dialog.present(Some(btn));
                });
            }
            row.add_suffix(&restore_btn);
        }

        // Enable/disable — add or remove the slot in the profile directory.
        let toggle_btn = gtk::Button::builder()
            .icon_name(if slot.enabled {
                "list-remove-symbolic"
            } else {
                "list-add-symbolic"
            })
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .tooltip_text(if slot.enabled {
                "Disable this profile slot"
            } else {
                "Add this profile slot"
            })
            // Don't allow disabling the active profile — the device needs one.
            .sensitive(!(slot.enabled && slot.active))
            .build();
        {
            let store = store.clone();
            let dev = device_id.to_string();
            let idx = slot.index;
            let enable = !slot.enabled;
            toggle_btn.connect_clicked(move |_| {
                store.dispatch(crate::commands::Command::OnboardProfileSetEnabled(
                    serde_json::json!({
                        "type": "onboard_profile_set_enabled",
                        "id": dev,
                        "slot": idx,
                        "enabled": enable,
                    })
                ));
            });
        }
        row.add_suffix(&toggle_btn);

        list.append(&row);
    }
}
