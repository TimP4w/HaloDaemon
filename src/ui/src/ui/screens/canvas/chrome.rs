// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas transport (play/pause/stop) and the FPS button.

use egui::Vec2;
use halod_shared::commands::{DaemonCommand, EngineKind};

use crate::runtime::ipc::CommandTx;
use crate::ui::components::{self as widgets, ButtonKind};

use super::CanvasUi;

/// The canvas transport cluster (play/pause/stop · FPS), laid out by the
/// caller's right-to-left header actions.
pub(crate) fn chrome(ui: &mut egui::Ui, canvas_ui: &mut CanvasUi, cmd: &CommandTx, playing: bool) {
    if widgets::button(
        ui,
        &canvas_ui.fps_label,
        ButtonKind::Secondary,
        Vec2::new(0.0, 36.0),
    )
    .clicked()
    {
        canvas_ui.fps_modal_open = true;
    }
    let labels = [t!("canvas.play"), t!("canvas.pause"), t!("canvas.stop")];
    let refs: Vec<&str> = labels.iter().map(|l| l.as_ref()).collect();
    let mut selected = transport_index(playing);
    let raised = widgets::SegmentedStyle {
        raised: true,
        md: false,
    };
    if widgets::segmented_styled(ui, &refs, &mut selected, false, raised) {
        crate::runtime::ipc::send(cmd, transport_command(selected));
    }
}

fn transport_index(playing: bool) -> usize {
    if playing {
        0
    } else {
        1
    }
}

fn transport_command(index: usize) -> DaemonCommand {
    match index {
        0 => DaemonCommand::set_engine_enabled(EngineKind::Canvas, true),
        1 => DaemonCommand::set_engine_enabled(EngineKind::Canvas, false),
        _ => DaemonCommand::CanvasStop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_round_trips_play_and_pause() {
        for playing in [true, false] {
            assert!(matches!(
                transport_command(transport_index(playing)),
                DaemonCommand::SetEngineConfig { enabled: Some(enabled), .. } if enabled == playing
            ));
        }
        assert!(matches!(transport_command(2), DaemonCommand::CanvasStop));
    }
}
