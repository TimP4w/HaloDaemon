// SPDX-License-Identifier: GPL-3.0-or-later
//! App chrome: the platform-adaptive title bar with the main navigation tabs.

mod bell;
mod nav;
mod titlebar;

pub use titlebar::{
    arm_pointer_release_workaround, daemon_overlay, take_pending_pointer_release, title_bar,
    title_bar_plain,
};
