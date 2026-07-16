// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin-declared config-field editing, shared by the Plugins screen (a
//! device/effect plugin's own settings) and the Integrations screen (an
//! integration's host/port/token, the only place those are editable — see
//! `screens::integrations`).

use std::collections::HashMap;

use egui::Stroke;
use halod_shared::types::{PluginConfigField, PluginConfigFieldKind, PluginInfo};

use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::theme;

/// Reset the edit buffer when the selection moves to a different plugin, so
/// stale text from a previous plugin's fields never leaks into another's. A
/// secure field's buffer always starts blank — never seeded from
/// `config_values` (which never carries a secret's plaintext) — so an
/// untouched secure field sends nothing and the stored secret is left alone.
pub fn seed_config_edit_if_needed(
    edit: &mut Option<(String, HashMap<String, String>)>,
    plugin_id: &str,
    config_values: &HashMap<String, String>,
) {
    if edit.as_ref().map(|(id, _)| id.as_str()) != Some(plugin_id) {
        *edit = Some((plugin_id.to_owned(), config_values.clone()));
    }
}

/// What `Save` actually sends: every non-secure field's current buffer value,
/// plus a secure field's value only when the user typed something into it —
/// an empty secure buffer means "leave the stored secret unchanged" (see
/// `SetPluginConfig`/`SetIntegrationConfig`).
pub fn config_values_to_send(
    edits: &HashMap<String, String>,
    fields: &[PluginConfigField],
) -> HashMap<String, String> {
    fields
        .iter()
        .filter_map(|f| {
            let v = edits.get(&f.key)?;
            if f.secure && v.is_empty() {
                None
            } else {
                Some((f.key.clone(), v.clone()))
            }
        })
        .collect()
}

/// Render `p`'s declared config fields with a Save button; `on_save` is
/// called with the values to send (see `config_values_to_send`) when clicked.
/// Blanks secure buffers again after a save so a typed replacement doesn't
/// linger on screen.
pub fn config_section(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    edits: &mut HashMap<String, String>,
    mut on_save: impl FnMut(HashMap<String, String>),
) {
    widgets::caps_label(ui, &t!("plugins.settings"));
    ui.add_space(theme::SPACE_4);

    let mut groups: std::collections::BTreeMap<String, Vec<&PluginConfigField>> =
        std::collections::BTreeMap::new();
    for f in &p.config_fields {
        groups.entry(f.category.clone()).or_default().push(f);
    }

    // The Configure panel sits on its own inset surface (darker than the card)
    // so the editable region reads apart from the status chrome above it.
    egui::Frame::NONE
        .fill(theme::INNER_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(theme::RADIUS_MD)
        .inner_margin(egui::Margin::symmetric(14, 4))
        .show(ui, |ui| {
            let mut first = true;
            for (category, fields) in &groups {
                if !category.is_empty() {
                    if !first {
                        field_separator(ui);
                    }
                    ui.add_space(theme::SPACE_4);
                    widgets::caps_label(ui, category);
                }
                for f in fields {
                    if !first {
                        field_separator(ui);
                    }
                    first = false;
                    config_field_row(ui, p, f, edits);
                }
            }
        });

    ui.add_space(theme::SPACE_6);
    if widgets::button(
        ui,
        &t!("plugins.save_settings"),
        ButtonKind::Primary,
        egui::Vec2::new(140.0, 30.0),
    )
    .clicked()
    {
        let values = config_values_to_send(edits, &p.config_fields);
        on_save(values);
        for f in &p.config_fields {
            if f.secure {
                edits.insert(f.key.clone(), String::new());
            }
        }
    }
}

/// One field laid out as a row: label on the left, padded input on the right.
fn config_field_row(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    f: &PluginConfigField,
    edits: &mut HashMap<String, String>,
) {
    let secret_set = f.secure && p.secret_set.get(&f.key).copied().unwrap_or(false);
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 40.0), egui::Sense::hover());

    let mut left = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    left.label(
        egui::RichText::new(&f.label)
            .font(theme::body_md())
            .color(theme::TEXT),
    );

    let mut right = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
    );
    let buf = edits.entry(f.key.clone()).or_default();
    let mut edit = egui::TextEdit::singleline(buf)
        .desired_width(200.0)
        .margin(egui::Margin::symmetric(9, 6))
        .horizontal_align(egui::Align::RIGHT);
    if f.secure {
        edit = edit.password(true);
    }
    if secret_set {
        // A secure field always starts blank; the hint tells the user a secret
        // is already stored so an untouched field leaves it be.
        edit = edit.hint_text(t!("plugins.secret_set_hint"));
    } else if matches!(f.kind, PluginConfigFieldKind::Number) {
        // Numeric fields are still stored/sent as strings (the plugin/Lua side
        // interprets them); this only nudges the on-screen keyboard/validation,
        // it doesn't change the type.
        edit = edit.hint_text("0");
    }
    right.add(edit);
}

/// A hairline divider between config rows, inset to the panel's content width.
fn field_separator(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        Stroke::new(1.0, theme::BORDER_SOFT),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(key: &str, secure: bool) -> PluginConfigField {
        PluginConfigField {
            key: key.to_string(),
            label: key.to_string(),
            kind: PluginConfigFieldKind::Text,
            category: String::new(),
            secure,
            min: None,
            max: None,
        }
    }

    #[test]
    fn seed_config_edit_initializes_from_config_values_on_first_use() {
        let mut edit = None;
        let values = HashMap::from([("host".to_string(), "1.2.3.4".to_string())]);
        seed_config_edit_if_needed(&mut edit, "openrgb", &values);
        assert_eq!(edit, Some(("openrgb".to_string(), values)));
    }

    #[test]
    fn seed_config_edit_resets_on_plugin_change_but_not_same_plugin() {
        let mut edit = Some((
            "openrgb".to_string(),
            HashMap::from([("host".to_string(), "typed-value".to_string())]),
        ));
        let stored = HashMap::from([("host".to_string(), "1.2.3.4".to_string())]);

        // Same plugin still selected: keep whatever the user is typing.
        seed_config_edit_if_needed(&mut edit, "openrgb", &stored);
        assert_eq!(
            edit.as_ref().unwrap().1.get("host"),
            Some(&"typed-value".to_string())
        );

        // Selection moved to a different plugin: reseed from its own values.
        seed_config_edit_if_needed(&mut edit, "other", &stored);
        assert_eq!(edit, Some(("other".to_string(), stored)));
    }

    #[test]
    fn config_values_to_send_always_includes_non_secure_fields() {
        let edits = HashMap::from([("host".to_string(), "1.2.3.4".to_string())]);
        let fields = vec![field("host", false)];
        let sent = config_values_to_send(&edits, &fields);
        assert_eq!(sent.get("host"), Some(&"1.2.3.4".to_string()));
    }

    #[test]
    fn config_values_to_send_omits_a_blank_secure_field() {
        let edits = HashMap::from([("token".to_string(), String::new())]);
        let fields = vec![field("token", true)];
        let sent = config_values_to_send(&edits, &fields);
        assert!(
            !sent.contains_key("token"),
            "an untouched secure field must not be sent, so the stored secret is left alone"
        );
    }

    #[test]
    fn config_values_to_send_includes_a_typed_secure_field() {
        let edits = HashMap::from([("token".to_string(), "new-secret".to_string())]);
        let fields = vec![field("token", true)];
        let sent = config_values_to_send(&edits, &fields);
        assert_eq!(sent.get("token"), Some(&"new-secret".to_string()));
    }

    #[test]
    fn config_values_to_send_skips_fields_with_no_buffer_entry() {
        let edits = HashMap::new();
        let fields = vec![field("host", false)];
        assert!(config_values_to_send(&edits, &fields).is_empty());
    }
}
