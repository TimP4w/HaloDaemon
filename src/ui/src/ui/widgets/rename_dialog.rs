//! Per-device rename dialog. Invoked from the home-card right-click menu and
//! from the pencil button on the device detail page. Sends the unified
//! `set_device_name` IPC; the broadcast loop in main.rs refreshes the UI.

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;
use serde_json::json;

use crate::store::Store;

const MAX_NAME_LEN: u32 = 64;

pub fn open_rename_dialog(
    parent: Option<&gtk::Window>,
    store: &Store,
    device_id: &str,
    current_name: &str,
) {
    let dialog = adw::MessageDialog::builder()
        .heading("Rename device")
        .body("Leave blank to restore the default name.")
        .modal(true)
        .build();
    if let Some(p) = parent {
        dialog.set_transient_for(Some(p));
    }

    let entry = gtk::Entry::builder()
        .text(current_name)
        .max_length(MAX_NAME_LEN as i32)
        .activates_default(true)
        .build();
    dialog.set_extra_child(Some(&entry));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);

    let store = store.clone();
    let device_id = device_id.to_string();
    dialog.connect_response(None, move |dlg, response| {
        if response == "save" {
            store.dispatch(crate::commands::Command::CanvasOp(json!({
                "type": "set_device_name",
                "device_id": device_id,
                "name": entry.text().to_string(),
            })));
        }
        dlg.close();
    });

    dialog.present();
}
