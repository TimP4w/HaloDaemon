// SPDX-License-Identifier: GPL-3.0-or-later
//! Plugin-declared config-field editing for an integration's detail pane on
//! the Plugins screen (host/port/token), shared by its setup flow and its
//! connection section.

use std::collections::HashMap;

use halod_shared::types::{PluginConfigField, PluginConfigFieldKind, PluginInfo, SerialPortInfo};

use crate::ui::components::{self as widgets, ButtonKind};
use crate::ui::theme;
use halod_shared::ids::PluginId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigEditor {
    Text,
    Number,
    Boolean,
    Enum,
    Host,
    Port,
    SerialPort,
    Url,
    DurationMs,
}

fn config_editor(kind: PluginConfigFieldKind) -> ConfigEditor {
    match kind {
        PluginConfigFieldKind::Text => ConfigEditor::Text,
        PluginConfigFieldKind::Number => ConfigEditor::Number,
        PluginConfigFieldKind::Boolean => ConfigEditor::Boolean,
        PluginConfigFieldKind::Enum => ConfigEditor::Enum,
        PluginConfigFieldKind::Host => ConfigEditor::Host,
        PluginConfigFieldKind::Port => ConfigEditor::Port,
        PluginConfigFieldKind::SerialPort => ConfigEditor::SerialPort,
        PluginConfigFieldKind::Url => ConfigEditor::Url,
        PluginConfigFieldKind::DurationMs => ConfigEditor::DurationMs,
    }
}

fn config_field_visible(field: &PluginConfigField, edits: &HashMap<String, String>) -> bool {
    field.visible_when.as_ref().is_none_or(|rule| {
        edits
            .get(&rule.field)
            .is_some_and(|value| value == &rule.equals)
    })
}

/// Reset the edit buffer when the selection moves to a different plugin, so
/// stale text from a previous plugin's fields never leaks into another's. A
/// secure field's buffer always starts blank — never seeded from
/// `config_values` (which never carries a secret's plaintext) — so an
/// untouched secure field sends nothing and the stored secret is left alone.
pub fn seed_config_edit_if_needed(
    edit: &mut Option<(PluginId, HashMap<String, String>)>,
    plugin_id: &PluginId,
    config_values: &HashMap<String, String>,
) {
    if edit.as_ref().map(|(id, _)| id.as_str()) != Some(plugin_id) {
        *edit = Some((plugin_id.clone(), config_values.clone()));
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
    serial_ports: &[SerialPortInfo],
    save_label: &str,
    mut on_save: impl FnMut(HashMap<String, String>),
) {
    config_fields_editor(ui, p, edits, serial_ports);
    ui.add_space(theme::SPACE_6);
    if widgets::button(
        ui,
        save_label,
        ButtonKind::Secondary,
        egui::Vec2::new(140.0, 30.0),
    )
    .clicked()
    {
        on_save(config_values_to_send(edits, &p.config_fields));
        for f in &p.config_fields {
            if f.secure {
                edits.insert(f.key.clone(), String::new());
            }
        }
    }
}

const ROW_WEIGHT: usize = 3;

fn field_weight(kind: PluginConfigFieldKind) -> usize {
    match config_editor(kind) {
        ConfigEditor::Port
        | ConfigEditor::Number
        | ConfigEditor::DurationMs
        | ConfigEditor::Boolean => 1,
        _ => 2,
    }
}

/// Greedy packing of field weights into rows of at most [`ROW_WEIGHT`], so a
/// host and its port share one line.
fn pack_rows(weights: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (i, w) in weights.iter().enumerate() {
        if i > start && used + w > ROW_WEIGHT {
            rows.push(start..i);
            start = i;
            used = 0;
        }
        used += w;
    }
    if start < weights.len() {
        rows.push(start..weights.len());
    }
    rows
}

/// Render only the manifest fields as labelled inputs. Setup flows reuse this
/// and own their navigation buttons separately.
pub fn config_fields_editor(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    edits: &mut HashMap<String, String>,
    serial_ports: &[SerialPortInfo],
) {
    let mut groups: std::collections::BTreeMap<String, Vec<&PluginConfigField>> =
        std::collections::BTreeMap::new();
    for f in &p.config_fields {
        groups.entry(f.category.clone()).or_default().push(f);
    }
    for (category, fields) in &groups {
        let visible: Vec<_> = fields
            .iter()
            .copied()
            .filter(|field| config_field_visible(field, edits))
            .collect();
        if visible.is_empty() {
            continue;
        }
        if !category.is_empty() {
            widgets::section_label(ui, category);
            ui.add_space(theme::SPACE_4);
        }
        let weights: Vec<usize> = visible.iter().map(|f| field_weight(f.kind)).collect();
        for row in pack_rows(&weights) {
            let total: usize = weights[row.clone()].iter().sum();
            let gaps = (row.len() - 1) as f32 * theme::SPACE_6;
            let unit = (ui.available_width() - gaps) / total as f32;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = theme::SPACE_6;
                for i in row {
                    let width = unit * weights[i] as f32;
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(width);
                            config_field(ui, p, visible[i], edits, serial_ports);
                        },
                    );
                }
            });
            ui.add_space(theme::SPACE_6);
        }
    }
}

fn config_field(
    ui: &mut egui::Ui,
    p: &PluginInfo,
    f: &PluginConfigField,
    edits: &mut HashMap<String, String>,
    serial_ports: &[SerialPortInfo],
) {
    widgets::field_label(ui, &f.label);
    let width = ui.available_width();
    let buf = edits.entry(f.key.clone()).or_default();
    match (f.secure, config_editor(f.kind)) {
        (false, ConfigEditor::Boolean) => {
            let on = buf == "true";
            if widgets::toggle(ui, on) != on {
                *buf = (!on).to_string();
            }
        }
        (false, ConfigEditor::Enum) => {
            let options: Vec<(String, String)> =
                f.options.iter().map(|o| (o.clone(), o.clone())).collect();
            if let Some(v) =
                widgets::combo_picker_full(ui, ("plugin_config", &f.key), &options, buf, None)
            {
                *buf = v;
            }
        }
        (false, ConfigEditor::SerialPort) => {
            // The current value stays the selected text so a stale/hand-typed
            // by-id path survives an unplug; the free-text field covers ports
            // that were never enumerated.
            let selected = serial_ports
                .iter()
                .find(|port| &port.value == buf)
                .map(|port| port.label.clone())
                .unwrap_or_else(|| buf.clone());
            egui::ComboBox::from_id_salt(("plugin_config", &f.key))
                .selected_text(selected)
                .width(width)
                .show_ui(ui, |ui| {
                    ui.add(
                        egui::TextEdit::singleline(buf).hint_text("/dev/serial/by-id/… or COM3"),
                    );
                    if !serial_ports.is_empty() {
                        ui.separator();
                    }
                    for port in serial_ports {
                        ui.selectable_value(buf, port.value.clone(), &port.label);
                    }
                });
        }
        (secure, editor) => {
            let secret_set = secure && p.secret_set.get(&f.key).copied().unwrap_or(false);
            let hint = if secret_set {
                t!("plugins.secret_set_hint").to_string()
            } else if let Some(placeholder) = &f.placeholder {
                placeholder.clone()
            } else {
                match editor {
                    ConfigEditor::Number => "0",
                    ConfigEditor::Host => "example.com",
                    ConfigEditor::Port => "1–65535",
                    ConfigEditor::Url => "https://example.com",
                    ConfigEditor::DurationMs => "milliseconds",
                    _ => "",
                }
                .to_owned()
            };
            widgets::text_field_ex(ui, buf, &hint, width, secure);
        }
    }
    if let Some(help) = &f.help {
        ui.add_space(theme::SPACE_3);
        ui.label(
            egui::RichText::new(help)
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
    }
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
            options: vec![],
            min: None,
            max: None,
            visible_when: None,
            help: None,
            placeholder: None,
        }
    }

    #[test]
    fn every_config_field_kind_has_a_distinct_editor() {
        let cases = [
            (PluginConfigFieldKind::Text, ConfigEditor::Text),
            (PluginConfigFieldKind::Number, ConfigEditor::Number),
            (PluginConfigFieldKind::Boolean, ConfigEditor::Boolean),
            (PluginConfigFieldKind::Enum, ConfigEditor::Enum),
            (PluginConfigFieldKind::Host, ConfigEditor::Host),
            (PluginConfigFieldKind::Port, ConfigEditor::Port),
            (PluginConfigFieldKind::Url, ConfigEditor::Url),
            (PluginConfigFieldKind::DurationMs, ConfigEditor::DurationMs),
        ];
        for (kind, expected) in cases {
            assert_eq!(config_editor(kind), expected);
        }
    }

    #[test]
    fn host_and_port_share_a_row_and_wide_fields_wrap() {
        let host = field_weight(PluginConfigFieldKind::Host);
        let port = field_weight(PluginConfigFieldKind::Port);
        assert_eq!(pack_rows(&[host, port]), vec![0..2]);
        assert_eq!(pack_rows(&[host, host, port]), vec![0..1, 1..3]);
        assert!(pack_rows(&[]).is_empty());
    }

    #[test]
    fn conditional_visibility_uses_sibling_string_equality() {
        let mut dependent = field("token", false);
        dependent.visible_when = Some(halod_shared::types::PluginConfigVisibility {
            field: "mode".into(),
            equals: "remote".into(),
        });
        assert!(!config_field_visible(&dependent, &HashMap::new()));
        assert!(config_field_visible(
            &dependent,
            &HashMap::from([("mode".into(), "remote".into())])
        ));
        assert!(!config_field_visible(
            &dependent,
            &HashMap::from([("mode".into(), "local".into())])
        ));
    }

    #[test]
    fn seed_config_edit_initializes_from_config_values_on_first_use() {
        let mut edit = None;
        let values = HashMap::from([("host".into(), "1.2.3.4".into())]);
        seed_config_edit_if_needed(&mut edit, &"openrgb".parse().unwrap(), &values);
        assert_eq!(edit, Some(("openrgb".parse().unwrap(), values)));
    }

    #[test]
    fn seed_config_edit_resets_on_plugin_change_but_not_same_plugin() {
        let mut edit = Some((
            "openrgb".parse::<PluginId>().unwrap(),
            HashMap::from([("host".to_string(), "typed-value".to_string())]),
        ));
        let stored = HashMap::from([("host".to_string(), "1.2.3.4".to_string())]);

        // Same plugin still selected: keep whatever the user is typing.
        seed_config_edit_if_needed(&mut edit, &"openrgb".parse().unwrap(), &stored);
        assert_eq!(
            edit.as_ref().unwrap().1.get("host"),
            Some(&"typed-value".to_string())
        );

        // Selection moved to a different plugin: reseed from its own values.
        seed_config_edit_if_needed(&mut edit, &"other".parse().unwrap(), &stored);
        assert_eq!(edit, Some(("other".parse().unwrap(), stored)));
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
