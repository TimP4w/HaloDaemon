// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal file-backed logger. The broker is launched with `CREATE_NO_WINDOW`
//! and has no console to print to, so its op log — the security-relevant record
//! of every register-bus operation it performed — is appended to
//! `halod-broker.log` next to the executable. No extra logging crate: keeping
//! the broker's dependency tree tiny is deliberate.

use std::io::Write;
use std::sync::Mutex;

use log::{Level, LevelFilter, Metadata, Record};

struct FileLogger {
    file: Mutex<Option<std::fs::File>>,
}

impl log::Log for FileLogger {
    fn enabled(&self, meta: &Metadata) -> bool {
        meta.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("[{}] {}\n", record.level(), record.args());
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
        // Also to stderr, harmless if nobody is attached.
        let _ = write!(std::io::stderr(), "{line}");
    }

    fn flush(&self) {}
}

/// Install the file logger. Best-effort: if the log file cannot be opened, the
/// broker still runs (logging only to stderr).
pub fn init() {
    let file = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("halod-broker.log")))
        .and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
    let logger = Box::new(FileLogger {
        file: Mutex::new(file),
    });
    // Ignore an error here — a second init (e.g. in tests) is harmless.
    let _ = log::set_boxed_logger(logger);
    log::set_max_level(LevelFilter::Debug);
}
