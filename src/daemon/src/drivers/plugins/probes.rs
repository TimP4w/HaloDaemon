// SPDX-License-Identifier: GPL-3.0-or-later
//! Host-capability probes behind injectable interfaces, so requirement
//! evaluation is unit-testable without touching `/sys`, `PATH`, or spawning
//! helpers. Command resolution lives in `command_resolve`; this module covers
//! kernel modules and inferred PawnIO, Linux I2C, and hwmon readiness.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use halod_shared::types::RequirementFailureReason;

/// Access to Linux kernel-module state, injected so tests need no real kernel.
pub trait ModuleEnv {
    /// Whether `/sys/module/<name>` exists — the module is loaded or built in.
    fn loaded(&self, name: &str) -> bool;
    /// Whether the module resolves for the running kernel via a bounded,
    /// non-mutating `modprobe --dry-run`. Never loads it: loading needs
    /// privilege the daemon must not assume.
    fn available(&self, name: &str) -> bool;
}

/// Production module probe: reads `/sys/module` and runs `modprobe --dry-run`.
pub struct RealModuleEnv;

const MODULE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

impl ModuleEnv for RealModuleEnv {
    fn loaded(&self, name: &str) -> bool {
        // sysfs canonicalizes module '-' characters to '_'. `modprobe`
        // accepts either spelling, so use the same identity for both checks.
        Path::new("/sys/module")
            .join(name.replace('-', "_"))
            .is_dir()
    }

    fn available(&self, name: &str) -> bool {
        let Ok(mut child) = Command::new("modprobe")
            .args(["--dry-run", "--quiet", name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if started.elapsed() < MODULE_PROBE_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }
}

/// Evaluate a kernel-module requirement: loaded/built-in → satisfied; available
/// but not loaded → `NotLoaded` (the UI suggests `sudo modprobe <name>`);
/// otherwise `Unavailable` (no such module for the running kernel).
pub fn probe_module(env: &dyn ModuleEnv, name: &str) -> (bool, Option<RequirementFailureReason>) {
    if env.loaded(name) {
        (true, None)
    } else if env.available(name) {
        (false, Some(RequirementFailureReason::NotLoaded))
    } else {
        (false, Some(RequirementFailureReason::Unavailable))
    }
}

/// Check the single PawnIO installation gate shared by every bundled Windows
/// hardware-access module.
#[cfg(windows)]
pub fn probe_pawnio() -> (bool, Option<RequirementFailureReason>) {
    if halod_hwaccess::pawnio::installation_present() {
        (true, None)
    } else {
        (false, Some(RequirementFailureReason::NotInstalled))
    }
}

#[cfg(not(windows))]
pub fn probe_pawnio() -> (bool, Option<RequirementFailureReason>) {
    (false, Some(RequirementFailureReason::Unavailable))
}

/// A Linux SMBus plugin needs at least one read/write-openable i2c-dev node.
/// This single check covers both a missing `i2c-dev` module and permissions.
#[cfg(target_os = "linux")]
pub fn probe_linux_i2c() -> (bool, Option<RequirementFailureReason>) {
    probe_linux_i2c_at(Path::new("/dev"), &|path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .is_ok()
    })
}

#[cfg(target_os = "linux")]
fn probe_linux_i2c_at(
    root: &Path,
    openable: &dyn Fn(&Path) -> bool,
) -> (bool, Option<RequirementFailureReason>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return (false, Some(RequirementFailureReason::NotFound));
    };
    let nodes = entries.flatten().filter(|entry| {
        entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("i2c-"))
            .is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            })
    });
    let mut found = false;
    for node in nodes {
        found = true;
        if openable(&node.path()) {
            return (true, None);
        }
    }
    if found {
        (false, Some(RequirementFailureReason::PermissionDenied))
    } else {
        (false, Some(RequirementFailureReason::NotFound))
    }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_linux_i2c() -> (bool, Option<RequirementFailureReason>) {
    (false, Some(RequirementFailureReason::Unavailable))
}

/// Probe Linux hwmon readiness. Read access is blocking because the integration
/// cannot discover sensors without it. PWM access is inferred as degrading: no
/// PWM attributes means there is nothing to control, while existing but
/// non-writable attributes indicate a permissions problem.
#[cfg(target_os = "linux")]
pub fn probe_linux_hwmon(access: &str) -> (bool, Option<RequirementFailureReason>) {
    probe_linux_hwmon_at(Path::new("/sys/class/hwmon"), access, &can_access)
}

#[cfg(target_os = "linux")]
fn probe_linux_hwmon_at(
    root: &Path,
    access: &str,
    accessible: &dyn Fn(&Path, libc::c_int) -> bool,
) -> (bool, Option<RequirementFailureReason>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (false, Some(RequirementFailureReason::NotFound));
        }
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return (false, Some(RequirementFailureReason::PermissionDenied));
        }
        Err(_) => return (false, Some(RequirementFailureReason::Unavailable)),
    };
    if entries.is_empty() {
        return (false, Some(RequirementFailureReason::NotFound));
    }

    if access == "read" {
        let readable = entries.iter().any(|dir| {
            std::fs::read_dir(dir).is_ok()
                && (accessible(&dir.join("name"), libc::R_OK)
                    || std::fs::read_dir(dir).is_ok_and(|files| {
                        files.flatten().any(|file| {
                            file.file_name()
                                .to_str()
                                .is_some_and(|name| name.ends_with("_input"))
                                && accessible(&file.path(), libc::R_OK)
                        })
                    }))
        });
        return if readable {
            (true, None)
        } else {
            (false, Some(RequirementFailureReason::PermissionDenied))
        };
    }

    let pwm_files = entries
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|files| files.flatten())
        .filter(|file| {
            file.file_name().to_str().is_some_and(|name| {
                name.strip_prefix("pwm").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                })
            })
        })
        .map(|file| file.path())
        .collect::<Vec<_>>();
    if pwm_files.is_empty()
        || pwm_files.iter().any(|path| {
            let enable = path.with_file_name(format!(
                "{}_enable",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            accessible(path, libc::R_OK | libc::W_OK)
                && (!enable.is_file() || accessible(&enable, libc::R_OK | libc::W_OK))
        })
    {
        (true, None)
    } else {
        (false, Some(RequirementFailureReason::PermissionDenied))
    }
}

#[cfg(target_os = "linux")]
fn can_access(path: &Path, mode: libc::c_int) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is NUL-terminated and remains alive for the call.
    unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

#[cfg(not(target_os = "linux"))]
pub fn probe_linux_hwmon(_: &str) -> (bool, Option<RequirementFailureReason>) {
    (false, Some(RequirementFailureReason::Unavailable))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        loaded: bool,
        available: bool,
    }
    impl ModuleEnv for Fake {
        fn loaded(&self, _: &str) -> bool {
            self.loaded
        }
        fn available(&self, _: &str) -> bool {
            self.available
        }
    }

    #[test]
    fn loaded_module_is_satisfied() {
        let (ok, reason) = probe_module(
            &Fake {
                loaded: true,
                available: false,
            },
            "nct6775",
        );
        assert!(ok);
        assert!(reason.is_none());
    }

    #[test]
    fn available_but_not_loaded_reports_not_loaded() {
        let (ok, reason) = probe_module(
            &Fake {
                loaded: false,
                available: true,
            },
            "nct6775",
        );
        assert!(!ok);
        assert_eq!(reason, Some(RequirementFailureReason::NotLoaded));
    }

    #[test]
    fn unavailable_module_reports_unavailable() {
        let (ok, reason) = probe_module(
            &Fake {
                loaded: false,
                available: false,
            },
            "nct6775",
        );
        assert!(!ok);
        assert_eq!(reason, Some(RequirementFailureReason::Unavailable));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn i2c_distinguishes_absence_from_permissions() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_linux_i2c_at(root.path(), &|_| false),
            (false, Some(RequirementFailureReason::NotFound))
        );
        std::fs::write(root.path().join("i2c-0"), "").unwrap();
        assert_eq!(
            probe_linux_i2c_at(root.path(), &|_| false),
            (false, Some(RequirementFailureReason::PermissionDenied))
        );
        assert_eq!(
            probe_linux_i2c_at(root.path(), &|path| path.ends_with("i2c-0")),
            (true, None)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hwmon_read_requires_a_readable_sensor_class() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_linux_hwmon_at(root.path(), "read", &|_, _| false),
            (false, Some(RequirementFailureReason::NotFound))
        );

        let chip = root.path().join("hwmon0");
        std::fs::create_dir(&chip).unwrap();
        std::fs::write(chip.join("temp1_input"), "42000\n").unwrap();
        assert_eq!(
            probe_linux_hwmon_at(root.path(), "read", &|path, _| path
                .ends_with("temp1_input")),
            (true, None)
        );
        assert_eq!(
            probe_linux_hwmon_at(root.path(), "read", &|_, _| false),
            (false, Some(RequirementFailureReason::PermissionDenied))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn hwmon_pwm_only_fails_when_present_but_not_writable() {
        let root = tempfile::tempdir().unwrap();
        let chip = root.path().join("hwmon0");
        std::fs::create_dir(&chip).unwrap();
        assert_eq!(
            probe_linux_hwmon_at(root.path(), "pwm", &|_, _| false),
            (true, None),
            "no PWM attributes means there is no fan-control permission to grant"
        );

        std::fs::write(chip.join("pwm1"), "128\n").unwrap();
        std::fs::write(chip.join("pwm1_enable"), "1\n").unwrap();
        assert_eq!(
            probe_linux_hwmon_at(root.path(), "pwm", &|_, _| false),
            (false, Some(RequirementFailureReason::PermissionDenied))
        );
        assert_eq!(
            probe_linux_hwmon_at(root.path(), "pwm", &|path, mode| {
                matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("pwm1" | "pwm1_enable")
                ) && mode == libc::R_OK | libc::W_OK
            }),
            (true, None)
        );
    }
}
