// SPDX-License-Identifier: GPL-3.0-or-later
//! The broker is launched with `CREATE_NO_WINDOW` and has no console, so its
//! op log, the record of every register-bus operation it performed, goes to
//! `halod-broker.log` next to the executable.

use std::io::Write;
use std::path::PathBuf;

use halod_hwaccess::log_file::RotatingLog;
use log::{Level, LevelFilter, Metadata, Record};

struct FileLogger(RotatingLog);

impl log::Log for FileLogger {
    fn enabled(&self, meta: &Metadata) -> bool {
        meta.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("[{}] {}\n", record.level(), record.args());
        self.0.write_line(&line);
        let _ = write!(std::io::stderr(), "{line}");
    }

    fn flush(&self) {
        self.0.flush();
    }
}

pub fn init() {
    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("halod-broker.log")))
        .unwrap_or_else(|| PathBuf::from("halod-broker.log"));
    let _ = log::set_boxed_logger(Box::new(FileLogger(RotatingLog::open(path))));
    log::set_max_level(LevelFilter::Debug);
}
