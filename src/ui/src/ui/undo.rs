// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::state::undo::UndoAction;

const UNDO: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Z);
const REDO: egui::KeyboardShortcut =
    egui::KeyboardShortcut::new(egui::Modifiers::COMMAND, egui::Key::Y);
const REDO_SHIFT: egui::KeyboardShortcut = egui::KeyboardShortcut::new(
    egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
    egui::Key::Z,
);

pub fn undo_action(input: &mut egui::InputState) -> Option<UndoAction> {
    let repeated = input.events.iter().any(|event| {
        matches!(
            event,
            egui::Event::Key {
                key: egui::Key::Z | egui::Key::Y,
                pressed: true,
                repeat: true,
                ..
            }
        )
    });
    if repeated {
        return None;
    }
    if input.consume_shortcut(&REDO_SHIFT) || input.consume_shortcut(&REDO) {
        Some(UndoAction::Redo)
    } else if input.consume_shortcut(&UNDO) {
        Some(UndoAction::Undo)
    } else {
        None
    }
}

pub fn settled(ctx: &egui::Context) -> bool {
    !ctx.input(|input| input.pointer.any_down()) && ctx.memory(|memory| memory.focused().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_with(key: egui::Key, modifiers: egui::Modifiers) -> Option<UndoAction> {
        pressed_frames(key, modifiers, 1)[0]
    }

    fn pressed_frames(
        key: egui::Key,
        modifiers: egui::Modifiers,
        frames: usize,
    ) -> Vec<Option<UndoAction>> {
        let ctx = egui::Context::default();
        let mut actions = Vec::new();
        for _ in 0..frames {
            let mut action = None;
            crate::ui::run_headless(
                &ctx,
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ui| action = ui.input_mut(undo_action),
            );
            actions.push(action);
        }
        actions
    }

    #[test]
    fn a_held_key_does_not_repeat_the_action() {
        assert_eq!(
            pressed_frames(egui::Key::Z, egui::Modifiers::COMMAND, 3),
            [Some(UndoAction::Undo), None, None]
        );
    }

    #[test]
    fn settled_means_no_pointer_down_and_no_focused_widget() {
        let ctx = egui::Context::default();
        let mut outcomes = Vec::new();
        for pressed in [false, true] {
            let events = if pressed {
                vec![egui::Event::PointerButton {
                    pos: egui::Pos2::ZERO,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                }]
            } else {
                Vec::new()
            };
            crate::ui::run_headless(
                &ctx,
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| outcomes.push(settled(ui.ctx())),
            );
        }
        assert_eq!(outcomes, [true, false]);
    }

    #[test]
    fn ctrl_z_undoes_and_ctrl_shift_z_or_ctrl_y_redo() {
        let shift = egui::Modifiers {
            shift: true,
            ..egui::Modifiers::COMMAND
        };
        assert_eq!(
            input_with(egui::Key::Z, egui::Modifiers::COMMAND),
            Some(UndoAction::Undo)
        );
        assert_eq!(input_with(egui::Key::Z, shift), Some(UndoAction::Redo));
        assert_eq!(
            input_with(egui::Key::Y, egui::Modifiers::COMMAND),
            Some(UndoAction::Redo)
        );
        assert_eq!(input_with(egui::Key::Z, egui::Modifiers::NONE), None);
    }
}
