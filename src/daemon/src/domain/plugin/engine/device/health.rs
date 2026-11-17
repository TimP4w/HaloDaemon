// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::atomic::{AtomicU8, Ordering};

const CLOSED: u8 = 1;
const UNRECOVERABLE: u8 = 1 << 1;
const FAILING: u8 = 1 << 2;
const CHILD_DEGRADED: u8 = 1 << 3;

/// A plugin device's health, shared with its poll and event tasks.
///
/// `closed` and `unrecoverable` are terminal. `failing` latches one failure
/// episode so the safety restore and the user toast fire once per episode.
/// `child_degraded` is a dynamic child's own liveness, kept off the runtime it
/// shares with its root and siblings.
#[derive(Debug, Default)]
pub(super) struct Health(AtomicU8);

impl Health {
    fn has(&self, flags: u8) -> bool {
        self.0.load(Ordering::Acquire) & flags != 0
    }

    fn set(&self, flag: u8, on: bool) -> bool {
        let previous = if on {
            self.0.fetch_or(flag, Ordering::AcqRel)
        } else {
            self.0.fetch_and(!flag, Ordering::AcqRel)
        };
        previous & flag != 0
    }

    /// Returns whether the device was already closed.
    pub(super) fn close(&self) -> bool {
        self.set(CLOSED, true)
    }

    pub(super) fn is_closed(&self) -> bool {
        self.has(CLOSED)
    }

    pub(super) fn mark_unrecoverable(&self) {
        self.set(UNRECOVERABLE, true);
    }

    pub(super) fn is_unrecoverable(&self) -> bool {
        self.has(UNRECOVERABLE)
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.has(CLOSED | UNRECOVERABLE)
    }

    /// Returns whether this failure starts a new episode.
    pub(super) fn begin_failure(&self) -> bool {
        !self.set(FAILING, true)
    }

    pub(super) fn end_failure(&self) {
        self.set(FAILING, false);
    }

    pub(super) fn set_child_degraded(&self, degraded: bool) {
        self.set(CHILD_DEGRADED, degraded);
    }

    pub(super) fn blocks_liveness(&self) -> bool {
        self.has(CLOSED | UNRECOVERABLE | CHILD_DEGRADED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_episode_begins_once_until_it_ends() {
        let health = Health::default();
        assert!(health.begin_failure());
        assert!(!health.begin_failure());
        health.end_failure();
        assert!(health.begin_failure());
    }

    #[test]
    fn terminal_states_survive_failure_recovery() {
        let health = Health::default();
        health.mark_unrecoverable();
        health.begin_failure();
        health.end_failure();
        assert!(health.is_unrecoverable());
        assert!(!health.close());
        assert!(health.close());
        assert!(health.is_stopped());
    }

    #[test]
    fn only_a_degraded_child_blocks_liveness_while_merely_failing() {
        let health = Health::default();
        health.begin_failure();
        assert!(!health.blocks_liveness());
        health.set_child_degraded(true);
        assert!(health.blocks_liveness());
        health.set_child_degraded(false);
        assert!(!health.blocks_liveness());
    }
}
