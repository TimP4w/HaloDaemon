// SPDX-License-Identifier: GPL-3.0-or-later
//! Presentation layer: pure rendering, hide/show, animation. Talks to the
//! daemon through the typed command channel in `runtime`.

pub mod components;
pub mod icons;
pub(crate) mod root;
pub mod screens;
pub mod shell;
pub mod theme;
pub mod tour;
pub mod undo;

/// Run one headless frame. egui asserts that every frame's texture delta is
/// consumed, and tests have no painter to hand it to.
#[cfg(test)]
pub fn run_headless(
    ctx: &egui::Context,
    input: egui::RawInput,
    run_ui: impl FnMut(&mut egui::Ui),
) -> egui::FullOutput {
    let mut out = ctx.run_ui(input, run_ui);
    out.textures_delta.clear();
    out
}
