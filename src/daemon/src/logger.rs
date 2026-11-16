// SPDX-License-Identifier: GPL-3.0-or-later
use halod_hwaccess::log_file::RotatingLog;
use log::{Level, LevelFilter, Metadata, Record};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Diagnostic escape hatch: `HALOD_TRACE=1` lifts the Debug clamp for one run so
/// `trace!` records reach the file/stderr. Off by default; never set in normal use.
static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

const NOISY_TARGETS: &[&str] = &["wmi::context", "wmi::connection"];

struct FileLogger(RotatingLog);

fn allowed(metadata: &Metadata<'_>) -> bool {
    let ceiling = if TRACE_ENABLED.load(Ordering::Relaxed) {
        Level::Trace
    } else {
        Level::Debug
    };
    metadata.level() <= ceiling
        && !NOISY_TARGETS
            .iter()
            .any(|target| metadata.target().starts_with(target))
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        allowed(metadata)
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "[{}] {}: {}\n",
            record.level(),
            record.target(),
            record.args()
        );
        self.0.write_line(&line);
        let _ = write!(std::io::stderr(), "{line}");
    }

    fn flush(&self) {
        self.0.flush();
    }
}

/// Install the daemon logger. File opening is best-effort; stderr remains
/// available if the config directory cannot be written.
pub fn init(path: PathBuf, level: LevelFilter) {
    let _ = log::set_boxed_logger(Box::new(FileLogger(RotatingLog::open(path))));
    if std::env::var_os("HALOD_TRACE").is_some() {
        TRACE_ENABLED.store(true, Ordering::Relaxed);
        log::set_max_level(LevelFilter::Trace);
    } else {
        log::set_max_level(without_trace(level));
    }
}

pub fn without_trace(level: LevelFilter) -> LevelFilter {
    level.min(LevelFilter::Debug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_and_noisy_wmi_targets_are_excluded() {
        let metadata = |level, target| Metadata::builder().level(level).target(target).build();
        assert!(allowed(&metadata(Level::Debug, "halod")));
        assert!(!allowed(&metadata(Level::Trace, "halod")));
        assert!(!allowed(&metadata(Level::Debug, "wmi::context")));
        assert!(!allowed(&metadata(Level::Info, "wmi::connection::query")));
    }

    #[test]
    fn trace_level_is_clamped_to_debug() {
        assert_eq!(without_trace(LevelFilter::Trace), LevelFilter::Debug);
        assert_eq!(without_trace(LevelFilter::Info), LevelFilter::Info);
    }
}
