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
