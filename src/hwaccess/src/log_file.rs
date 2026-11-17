// SPDX-License-Identifier: GPL-3.0-or-later
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_ROTATED: u32 = 3;

struct LogState {
    file: File,
    written: u64,
}

pub struct RotatingLog {
    path: PathBuf,
    state: Mutex<Option<LogState>>,
}

impl RotatingLog {
    pub fn open(path: PathBuf) -> Self {
        let state = open_log(&path);
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    pub fn write_line(&self, line: &str) {
        let mut guard = self.state.lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        let _ = state.file.write_all(line.as_bytes());
        let _ = state.file.flush();
        state.written += line.len() as u64;
        if state.written >= MAX_LOG_BYTES {
            shift_rotations(&self.path, MAX_ROTATED);
            if let Some(fresh) = open_log(&self.path) {
                *state = fresh;
            }
        }
    }

    pub fn flush(&self) {
        let mut guard = self.state.lock();
        if let Some(state) = guard.as_mut() {
            let _ = state.file.flush();
        }
    }
}

fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut name = base.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotated_path_appends_index() {
        let base = Path::new("dir/halod.log");
        assert!(rotated_path(base, 1).ends_with("halod.log.1"));
        assert!(rotated_path(base, 3).ends_with("halod.log.3"));
    }

    #[test]
    fn rotates_exactly_when_the_cap_is_reached() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("halod.log");
        std::fs::write(&base, vec![b'x'; (MAX_LOG_BYTES - 7) as usize]).unwrap();
        let log = RotatingLog::open(base.clone());

        log.write_line("hello\n");
        assert!(!rotated_path(&base, 1).exists());
        assert_eq!(base.metadata().unwrap().len(), MAX_LOG_BYTES - 1);

        log.write_line("x");
        assert!(rotated_path(&base, 1).exists());
    }

    #[test]
    fn shift_rotations_ages_files_out_and_drops_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("halod.log");
        for (index, body) in [(0, "live"), (1, "one"), (2, "two"), (3, "three")] {
            let path = if index == 0 {
                base.clone()
            } else {
                rotated_path(&base, index)
            };
            std::fs::write(path, body).unwrap();
        }

        shift_rotations(&base, MAX_ROTATED);

        assert!(!base.exists());
        assert_eq!(
            std::fs::read_to_string(rotated_path(&base, 1)).unwrap(),
            "live"
        );
        assert_eq!(
            std::fs::read_to_string(rotated_path(&base, 2)).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(rotated_path(&base, 3)).unwrap(),
            "two"
        );
        assert!(!rotated_path(&base, 4).exists());
    }

    #[test]
    fn crossing_the_cap_rotates_to_a_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("halod.log");
        std::fs::write(&base, vec![b'x'; (MAX_LOG_BYTES - 4) as usize]).unwrap();
        let log = RotatingLog::open(base.clone());

        log.write_line("hello\n");

        assert!(rotated_path(&base, 1).exists());
        assert!(base.metadata().unwrap().len() < MAX_LOG_BYTES);
    }
}
