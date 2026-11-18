// SPDX-License-Identifier: GPL-3.0-or-later
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use halod_shared::update::{update_dir, UpdaterEvent};

use super::UpdaterPhase;
use crate::domain::lifecycle::sibling_path;

const UPDATER_EXE: &str = "halod-updater.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct Run {
    child: Child,
    stopped: Arc<AtomicBool>,
}

impl Run {
    pub fn stop(mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn updater_present() -> bool {
    let exe = std::env::current_exe().ok();
    sibling_path(exe.as_deref(), UPDATER_EXE).is_some()
}

pub fn remove_leftover_installer() {
    if let Some(dir) = update_dir() {
        let _ = std::fs::remove_file(dir.join(halod_shared::app::INSTALLER_ASSET));
    }
}

pub fn start(
    version: &str,
    phase: Arc<Mutex<UpdaterPhase>>,
    ctx: egui::Context,
) -> std::io::Result<Run> {
    let not_found = || std::io::Error::from(std::io::ErrorKind::NotFound);
    let gui = std::env::current_exe()?;
    let source = sibling_path(Some(&gui), UPDATER_EXE).ok_or_else(not_found)?;
    let dir = update_dir().ok_or_else(not_found)?;
    std::fs::create_dir_all(&dir)?;
    let updater = dir.join(UPDATER_EXE);
    if let Err(e) = std::fs::copy(source, &updater) {
        if !updater.exists() {
            return Err(e);
        }
        log::warn!("reusing the previous updater copy: {e}");
    }
    let mut child = Command::new(&updater)
        .arg("--version")
        .arg(version)
        .arg("--relaunch")
        .arg(&gui)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or_else(not_found)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let reader_stopped = stopped.clone();
    let advance = move |next: &dyn Fn(UpdaterPhase) -> UpdaterPhase| {
        if let Ok(mut current) = phase.lock() {
            if !reader_stopped.load(Ordering::SeqCst) {
                *current = next(*current);
            }
        }
        ctx.request_repaint();
    };
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(event) = UpdaterEvent::parse(&line) {
                advance(&|current| current.after(event));
            }
        }
        advance(&UpdaterPhase::on_exit);
    });
    Ok(Run { child, stopped })
}
