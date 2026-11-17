// SPDX-License-Identifier: GPL-3.0-or-later
use halod_shared::lcd_custom::{CustomTemplateDef, WidgetDef};

use super::library::next_widget_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayerAction {
    BringForward,
    SendBackward,
    BringToFront,
    SendToBack,
    Duplicate,
    CenterHorizontally,
    CenterVertically,
    Delete,
}

impl LayerAction {
    pub(super) const ORDER: [LayerAction; 4] = [
        Self::SendToBack,
        Self::SendBackward,
        Self::BringForward,
        Self::BringToFront,
    ];

    pub(super) const MENU: [LayerAction; 6] = [
        Self::Delete,
        Self::BringForward,
        Self::SendBackward,
        Self::Duplicate,
        Self::CenterHorizontally,
        Self::CenterVertically,
    ];

    pub(super) fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::BringForward => t!("lcd.layer_bring_forward"),
            Self::SendBackward => t!("lcd.layer_send_backward"),
            Self::BringToFront => t!("lcd.layer_bring_to_front"),
            Self::SendToBack => t!("lcd.layer_send_to_back"),
            Self::Duplicate => t!("lcd.layer_duplicate"),
            Self::CenterHorizontally => t!("lcd.layer_center_horizontally"),
            Self::CenterVertically => t!("lcd.layer_center_vertically"),
            Self::Delete => t!("lcd.delete"),
        }
    }
}

pub(super) enum LayerOutcome {
    Unchanged,
    Changed,
    Selected(String),
    Removed,
}

pub(super) fn apply(def: &mut CustomTemplateDef, id: &str, action: LayerAction) -> LayerOutcome {
    let Some(index) = def.widgets.iter().position(|widget| widget.id == id) else {
        return LayerOutcome::Unchanged;
    };
    let last = def.widgets.len() - 1;
    let target = match action {
        LayerAction::BringForward => Some((index + 1).min(last)),
        LayerAction::SendBackward => Some(index.saturating_sub(1)),
        LayerAction::BringToFront => Some(last),
        LayerAction::SendToBack => Some(0),
        _ => None,
    };
    if let Some(target) = target {
        if target == index {
            return LayerOutcome::Unchanged;
        }
        let widget = def.widgets.remove(index);
        def.widgets.insert(target, widget);
        return LayerOutcome::Changed;
    }
    match action {
        LayerAction::Duplicate => {
            let copy = WidgetDef {
                id: next_widget_id(&def.widgets),
                ..def.widgets[index].clone()
            };
            let new_id = copy.id.clone();
            def.widgets.insert(index + 1, copy);
            LayerOutcome::Selected(new_id)
        }
        LayerAction::CenterHorizontally => center(&mut def.widgets[index].x),
        LayerAction::CenterVertically => center(&mut def.widgets[index].y),
        LayerAction::Delete => {
            def.widgets.remove(index);
            LayerOutcome::Removed
        }
        _ => LayerOutcome::Unchanged,
    }
}

fn center(coordinate: &mut f32) -> LayerOutcome {
    if *coordinate == 0.5 {
        return LayerOutcome::Unchanged;
    }
    *coordinate = 0.5;
    LayerOutcome::Changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn def(count: usize) -> CustomTemplateDef {
        CustomTemplateDef {
            widgets: (0..count)
                .map(|index| WidgetDef {
                    id: format!("w{}", index + 1),
                    widget: "text".into(),
                    x: 0.1 * index as f32,
                    y: 0.2,
                    scale: 0.4,
                    rotation: 0.0,
                    color: None,
                    font: None,
                    params: Default::default(),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn ids(def: &CustomTemplateDef) -> Vec<&str> {
        def.widgets
            .iter()
            .map(|widget| widget.id.as_str())
            .collect()
    }

    proptest! {
        #[test]
        fn a_one_step_move_keeps_the_layers_and_swaps_only_neighbours(count in 1usize..8, pick in 0usize..8, forward: bool) {
            let mut layers = def(count);
            let id = format!("w{}", pick % count + 1);
            let before: Vec<String> = ids(&layers).iter().map(|id| id.to_string()).collect();
            let action = if forward { LayerAction::BringForward } else { LayerAction::SendBackward };
            apply(&mut layers, &id, action);
            let after = ids(&layers);
            let mut sorted_before = before.clone();
            sorted_before.sort();
            let mut sorted_after: Vec<String> = after.iter().map(|id| id.to_string()).collect();
            sorted_after.sort();
            prop_assert_eq!(sorted_before, sorted_after);
            let index = pick % count;
            let moved: Vec<usize> = (0..count).filter(|i| before[*i] != after[*i]).collect();
            let expected: Vec<usize> = match (forward, index) {
                (true, i) if i + 1 < count => vec![i, i + 1],
                (false, i) if i > 0 => vec![i - 1, i],
                _ => Vec::new(),
            };
            prop_assert_eq!(moved, expected);
        }

        #[test]
        fn front_and_back_are_idempotent(count in 1usize..8, pick in 0usize..8) {
            let id = format!("w{}", pick % count + 1);
            for action in [LayerAction::BringToFront, LayerAction::SendToBack] {
                let mut once = def(count);
                apply(&mut once, &id, action);
                let mut twice = once.clone();
                prop_assert!(matches!(apply(&mut twice, &id, action), LayerOutcome::Unchanged));
                prop_assert_eq!(once, twice);
            }
        }

        #[test]
        fn centering_puts_the_midpoint_at_the_canvas_midpoint(width in 1u32..2000, height in 1u32..2000, x in 0.0f32..1.0, y in 0.0f32..1.0) {
            let mut layers = def(1);
            layers.widgets[0].x = x;
            layers.widgets[0].y = y;
            let centred_h = apply(&mut layers, "w1", LayerAction::CenterHorizontally);
            let centred_v = apply(&mut layers, "w1", LayerAction::CenterVertically);
            prop_assert_eq!(matches!(centred_h, LayerOutcome::Changed), x != 0.5);
            prop_assert_eq!(matches!(centred_v, LayerOutcome::Changed), y != 0.5);
            prop_assert_eq!(layers.widgets[0].x * width as f32, width as f32 / 2.0);
            prop_assert_eq!(layers.widgets[0].y * height as f32, height as f32 / 2.0);
        }
    }

    #[test]
    fn moving_past_either_end_is_a_no_op() {
        let mut layers = def(3);
        assert!(matches!(
            apply(&mut layers, "w3", LayerAction::BringForward),
            LayerOutcome::Unchanged
        ));
        assert!(matches!(
            apply(&mut layers, "w1", LayerAction::SendBackward),
            LayerOutcome::Unchanged
        ));
        assert_eq!(ids(&layers), ["w1", "w2", "w3"]);
    }

    #[test]
    fn duplicate_inserts_a_new_id_with_the_same_geometry_directly_above() {
        let mut layers = def(3);
        let LayerOutcome::Selected(new_id) = apply(&mut layers, "w2", LayerAction::Duplicate)
        else {
            panic!("duplicate selects the copy");
        };
        assert_eq!(ids(&layers), ["w1", "w2", new_id.as_str(), "w3"]);
        let (source, copy) = (&layers.widgets[1], &layers.widgets[2]);
        assert_ne!(source.id, copy.id);
        assert_eq!(
            (source.x, source.y, source.scale, source.rotation),
            (copy.x, copy.y, copy.scale, copy.rotation)
        );
    }
}
