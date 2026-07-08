// SPDX-License-Identifier: GPL-3.0-or-later
//! Presentation layer: pure rendering, hide/show, animation. Talks to the
//! daemon only through `domain::actions`; never touches `runtime` directly.

pub mod components;
pub mod icons;
mod root;
pub mod screens;
pub mod shell;
pub mod theme;
pub mod tour;
