// SPDX-License-Identifier: GPL-3.0-or-later
//! Rotating daemon file logger. Mirrors the broker's bounded policy while
//! writing to the daemon's user-writable config directory.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{Level, LevelFilter, Metadata, Record};

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_ROTATED: u32 = 3;
const NOISY_TARGETS: &[&str] = &["wmi::context", "wmi::connection"];

struct LogState {
    file: File,
    written: u64,
}

struct FileLogger {
    path: PathBuf,
    state: Mutex<Option<LogState>>,
}

fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut name = base.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn should_rotate(written: u64, max: u64) -> bool {
    written >= max
}

fn shift_rotations(base: &Path, keep: u32) {
    let _ = std::fs::remove_file(rotated_path(base, keep));
    for index in (1..keep).rev() {
        let _ = std::fs::rename(rotated_path(base, index), rotated_path(base, index + 1));
    }
    let _ = std::fs::rename(base, rotated_path(base, 1));
}

fn open_log(path: &Path) -> Option<LogState> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    Some(LogState { file, written })
}

fn allowed(metadata: &Metadata<'_>) -> bool {
    metadata.level() <= Level::Debug
        && !NOISY_TARGETS
            .iter()
            .any(|target| metadata.target().starts_with(target))
}

impl FileLogger {
    fn rotate(&self) -> Option<LogState> {
        shift_rotations(&self.path, MAX_ROTATED);
        open_log(&self.path)
    }
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
        if let Ok(mut guard) = self.state.lock() {
            if let Some(state) = guard.as_mut() {
                let _ = state.file.write_all(line.as_bytes());
                let _ = state.file.flush();
                state.written += line.len() as u64;
                if should_rotate(state.written, MAX_LOG_BYTES) {
                    if let Some(fresh) = self.rotate() {
                        *state = fresh;
                    }
                }
            }
        }
        let _ = write!(std::io::stderr(), "{line}");
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.state.lock() {
            if let Some(state) = guard.as_mut() {
                let _ = state.file.flush();
            }
        }
    }
}

/// Install the daemon logger. File opening is best-effort; stderr remains
/// available if the config directory cannot be written.
pub fn init(path: PathBuf, level: LevelFilter) {
    let state = open_log(&path);
    let logger = Box::new(FileLogger {
        path,
        state: Mutex::new(state),
    });
    let _ = log::set_boxed_logger(logger);
    log::set_max_level(without_trace(level));
}

pub fn without_trace(level: LevelFilter) -> LevelFilter {
    level.min(LevelFilter::Debug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;
    use std::io::Read;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn read(path: &Path) -> String {
        let mut output = String::new();
        File::open(path)
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        output
    }

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

    #[test]
    fn shift_rotations_ages_files_out() {
        let dir = temp_dir();
        let base = dir.path().join("halod.log");
        std::fs::write(&base, b"live").unwrap();
        std::fs::write(rotated_path(&base, 1), b"one").unwrap();
        std::fs::write(rotated_path(&base, 2), b"two").unwrap();
        std::fs::write(rotated_path(&base, 3), b"three").unwrap();

        shift_rotations(&base, 3);

        assert_eq!(read(&rotated_path(&base, 1)), "live");
        assert_eq!(read(&rotated_path(&base, 2)), "one");
        assert_eq!(read(&rotated_path(&base, 3)), "two");
        assert!(!rotated_path(&base, 4).exists());
    }

    #[test]
    fn logger_rotates_when_the_cap_is_crossed() {
        let dir = temp_dir();
        let base = dir.path().join("halod.log");
        std::fs::write(&base, vec![b'x'; (MAX_LOG_BYTES - 4) as usize]).unwrap();
        let logger = FileLogger {
            path: base.clone(),
            state: Mutex::new(open_log(&base)),
        };
        let record = Record::builder()
            .level(Level::Info)
            .target("test")
            .args(format_args!("hello"))
            .build();

        logger.log(&record);

        assert!(rotated_path(&base, 1).exists());
        assert!(base.metadata().unwrap().len() < MAX_LOG_BYTES);
    }
}
