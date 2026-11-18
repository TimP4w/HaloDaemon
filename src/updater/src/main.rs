// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use std::io::Write;

    use halod_shared::update::{update_dir, UpdaterEvent};

    if let Some(dir) = update_dir() {
        let _ = std::fs::create_dir_all(&dir);
        halod_updater::log_file::init(&dir.join("updater.log"));
    }
    let mut stdout = std::io::stdout();
    let mut emit = |event: UpdaterEvent| {
        let _ = writeln!(stdout, "{}", event.encode());
        let _ = stdout.flush();
    };
    let args = match halod_updater::args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            log::error!("{e:#}");
            std::process::exit(2);
        }
    };
    log::info!("updating to {}", args.version);
    if let Err(error) = halod_updater::run::run(&args, &mut emit) {
        log::error!("update failed: {error:?}");
        emit(UpdaterEvent::Error { error });
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("halod-updater is a Windows-only component; nothing to do on this platform");
}
