use std::time::Duration;

use gtk4::glib;

use crate::ipc::IpcSender;

pub fn start_service() {
    std::thread::spawn(|| {
        #[cfg(windows)]
        {
            std::process::Command::new("sc.exe")
                .args(["start", "HalodDaemon"])
                .status()
                .ok();
        }
        #[cfg(not(windows))]
        {
            std::process::Command::new("systemctl")
                .args(["--user", "start", "halod"])
                .status()
                .ok();
        }
    });
}

pub fn stop_service(ipc: &IpcSender) {
    ipc.send(serde_json::json!({"type": "shutdown"}));
}

pub fn restart_service(ipc: &IpcSender) {
    ipc.send(serde_json::json!({"type": "shutdown"}));
    // Allow the daemon to exit and the service manager to mark it stopped
    // before requesting a restart.
    glib::timeout_add_local_once(Duration::from_millis(1500), start_service);
}
