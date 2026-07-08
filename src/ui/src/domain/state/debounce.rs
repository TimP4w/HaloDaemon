// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared debounce map: queue one command per key, send whatever's past its
//! deadline. Used by the device page's per-slider/curve/paint edits so a fast
//! drag coalesces into a single daemon round-trip instead of one per frame.

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};

use halod_shared::commands::DaemonCommand;

use crate::runtime::ipc::{self, CommandTx};

/// Debounced outbound commands, keyed by a caller string → (command, due
/// time). Latest-queued value per key wins; [`Debouncer::flush`] sends
/// whatever has passed its deadline. Derefs to the underlying map so callers
/// can still inspect pending entries directly (`contains_key`, `len`, …).
#[derive(Default)]
pub struct Debouncer(HashMap<String, (DaemonCommand, f64)>);

impl Debouncer {
    /// Queue a command; the latest value per `key` wins and is sent once
    /// [`flush`](Self::flush) runs at or after `time + debounce_secs`.
    pub fn queue(&mut self, key: &str, cmd: DaemonCommand, time: f64, debounce_secs: f64) {
        self.0.insert(key.to_string(), (cmd, time + debounce_secs));
    }

    /// Send each pending command whose deadline has passed; keep the rest.
    pub fn flush(&mut self, cmd: &CommandTx, time: f64) {
        let due: Vec<String> = self
            .0
            .iter()
            .filter(|(_, (_, t))| time >= *t)
            .map(|(k, _)| k.clone())
            .collect();
        for k in due {
            if let Some((c, _)) = self.0.remove(&k) {
                ipc::send(cmd, c);
            }
        }
    }
}

impl Deref for Debouncer {
    type Target = HashMap<String, (DaemonCommand, f64)>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Debouncer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_and_flush_respects_debounce() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut db = Debouncer::default();
        db.queue("k", DaemonCommand::GetDebugInfo, 10.0, 0.14);
        // Not yet due — nothing flushed, command still pending.
        db.flush(&tx, 10.0 + 0.14 - 0.01);
        assert!(rx.try_recv().is_err());
        assert!(db.contains_key("k"));
        // Past the debounce window — command is sent and removed.
        db.flush(&tx, 10.0 + 0.14);
        assert!(rx.try_recv().is_ok());
        assert!(db.is_empty());
    }

    #[test]
    fn queue_keeps_only_latest_value_per_key() {
        let mut db = Debouncer::default();
        db.queue("k", DaemonCommand::GetDebugInfo, 1.0, 0.14);
        db.queue("k", DaemonCommand::ListLcdImages, 2.0, 0.14);
        assert_eq!(db.len(), 1);
        let (_, due) = &db["k"];
        assert_eq!(*due, 2.0 + 0.14);
    }
}
