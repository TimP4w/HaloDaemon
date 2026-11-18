// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use log::{LevelFilter, Metadata, Record};

struct FileLogger(Mutex<File>);

impl log::Log for FileLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if let Ok(mut file) = self.0.lock() {
            let _ = writeln!(file, "[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.0.lock() {
            let _ = file.flush();
        }
    }
}

pub fn init(path: &Path) {
    if let Ok(file) = File::create(path) {
        let _ = log::set_boxed_logger(Box::new(FileLogger(Mutex::new(file))));
        log::set_max_level(LevelFilter::Info);
    }
}
