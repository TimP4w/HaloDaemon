// SPDX-License-Identifier: GPL-3.0-or-later
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. Saturates to 0 if the clock is before the
/// epoch (only possible with a badly misconfigured clock).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
