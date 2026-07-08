// SPDX-License-Identifier: GPL-3.0-or-later
//! App chrome: the platform-adaptive title bar and the left sidebar (workspace nav +
//! live device list + account footer). Only "Home" is wired up; the other nav
//! entries are present for the layout but inert in this prototype.

mod sidebar;
mod titlebar;

pub use sidebar::sidebar;
pub use titlebar::{
    arm_pointer_release_workaround, daemon_overlay, take_pending_pointer_release, title_bar,
};
