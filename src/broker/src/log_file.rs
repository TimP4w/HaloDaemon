// SPDX-License-Identifier: GPL-3.0-or-later
//! Broker diagnostics. Only the installed service writes `halod-broker.log`
//! next to the executable, whose directory is admin-only there; a foreground
//! (dev) broker would otherwise write as an elevated user into a
//! user-writable tree, so it logs to stderr only.

use std::io::Write;

use halod_hwaccess::log_file::RotatingLog;
use log::{Level, LevelFilter, Metadata, Record};

struct BrokerLogger(Option<RotatingLog>);

impl log::Log for BrokerLogger {
    fn enabled(&self, meta: &Metadata) -> bool {
        meta.level() <= Level::Debug
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("[{}] {}\n", record.level(), record.args());
        if let Some(file) = &self.0 {
            file.write_line(&line);
        }
        let _ = write!(std::io::stderr(), "{line}");
    }

    fn flush(&self) {
        if let Some(file) = &self.0 {
            file.flush();
        }
    }
}

pub fn init(file: bool) {
    let file = file
        .then(|| halod_hwaccess::winsec::installed_sibling("halod-broker.log").ok())
        .flatten()
        .map(RotatingLog::open);
    let _ = log::set_boxed_logger(Box::new(BrokerLogger(file)));
    log::set_max_level(LevelFilter::Debug);
}
