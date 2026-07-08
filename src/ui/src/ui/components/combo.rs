// SPDX-License-Identifier: GPL-3.0-or-later
//! The labeled-option combo box.

/// The label to show for `current` in a combo box: the matching option's name,
/// falling back to `none_label` (typically shown when `current` is empty),
/// then a plain dash.
pub fn combo_display<'a>(
    options: &'a [(String, String)],
    current: &str,
    none_label: Option<&'a str>,
) -> &'a str {
    options
        .iter()
        .find(|(id, _)| id == current)
        .map(|(_, name)| name.as_str())
        .or(none_label)
        .unwrap_or("-")
}

/// A labeled-option combo box (id, display name). Returns the newly selected
/// id when the user picks a different entry. `none_label`, when set, renders
/// an extra leading entry that maps to an empty string id.
pub fn combo_picker(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    options: &[(String, String)],
    current: &str,
    none_label: Option<&str>,
) -> Option<String> {
    let display = combo_display(options, current, none_label);
    let mut new_val = None;
    egui::ComboBox::from_id_salt(id_salt)
        .selected_text(display)
        .show_ui(ui, |ui| {
            if let Some(none_label) = none_label {
                if ui
                    .selectable_label(current.is_empty(), none_label)
                    .clicked()
                {
                    new_val = Some(String::new());
                }
            }
            for (id, name) in options {
                if ui.selectable_label(id == current, name).clicked() {
                    new_val = Some(id.clone());
                }
            }
        });
    new_val
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combo_display_finds_matching_option_name() {
        let opts = vec![
            ("a".to_string(), "Alpha".to_string()),
            ("b".to_string(), "Beta".to_string()),
        ];
        assert_eq!(combo_display(&opts, "b", None), "Beta");
    }

    #[test]
    fn combo_display_falls_back_to_none_label_when_unmatched() {
        let opts = vec![("a".to_string(), "Alpha".to_string())];
        assert_eq!(combo_display(&opts, "", Some("(none)")), "(none)");
        assert_eq!(combo_display(&opts, "missing", Some("(none)")), "(none)");
    }

    #[test]
    fn combo_display_falls_back_to_dash_without_none_label() {
        let opts = vec![("a".to_string(), "Alpha".to_string())];
        assert_eq!(combo_display(&opts, "missing", None), "-");
    }
}
