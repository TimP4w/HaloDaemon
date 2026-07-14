// SPDX-License-Identifier: GPL-3.0-or-later
//! Minimal file-backed logger. The broker is launched with `CREATE_NO_WINDOW`
//! and has no console to print to, so its op log — the security-relevant record
//! of every register-bus operation it performed — is appended to
//! `halod-broker.log` next to the executable. No extra logging crate: keeping
//! the broker's dependency tree tiny is deliberate.
//!
//! The file is size-rotated at runtime (the service can stay up for the whole
//! session while a device holds a register bus open), so it stays bounded at
//! roughly `(MAX_ROTATED + 1) * MAX_LOG_BYTES`. Rotation drops the oldest lines
//! by design — that bound is the point.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::{Level, LevelFilter, Metadata, Record};

/// Rotate the active log once it reaches this size.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Keep this many rotated files (`halod-broker.log.1` .. `.N`).
const MAX_ROTATED: u32 = 3;

struct LogState {
    file: File,
    written: u64,
}

struct FileLogger {
    path: PathBuf,
    state: Mutex<Option<LogState>>,
}

/// `base` with a numeric rotation suffix, e.g. `halod-broker.log.1`.
fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut name = base.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn should_rotate(written: u64, max: u64) -> bool {
    written >= max
}

/// Age out the rotated files: drop `.{keep}`, shift `.{i}` → `.{i+1}`, and move
/// the live file to `.1`. Best-effort — a failed rename just means an older
/// line survives a bit longer, never a crash.
fn shift_rotations(base: &Path, keep: u32) {
    let _ = std::fs::remove_file(rotated_path(base, keep));
    for index in (1..keep).rev() {
        let _ = std::fs::rename(rotated_path(base, index), rotated_path(base, index + 1));
    }
    let _ = std::fs::rename(base, rotated_path(base, 1));
}

/// Open (creating/appending) the log at `path`, seeding the byte counter from
/// the current file length so an already-large file rotates promptly.
fn open_log(path: &Path) -> Option<LogState> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    Some(LogState { file, written })
}

impl FileLogger {
    fn rotate(&self) -> Option<LogState> {
        shift_rotations(&self.path, MAX_ROTATED);
        open_log(&self.path)
    }
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
        if let Ok(mut guard) = self.state.lock() {
            if let Some(st) = guard.as_mut() {
                let _ = st.file.write_all(line.as_bytes());
                let _ = st.file.flush();
                st.written += line.len() as u64;
                if should_rotate(st.written, MAX_LOG_BYTES) {
                    if let Some(fresh) = self.rotate() {
                        *st = fresh;
                    }
                }
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
    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("halod-broker.log")))
        .unwrap_or_else(|| PathBuf::from("halod-broker.log"));
    let state = open_log(&path);
    let logger = Box::new(FileLogger {
        path,
        state: Mutex::new(state),
    });
    // Ignore an error here — a second init (e.g. in tests) is harmless.
    let _ = log::set_boxed_logger(logger);
    log::set_max_level(LevelFilter::Debug);
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;
    use std::io::Read;

    fn temp_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("halod-broker-log-test-{:?}", unique()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    // A process-unique counter avoids `Instant`/`SystemTime` and keeps parallel
    // test runs from colliding on a directory name.
    fn unique() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        std::process::id() as u64 * 1_000_000 + N.fetch_add(1, Ordering::Relaxed)
    }

    fn read(path: &Path) -> String {
        let mut s = String::new();
        File::open(path).unwrap().read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn rotated_path_appends_index() {
        let base = Path::new("dir/halod-broker.log");
        assert!(rotated_path(base, 1).ends_with("halod-broker.log.1"));
        assert!(rotated_path(base, 3).ends_with("halod-broker.log.3"));
    }

    #[test]
    fn should_rotate_at_or_over_cap() {
        assert!(!should_rotate(0, 10));
        assert!(!should_rotate(9, 10));
        assert!(should_rotate(10, 10));
        assert!(should_rotate(11, 10));
    }

    #[test]
    fn shift_rotations_ages_files_out() {
        let dir = temp_dir();
        let base = dir.join("halod-broker.log");
        std::fs::write(&base, b"live").unwrap();
        std::fs::write(rotated_path(&base, 1), b"one").unwrap();
        std::fs::write(rotated_path(&base, 2), b"two").unwrap();

        shift_rotations(&base, 3);

        assert!(!base.exists(), "live file moved to .1");
        assert_eq!(read(&rotated_path(&base, 1)), "live");
        assert_eq!(read(&rotated_path(&base, 2)), "one");
        assert_eq!(read(&rotated_path(&base, 3)), "two");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shift_rotations_drops_the_oldest() {
        let dir = temp_dir();
        let base = dir.join("halod-broker.log");
        std::fs::write(&base, b"live").unwrap();
        std::fs::write(rotated_path(&base, 1), b"one").unwrap();
        std::fs::write(rotated_path(&base, 2), b"two").unwrap();
        std::fs::write(rotated_path(&base, 3), b"three").unwrap();

        shift_rotations(&base, 3);

        // The former ".3" (oldest) is gone; nothing beyond ".3" is created.
        assert!(!rotated_path(&base, 4).exists());
        assert_eq!(read(&rotated_path(&base, 3)), "two");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn logger_rotates_when_the_cap_is_crossed() {
        let dir = temp_dir();
        let base = dir.join("halod-broker.log");
        // Seed the live file just under the cap so the next line trips rotation.
        let filler = vec![b'x'; (MAX_LOG_BYTES - 4) as usize];
        std::fs::write(&base, &filler).unwrap();

        let logger = FileLogger {
            path: base.clone(),
            state: Mutex::new(open_log(&base)),
        };
        let record = Record::builder()
            .level(Level::Info)
            .args(format_args!("hello"))
            .build();
        logger.log(&record);

        // The oversized file became ".1"; the live file is fresh and small.
        assert!(rotated_path(&base, 1).exists());
        assert!(base.metadata().unwrap().len() < MAX_LOG_BYTES);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
