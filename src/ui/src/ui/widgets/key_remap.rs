//! Key-remap capability UI.
//!
//! Renders one row per remappable button reported by the device. Each row shows
//! a summary of the configured action; an Edit button opens a modal dialog that
//! lets the user pick a base action and a Layer Shift action. Every row control is a
//! plain button — there are no user-controlled live widgets — so `update_live`
//! is free to rebuild the whole list when the device reports changed mappings.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use crate::state::AppState;
use crate::ui::capability_registry::CapabilityPanel;
use halod_protocol::types::{
    ButtonAction, ButtonMapping, CycleDir, DeviceCapability, KeyRemapStatus, MacroAtom, MacroStep,
    MediaAction, ModKey, MouseBtn, ScrollAxis, WireDevice,
};

// ── Action type table ───────────────────────────────────────────────────────

const ACTION_TYPES: &[&str] = &[
    "Native (firmware default)",
    "Disabled",
    "Mouse button",
    "Scroll wheel",
    "Keyboard shortcut",
    "Media key",
    "Cycle DPI",
    "Cycle profile",
    "Momentary DPI",
    "Layer Shift modifier",
    "Macro",
    "Open application",
    "Run command",
];

fn action_type_index(a: &ButtonAction) -> u32 {
    match a {
        ButtonAction::Native => 0,
        ButtonAction::Disable => 1,
        ButtonAction::MouseButton { .. } => 2,
        ButtonAction::Scroll { .. } => 3,
        ButtonAction::KeyChord { .. } => 4,
        ButtonAction::MediaKey { .. } => 5,
        ButtonAction::DpiCycle { .. } => 6,
        ButtonAction::ProfileCycle { .. } => 7,
        ButtonAction::MomentaryDpi { .. } => 8,
        ButtonAction::LayerShift => 9,
        ButtonAction::Macro { .. } => 10,
        ButtonAction::OpenApp { .. } => 11,
        ButtonAction::Command { .. } => 12,
    }
}

const MOUSE_BTNS: &[&str] = &["Left", "Right", "Middle", "Back", "Forward"];
const SCROLL_AXES: &[&str] = &["Vertical", "Horizontal"];
const MEDIA_KEYS: &[&str] = &["Volume up", "Volume down", "Mute", "Play / Pause", "Next", "Previous"];
const CYCLE_DIRS: &[&str] = &["Up", "Down"];
const MACRO_ATOMS: &[&str] = &["Key down", "Key up", "Mouse down", "Mouse up", "Delay"];

fn mouse_btn_from_idx(i: u32) -> MouseBtn {
    match i {
        0 => MouseBtn::Left,
        1 => MouseBtn::Right,
        2 => MouseBtn::Middle,
        3 => MouseBtn::Back,
        _ => MouseBtn::Forward,
    }
}
fn mouse_btn_to_idx(b: &MouseBtn) -> u32 {
    match b {
        MouseBtn::Left => 0,
        MouseBtn::Right => 1,
        MouseBtn::Middle => 2,
        MouseBtn::Back => 3,
        MouseBtn::Forward => 4,
    }
}
fn axis_from_idx(i: u32) -> ScrollAxis {
    if i == 1 { ScrollAxis::Horizontal } else { ScrollAxis::Vertical }
}
fn media_from_idx(i: u32) -> MediaAction {
    match i {
        0 => MediaAction::VolumeUp,
        1 => MediaAction::VolumeDown,
        2 => MediaAction::Mute,
        3 => MediaAction::Play,
        4 => MediaAction::Next,
        _ => MediaAction::Prev,
    }
}
fn media_to_idx(m: &MediaAction) -> u32 {
    match m {
        MediaAction::VolumeUp => 0,
        MediaAction::VolumeDown => 1,
        MediaAction::Mute => 2,
        MediaAction::Play => 3,
        MediaAction::Next => 4,
        MediaAction::Prev => 5,
    }
}
fn dir_from_idx(i: u32) -> CycleDir {
    if i == 1 { CycleDir::Down } else { CycleDir::Up }
}
fn dir_to_idx(d: &CycleDir) -> u32 {
    match d {
        CycleDir::Up => 0,
        CycleDir::Down => 1,
    }
}

/// Short one-line description shown in a button row's subtitle.
fn action_summary(a: &ButtonAction) -> String {
    match a {
        ButtonAction::Native => "Firmware default".to_string(),
        ButtonAction::Disable => "Disabled".to_string(),
        ButtonAction::MouseButton { btn } => format!("Mouse {}", MOUSE_BTNS[mouse_btn_to_idx(btn) as usize]),
        ButtonAction::Scroll { axis, clicks } => {
            format!("Scroll {} ({clicks:+})", SCROLL_AXES[if matches!(axis, ScrollAxis::Horizontal) { 1 } else { 0 }])
        }
        ButtonAction::KeyChord { key, modifiers } => {
            let mods: String = modifiers
                .iter()
                .map(|m| format!("{}+", mod_label(m)))
                .collect();
            format!("Shortcut {mods}#{key}")
        }
        ButtonAction::MediaKey { key } => format!("Media: {}", MEDIA_KEYS[media_to_idx(key) as usize]),
        ButtonAction::DpiCycle { direction } => format!("Cycle DPI {}", CYCLE_DIRS[dir_to_idx(direction) as usize]),
        ButtonAction::ProfileCycle { direction } => {
            format!("Cycle profile {}", CYCLE_DIRS[dir_to_idx(direction) as usize])
        }
        ButtonAction::MomentaryDpi { dpi } => format!("Hold for {dpi} DPI"),
        ButtonAction::LayerShift => "Layer Shift modifier".to_string(),
        ButtonAction::Macro { steps } => format!("Macro · {} step(s)", steps.len()),
        ButtonAction::OpenApp { path } => format!("Open {path}"),
        ButtonAction::Command { cmd, .. } => format!("Run {cmd}"),
    }
}

fn mod_label(m: &ModKey) -> &'static str {
    match m {
        ModKey::Ctrl => "Ctrl",
        ModKey::Shift => "Shift",
        ModKey::Alt => "Alt",
        ModKey::Super => "Super",
    }
}

// ── Panel ───────────────────────────────────────────────────────────────────

pub struct KeyRemapWidget {
    root: gtk::Box,
    banner: adw::Banner,
    list: gtk::ListBox,
    device_id: String,
    store: Store,
    /// Serialized snapshot of the last rendered status — compared on every
    /// broadcast so the row list is rebuilt only when the device's buttons or
    /// mappings actually change.
    last_sig: RefCell<String>,
}

impl KeyRemapWidget {
    pub fn build(
        device_id: &str,
        status: &KeyRemapStatus,
        device: &WireDevice,
        store: &Store,
    ) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(16)
            .build();

        let banner = adw::Banner::builder()
            .title("Key remapping needs Host Mode — enable it on the Controls tab.")
            .build();
        root.append(&banner);

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        let title = gtk::Label::builder()
            .label("Remappable Buttons")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .css_classes(["heading"])
            .build();
        let reset_all = gtk::Button::builder()
            .label("Reset all")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        header.append(&title);
        header.append(&reset_all);
        root.append(&header);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        root.append(&list);

        {
            let store = store.clone();
            let dev = device_id.to_string();
            reset_all.connect_clicked(move |btn| {
                let dialog = adw::AlertDialog::new(
                    Some("Reset all button mappings?"),
                    Some("Every button returns to its firmware default."),
                );
                dialog.add_response("cancel", "Cancel");
                dialog.add_response("reset", "Reset All");
                dialog.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
                dialog.set_default_response(Some("cancel"));
                dialog.set_close_response("cancel");
                {
                    let store = store.clone();
                    let dev = dev.clone();
                    dialog.connect_response(None, move |_, resp| {
                        if resp == "reset" {
                            store.dispatch(crate::commands::Command::ResetAllButtonMappings {
                                device_id: dev.clone(),
                            });
                        }
                    });
                }
                dialog.present(Some(btn));
            });
        }

        let widget = Self {
            root,
            banner,
            list,
            device_id: device_id.to_string(),
            store: store.clone(),
            last_sig: RefCell::new(String::new()),
        };
        widget.render(status, device);
        widget
    }

    /// Clear and repopulate the button list from `status`.
    fn render(&self, status: &KeyRemapStatus, device: &WireDevice) {
        *self.last_sig.borrow_mut() = serde_json::to_string(status).unwrap_or_default();

        let host_ok = !status.requires_host_mode || status.host_mode_active;
        self.banner.set_revealed(status.requires_host_mode && !status.host_mode_active);

        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        // DPI values the device accepts — used to bound the Momentary DPI editor.
        let available_dpis: Vec<u16> = device
            .capabilities
            .iter()
            .find_map(|c| match c {
                DeviceCapability::Dpi(s) => Some(s.available_dpis.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let available_dpis = Rc::new(available_dpis);

        if status.buttons.is_empty() {
            let row = adw::ActionRow::builder()
                .title("No remappable buttons")
                .subtitle("This device does not report any reprogrammable controls.")
                .build();
            self.list.append(&row);
            return;
        }

        for btn in &status.buttons {
            let mapping = status.mappings.iter().find(|m| m.cid == btn.cid);
            let base = mapping.map(|m| m.base.clone()).unwrap_or_default();
            let shifted = mapping.map(|m| m.shifted.clone()).unwrap_or_default();

            let row = adw::ActionRow::builder().title(&btn.label).build();

            let subtitle = if !btn.divertable {
                "Not remappable".to_string()
            } else {
                let mut s = action_summary(&base);
                if !matches!(shifted, ButtonAction::Native) {
                    s = format!("{s}    ·    Layer Shift: {}", action_summary(&shifted));
                }
                s
            };
            row.set_subtitle(&subtitle);
            if !btn.divertable {
                row.add_css_class("dim-label");
            }

            if btn.divertable {
                if mapping.is_some() {
                    let reset = gtk::Button::builder()
                        .icon_name("edit-clear-symbolic")
                        .css_classes(["flat", "circular"])
                        .valign(gtk::Align::Center)
                        .tooltip_text("Reset this button to default")
                        .build();
                    let store = self.store.clone();
                    let dev = self.device_id.clone();
                    let cid = btn.cid;
                    reset.connect_clicked(move |_| {
                        store.dispatch(crate::commands::Command::ResetButtonMapping {
                            device_id: dev.clone(),
                            cid,
                        });
                    });
                    row.add_suffix(&reset);
                }

                let edit = gtk::Button::builder()
                    .icon_name("document-edit-symbolic")
                    .css_classes(["flat", "circular"])
                    .valign(gtk::Align::Center)
                    .tooltip_text("Edit mapping")
                    .sensitive(host_ok)
                    .build();
                {
                    let store = self.store.clone();
                    let dev = self.device_id.clone();
                    let cid = btn.cid;
                    let label = btn.label.clone();
                    let dpis = available_dpis.clone();
                    edit.connect_clicked(move |b| {
                        open_edit_dialog(b, &dev, &store, cid, &label, &base, &shifted, &dpis);
                    });
                }
                row.add_suffix(&edit);
            }

            self.list.append(&row);
        }
    }
}

impl CapabilityPanel for KeyRemapWidget {
    fn root_widget(&self) -> gtk::Widget {
        self.root.clone().upcast()
    }
    fn tab_label(&self) -> &'static str {
        "Buttons"
    }
    fn tab_icon(&self) -> &'static str {
        "input-mouse-symbolic"
    }
    fn tab_name(&self) -> &'static str {
        "buttons"
    }
    fn update_live(&self, state: &AppState) {
        let Some(device) = state.devices.iter().find(|d| d.id == self.device_id) else { return };
        for cap in &device.capabilities {
            if let DeviceCapability::KeyRemap(status) = cap {
                let sig = serde_json::to_string(status).unwrap_or_default();
                if *self.last_sig.borrow() != sig {
                    self.render(status, device);
                }
                return;
            }
        }
    }
}

// ── Edit dialog ─────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn open_edit_dialog(
    parent: &impl IsA<gtk::Widget>,
    device_id: &str,
    store: &Store,
    cid: u16,
    label: &str,
    base: &ButtonAction,
    shifted: &ButtonAction,
    available_dpis: &[u16],
) {
    let dialog = adw::Dialog::builder()
        .title(format!("Remap · {label}"))
        .content_width(470)
        .content_height(640)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let save = gtk::Button::builder()
        .label("Save")
        .css_classes(["suggested-action"])
        .build();
    header.pack_end(&save);
    toolbar.add_top_bar(&header);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(16)
        .margin_bottom(16)
        .margin_start(16)
        .margin_end(16)
        .build();

    let base_editor = ActionEditor::build("Action", base, available_dpis);
    let shift_editor = ActionEditor::build("Layer Shift action", shifted, available_dpis);
    content.append(&base_editor.root);
    content.append(&shift_editor.root);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));

    {
        let store = store.clone();
        let device_id = device_id.to_string();
        let dialog = dialog.clone();
        save.connect_clicked(move |_| {
            let base = base_editor.value();
            let shifted = shift_editor.value();
            if matches!((&base, &shifted), (ButtonAction::Native, ButtonAction::Native)) {
                store.dispatch(crate::commands::Command::ResetButtonMapping {
                    device_id: device_id.clone(),
                    cid,
                });
            } else {
                let mapping = serde_json::json!({
                    "type": "set_button_mapping",
                    "mapping": ButtonMapping { cid, base, shifted },
                });
                store.dispatch(crate::commands::Command::SetButtonMapping {
                    device_id: device_id.clone(),
                    mapping,
                });
            }
            dialog.close();
        });
    }

    dialog.present(Some(parent));
}

// ── Action editor ───────────────────────────────────────────────────────────

/// Editor for a single [`ButtonAction`]: a type selector plus a stack of
/// parameter pages, one per action variant.
struct ActionEditor {
    root: gtk::Box,
    combo: adw::ComboRow,
    mouse_combo: adw::ComboRow,
    scroll_axis: adw::ComboRow,
    scroll_clicks: adw::SpinRow,
    key_code: adw::SpinRow,
    mod_ctrl: adw::SwitchRow,
    mod_shift: adw::SwitchRow,
    mod_alt: adw::SwitchRow,
    mod_super: adw::SwitchRow,
    media_combo: adw::ComboRow,
    dpi_dir: adw::ComboRow,
    profile_dir: adw::ComboRow,
    momentary_dpi: adw::SpinRow,
    app_path: adw::EntryRow,
    cmd_entry: adw::EntryRow,
    args_entry: adw::EntryRow,
    macro_editor: MacroEditor,
}

impl ActionEditor {
    fn build(title: &str, initial: &ButtonAction, available_dpis: &[u16]) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();

        let type_group = adw::PreferencesGroup::builder().title(title).build();
        let combo = adw::ComboRow::builder()
            .title("Action")
            .model(&gtk::StringList::new(ACTION_TYPES))
            .build();
        type_group.add(&combo);
        root.append(&type_group);

        let stack = gtk::Stack::builder()
            .vhomogeneous(false)
            .hhomogeneous(true)
            .build();
        root.append(&stack);

        // p0 Native, p1 Disable, p9 Layer Shift — informational only.
        stack.add_named(&info_page("The button keeps its firmware behaviour."), Some("p0"));
        stack.add_named(&info_page("The button press is swallowed and does nothing."), Some("p1"));

        // p2 Mouse button
        let mouse_combo = adw::ComboRow::builder()
            .title("Button")
            .model(&gtk::StringList::new(MOUSE_BTNS))
            .build();
        stack.add_named(&group_with(&[mouse_combo.clone().upcast()]), Some("p2"));

        // p3 Scroll
        let scroll_axis = adw::ComboRow::builder()
            .title("Axis")
            .model(&gtk::StringList::new(SCROLL_AXES))
            .build();
        let scroll_clicks = adw::SpinRow::builder()
            .title("Clicks per press")
            .subtitle("Negative scrolls the opposite direction")
            .adjustment(&gtk::Adjustment::new(1.0, -50.0, 50.0, 1.0, 5.0, 0.0))
            .build();
        stack.add_named(
            &group_with(&[scroll_axis.clone().upcast(), scroll_clicks.clone().upcast()]),
            Some("p3"),
        );

        // p4 Key chord
        let key_code = adw::SpinRow::builder()
            .title("Key code")
            .subtitle("Platform virtual-key / keysym code")
            .adjustment(&gtk::Adjustment::new(0.0, 0.0, 65535.0, 1.0, 16.0, 0.0))
            .build();
        let mod_ctrl = adw::SwitchRow::builder().title("Ctrl").build();
        let mod_shift = adw::SwitchRow::builder().title("Shift").build();
        let mod_alt = adw::SwitchRow::builder().title("Alt").build();
        let mod_super = adw::SwitchRow::builder().title("Super").build();
        stack.add_named(
            &group_with(&[
                key_code.clone().upcast(),
                mod_ctrl.clone().upcast(),
                mod_shift.clone().upcast(),
                mod_alt.clone().upcast(),
                mod_super.clone().upcast(),
            ]),
            Some("p4"),
        );

        // p5 Media key
        let media_combo = adw::ComboRow::builder()
            .title("Media key")
            .model(&gtk::StringList::new(MEDIA_KEYS))
            .build();
        stack.add_named(&group_with(&[media_combo.clone().upcast()]), Some("p5"));

        // p6 DPI cycle
        let dpi_dir = adw::ComboRow::builder()
            .title("Direction")
            .model(&gtk::StringList::new(CYCLE_DIRS))
            .build();
        stack.add_named(&group_with(&[dpi_dir.clone().upcast()]), Some("p6"));

        // p7 Profile cycle
        let profile_dir = adw::ComboRow::builder()
            .title("Direction")
            .model(&gtk::StringList::new(CYCLE_DIRS))
            .build();
        stack.add_named(&group_with(&[profile_dir.clone().upcast()]), Some("p7"));

        // p8 Momentary DPI
        let dpi_min = available_dpis.iter().copied().filter(|&v| v > 0).min().unwrap_or(100) as f64;
        let dpi_max = available_dpis.iter().copied().max().unwrap_or(25600) as f64;
        let momentary_dpi = adw::SpinRow::builder()
            .title("DPI while held")
            .adjustment(&gtk::Adjustment::new(dpi_min, dpi_min, dpi_max, 50.0, 400.0, 0.0))
            .build();
        stack.add_named(&group_with(&[momentary_dpi.clone().upcast()]), Some("p8"));

        // p9 Layer Shift
        stack.add_named(
            &info_page("While held, every other button uses its Layer Shift action."),
            Some("p9"),
        );

        // p10 Macro
        let macro_steps = match initial {
            ButtonAction::Macro { steps } => steps.as_slice(),
            _ => &[],
        };
        let macro_editor = MacroEditor::build(macro_steps);
        stack.add_named(&macro_editor.root, Some("p10"));

        // p11 Open application
        let app_path = adw::EntryRow::builder().title("Application path").build();
        stack.add_named(&group_with(&[app_path.clone().upcast()]), Some("p11"));

        // p12 Run command
        let cmd_entry = adw::EntryRow::builder().title("Command").build();
        let args_entry = adw::EntryRow::builder()
            .title("Arguments (space-separated)")
            .build();
        stack.add_named(
            &group_with(&[cmd_entry.clone().upcast(), args_entry.clone().upcast()]),
            Some("p12"),
        );

        // ── Seed widget values from the initial action ───────────────────────
        match initial {
            ButtonAction::MouseButton { btn } => mouse_combo.set_selected(mouse_btn_to_idx(btn)),
            ButtonAction::Scroll { axis, clicks } => {
                scroll_axis.set_selected(if matches!(axis, ScrollAxis::Horizontal) { 1 } else { 0 });
                scroll_clicks.set_value(*clicks as f64);
            }
            ButtonAction::KeyChord { key, modifiers } => {
                key_code.set_value(*key as f64);
                mod_ctrl.set_active(modifiers.contains(&ModKey::Ctrl));
                mod_shift.set_active(modifiers.contains(&ModKey::Shift));
                mod_alt.set_active(modifiers.contains(&ModKey::Alt));
                mod_super.set_active(modifiers.contains(&ModKey::Super));
            }
            ButtonAction::MediaKey { key } => media_combo.set_selected(media_to_idx(key)),
            ButtonAction::DpiCycle { direction } => dpi_dir.set_selected(dir_to_idx(direction)),
            ButtonAction::ProfileCycle { direction } => profile_dir.set_selected(dir_to_idx(direction)),
            ButtonAction::MomentaryDpi { dpi } => momentary_dpi.set_value(*dpi as f64),
            ButtonAction::OpenApp { path } => app_path.set_text(path),
            ButtonAction::Command { cmd, args } => {
                cmd_entry.set_text(cmd);
                args_entry.set_text(&args.join(" "));
            }
            _ => {}
        }

        combo.set_selected(action_type_index(initial));
        stack.set_visible_child_name(&format!("p{}", action_type_index(initial)));

        {
            let stack = stack.clone();
            combo.connect_selected_notify(move |c| {
                stack.set_visible_child_name(&format!("p{}", c.selected()));
            });
        }

        Self {
            root,
            combo,
            mouse_combo,
            scroll_axis,
            scroll_clicks,
            key_code,
            mod_ctrl,
            mod_shift,
            mod_alt,
            mod_super,
            media_combo,
            dpi_dir,
            profile_dir,
            momentary_dpi,
            app_path,
            cmd_entry,
            args_entry,
            macro_editor,
        }
    }

    /// Read the configured action out of the live widgets.
    fn value(&self) -> ButtonAction {
        match self.combo.selected() {
            0 => ButtonAction::Native,
            1 => ButtonAction::Disable,
            2 => ButtonAction::MouseButton {
                btn: mouse_btn_from_idx(self.mouse_combo.selected()),
            },
            3 => ButtonAction::Scroll {
                axis: axis_from_idx(self.scroll_axis.selected()),
                clicks: self.scroll_clicks.value() as i32,
            },
            4 => {
                let mut modifiers = Vec::new();
                if self.mod_ctrl.is_active() {
                    modifiers.push(ModKey::Ctrl);
                }
                if self.mod_shift.is_active() {
                    modifiers.push(ModKey::Shift);
                }
                if self.mod_alt.is_active() {
                    modifiers.push(ModKey::Alt);
                }
                if self.mod_super.is_active() {
                    modifiers.push(ModKey::Super);
                }
                ButtonAction::KeyChord {
                    key: self.key_code.value() as u32,
                    modifiers,
                }
            }
            5 => ButtonAction::MediaKey {
                key: media_from_idx(self.media_combo.selected()),
            },
            6 => ButtonAction::DpiCycle {
                direction: dir_from_idx(self.dpi_dir.selected()),
            },
            7 => ButtonAction::ProfileCycle {
                direction: dir_from_idx(self.profile_dir.selected()),
            },
            8 => ButtonAction::MomentaryDpi {
                dpi: self.momentary_dpi.value() as u16,
            },
            9 => ButtonAction::LayerShift,
            10 => ButtonAction::Macro {
                steps: self.macro_editor.value(),
            },
            11 => ButtonAction::OpenApp {
                path: self.app_path.text().to_string(),
            },
            12 => ButtonAction::Command {
                cmd: self.cmd_entry.text().to_string(),
                args: self
                    .args_entry
                    .text()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect(),
            },
            _ => ButtonAction::Native,
        }
    }
}

/// A `PreferencesGroup` populated with the given list rows.
fn group_with(rows: &[gtk::Widget]) -> gtk::Widget {
    let group = adw::PreferencesGroup::new();
    for row in rows {
        if let Some(row) = row.downcast_ref::<adw::PreferencesRow>() {
            group.add(row);
        }
    }
    group.upcast()
}

/// A dim, wrapping label used for parameter-less action variants.
fn info_page(text: &str) -> gtk::Widget {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .halign(gtk::Align::Start)
        .margin_top(4)
        .css_classes(["dim-label"])
        .build()
        .upcast()
}

// ── Macro editor ────────────────────────────────────────────────────────────

/// Editor for a [`ButtonAction::Macro`] step list.
struct MacroEditor {
    root: gtk::Box,
    rows: Rc<RefCell<Vec<MacroStepRow>>>,
}

struct MacroStepRow {
    container: gtk::Box,
    atom: gtk::DropDown,
    key_spin: gtk::SpinButton,
    mouse_drop: gtk::DropDown,
    delay_spin: gtk::SpinButton,
}

impl MacroEditor {
    fn build(steps: &[MacroStep]) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();

        let hint = gtk::Label::builder()
            .label("Steps run top to bottom. Delay is the wait after the step, in ms.")
            .wrap(true)
            .halign(gtk::Align::Start)
            .css_classes(["dim-label"])
            .build();
        root.append(&hint);

        let steps_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        root.append(&steps_box);

        let rows: Rc<RefCell<Vec<MacroStepRow>>> = Rc::new(RefCell::new(Vec::new()));

        let add_btn = gtk::Button::builder()
            .label("Add step")
            .icon_name("list-add-symbolic")
            .css_classes(["flat"])
            .halign(gtk::Align::Start)
            .build();
        {
            let steps_box = steps_box.clone();
            let rows = rows.clone();
            add_btn.connect_clicked(move |_| {
                add_macro_row(&steps_box, &rows, None);
            });
        }
        root.append(&add_btn);

        for step in steps {
            add_macro_row(&steps_box, &rows, Some(step));
        }

        Self { root, rows }
    }

    fn value(&self) -> Vec<MacroStep> {
        self.rows
            .borrow()
            .iter()
            .map(|r| {
                let kind = match r.atom.selected() {
                    0 => MacroAtom::KeyDown { key: r.key_spin.value() as u32 },
                    1 => MacroAtom::KeyUp { key: r.key_spin.value() as u32 },
                    2 => MacroAtom::MouseDown { btn: mouse_btn_from_idx(r.mouse_drop.selected()) },
                    3 => MacroAtom::MouseUp { btn: mouse_btn_from_idx(r.mouse_drop.selected()) },
                    _ => MacroAtom::Delay,
                };
                MacroStep {
                    kind,
                    delay_after_ms: r.delay_spin.value() as u32,
                }
            })
            .collect()
    }
}

/// Append one step row to a macro editor, optionally seeded from `step`.
fn add_macro_row(
    steps_box: &gtk::Box,
    rows: &Rc<RefCell<Vec<MacroStepRow>>>,
    step: Option<&MacroStep>,
) {
    let container = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();

    let atom = gtk::DropDown::from_strings(MACRO_ATOMS);
    let key_spin = gtk::SpinButton::with_range(0.0, 65535.0, 1.0);
    key_spin.set_width_chars(6);
    key_spin.set_tooltip_text(Some("Key code"));
    let mouse_drop = gtk::DropDown::from_strings(MOUSE_BTNS);
    let wait_lbl = gtk::Label::builder().label("wait").css_classes(["dim-label"]).build();
    let delay_spin = gtk::SpinButton::with_range(0.0, 60000.0, 10.0);
    delay_spin.set_width_chars(6);
    let ms_lbl = gtk::Label::builder().label("ms").css_classes(["dim-label"]).build();
    let remove = gtk::Button::builder()
        .icon_name("list-remove-symbolic")
        .css_classes(["flat", "circular"])
        .tooltip_text("Remove step")
        .build();

    // Seed from an existing step.
    match step.map(|s| &s.kind) {
        Some(MacroAtom::KeyDown { key }) => {
            atom.set_selected(0);
            key_spin.set_value(*key as f64);
        }
        Some(MacroAtom::KeyUp { key }) => {
            atom.set_selected(1);
            key_spin.set_value(*key as f64);
        }
        Some(MacroAtom::MouseDown { btn }) => {
            atom.set_selected(2);
            mouse_drop.set_selected(mouse_btn_to_idx(btn));
        }
        Some(MacroAtom::MouseUp { btn }) => {
            atom.set_selected(3);
            mouse_drop.set_selected(mouse_btn_to_idx(btn));
        }
        Some(MacroAtom::Delay) => atom.set_selected(4),
        None => {}
    }
    if let Some(s) = step {
        delay_spin.set_value(s.delay_after_ms as f64);
    }

    // Show only the value widget that applies to the selected atom kind.
    let sync_visibility = {
        let atom = atom.clone();
        let key_spin = key_spin.clone();
        let mouse_drop = mouse_drop.clone();
        move || {
            let sel = atom.selected();
            key_spin.set_visible(sel == 0 || sel == 1);
            mouse_drop.set_visible(sel == 2 || sel == 3);
        }
    };
    sync_visibility();
    {
        let sync = sync_visibility.clone();
        atom.connect_selected_notify(move |_| sync());
    }

    container.append(&atom);
    container.append(&key_spin);
    container.append(&mouse_drop);
    container.append(&wait_lbl);
    container.append(&delay_spin);
    container.append(&ms_lbl);
    let spacer = gtk::Box::builder().hexpand(true).build();
    container.append(&spacer);
    container.append(&remove);

    {
        let steps_box = steps_box.clone();
        let rows = rows.clone();
        let container = container.clone();
        remove.connect_clicked(move |_| {
            steps_box.remove(&container);
            rows.borrow_mut().retain(|r| r.container != container);
        });
    }

    steps_box.append(&container);
    rows.borrow_mut().push(MacroStepRow {
        container,
        atom,
        key_spin,
        mouse_drop,
        delay_spin,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::{ButtonAction, CycleDir, MediaAction, ModKey, MouseBtn, ScrollAxis};

    // ── action_type_index ─────────────────────────────────────────────────────

    #[test]
    fn action_type_index_covers_all_variants() {
        assert_eq!(action_type_index(&ButtonAction::Native), 0);
        assert_eq!(action_type_index(&ButtonAction::Disable), 1);
        assert_eq!(action_type_index(&ButtonAction::MouseButton { btn: MouseBtn::Left }), 2);
        assert_eq!(action_type_index(&ButtonAction::Scroll { axis: ScrollAxis::Vertical, clicks: 1 }), 3);
        assert_eq!(action_type_index(&ButtonAction::KeyChord { key: 65, modifiers: vec![] }), 4);
        assert_eq!(action_type_index(&ButtonAction::MediaKey { key: MediaAction::Play }), 5);
        assert_eq!(action_type_index(&ButtonAction::DpiCycle { direction: CycleDir::Up }), 6);
        assert_eq!(action_type_index(&ButtonAction::ProfileCycle { direction: CycleDir::Down }), 7);
        assert_eq!(action_type_index(&ButtonAction::MomentaryDpi { dpi: 800 }), 8);
        assert_eq!(action_type_index(&ButtonAction::LayerShift), 9);
        assert_eq!(action_type_index(&ButtonAction::Macro { steps: vec![] }), 10);
        assert_eq!(action_type_index(&ButtonAction::OpenApp { path: "".into() }), 11);
        assert_eq!(action_type_index(&ButtonAction::Command { cmd: "".into(), args: vec![] }), 12);
    }

    // ── mouse button roundtrip ────────────────────────────────────────────────

    #[test]
    fn mouse_btn_roundtrip() {
        use MouseBtn::*;
        for btn in [Left, Right, Middle, Back, Forward] {
            let idx = mouse_btn_to_idx(&btn);
            assert_eq!(mouse_btn_from_idx(idx), btn, "roundtrip failed for {btn:?}");
        }
    }

    // ── scroll axis ───────────────────────────────────────────────────────────

    #[test]
    fn axis_from_idx_vertical_is_default() {
        assert!(matches!(axis_from_idx(0), ScrollAxis::Vertical));
        assert!(matches!(axis_from_idx(99), ScrollAxis::Vertical));
    }

    #[test]
    fn axis_from_idx_horizontal_at_1() {
        assert!(matches!(axis_from_idx(1), ScrollAxis::Horizontal));
    }

    // ── media key roundtrip ───────────────────────────────────────────────────

    #[test]
    fn media_roundtrip() {
        use MediaAction::*;
        for key in [VolumeUp, VolumeDown, Mute, Play, Next, Prev] {
            let idx = media_to_idx(&key);
            assert_eq!(media_from_idx(idx), key, "roundtrip failed for {key:?}");
        }
    }

    // ── cycle direction roundtrip ─────────────────────────────────────────────

    #[test]
    fn dir_roundtrip() {
        for dir in [CycleDir::Up, CycleDir::Down] {
            let idx = dir_to_idx(&dir);
            assert_eq!(dir_from_idx(idx), dir, "roundtrip failed for {dir:?}");
        }
    }

    // ── action_summary ────────────────────────────────────────────────────────

    #[test]
    fn action_summary_native_and_disable() {
        assert_eq!(action_summary(&ButtonAction::Native), "Firmware default");
        assert_eq!(action_summary(&ButtonAction::Disable), "Disabled");
    }

    #[test]
    fn action_summary_mouse_button_names_button() {
        let s = action_summary(&ButtonAction::MouseButton { btn: MouseBtn::Middle });
        assert!(s.contains("Middle"), "got: {s}");
    }

    #[test]
    fn action_summary_scroll_includes_sign_and_axis() {
        let s = action_summary(&ButtonAction::Scroll { axis: ScrollAxis::Horizontal, clicks: -3 });
        assert!(s.contains("Horizontal") && s.contains("-3"), "got: {s}");
    }

    #[test]
    fn action_summary_key_chord_formats_modifiers() {
        let s = action_summary(&ButtonAction::KeyChord {
            key: 65,
            modifiers: vec![ModKey::Ctrl, ModKey::Shift],
        });
        assert!(s.contains("Ctrl+") && s.contains("Shift+"), "got: {s}");
    }

    #[test]
    fn action_summary_macro_shows_step_count() {
        use halod_protocol::types::{MacroStep, MacroAtom};
        let steps = vec![
            MacroStep { kind: MacroAtom::KeyDown { key: 65 }, delay_after_ms: 0 },
            MacroStep { kind: MacroAtom::KeyUp  { key: 65 }, delay_after_ms: 0 },
        ];
        let s = action_summary(&ButtonAction::Macro { steps });
        assert!(s.contains("2 step"), "got: {s}");
    }

    // ── mod_label ─────────────────────────────────────────────────────────────

    #[test]
    fn mod_label_all_variants() {
        assert_eq!(mod_label(&ModKey::Ctrl),  "Ctrl");
        assert_eq!(mod_label(&ModKey::Shift), "Shift");
        assert_eq!(mod_label(&ModKey::Alt),   "Alt");
        assert_eq!(mod_label(&ModKey::Super), "Super");
    }
}
