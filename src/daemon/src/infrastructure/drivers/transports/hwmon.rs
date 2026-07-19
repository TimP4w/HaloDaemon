// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "linux")]

use anyhow::{Context, Result};
use halod_shared::types::{WriteRateLimit, WriteRateStatus};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::infrastructure::drivers::Metered;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HwmonChipInfo {
    pub key: String,
    pub stable_id: String,
    pub name: String,
    pub attributes: Vec<String>,
    /// Allowlisted attributes the daemon process can currently write.
    pub writable_attributes: Vec<String>,
}

#[derive(Clone)]
struct HwmonChip {
    info: HwmonChipInfo,
    storage: HwmonStorage,
}

#[derive(Clone)]
enum HwmonStorage {
    Fs(PathBuf),
    #[cfg(feature = "plugin-test")]
    Memory(Arc<Mutex<HashMap<String, String>>>),
}

impl HwmonChip {
    fn read(&self, attribute: &str) -> std::io::Result<String> {
        match &self.storage {
            HwmonStorage::Fs(path) => std::fs::read_to_string(path.join(attribute)),
            #[cfg(feature = "plugin-test")]
            HwmonStorage::Memory(values) => values
                .lock()
                .unwrap()
                .get(attribute)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound)),
        }
    }

    fn write(&self, attribute: &str, value: String) -> std::io::Result<()> {
        match &self.storage {
            HwmonStorage::Fs(path) => std::fs::write(path.join(attribute), value),
            #[cfg(feature = "plugin-test")]
            HwmonStorage::Memory(values) => {
                let mut values = values.lock().unwrap();
                if !values.contains_key(attribute) {
                    return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
                }
                values.insert(attribute.to_owned(), value);
                Ok(())
            }
        }
    }
}

/// A scoped view of Linux's hwmon class for an integration plugin. Scripts see
/// opaque keys and allowlisted attribute names, never filesystem paths.
#[derive(Clone)]
pub struct HwmonTransport {
    chips: Metered<HashMap<String, HwmonChip>>,
    original_pwm_enable: Arc<Mutex<HashMap<(String, String), String>>>,
    unrecoverable: Arc<Mutex<Option<String>>>,
}

impl HwmonTransport {
    pub fn discover(limit: Option<WriteRateLimit>) -> Result<Self> {
        Self::discover_at(Path::new("/sys/class/hwmon"), limit)
    }

    fn discover_at(root: &Path, limit: Option<WriteRateLimit>) -> Result<Self> {
        let mut paths: Vec<PathBuf> = match std::fs::read_dir(root) {
            Ok(entries) => entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("hwmon"))
                .map(|entry| entry.path())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", root.display())),
        };
        paths.sort();

        let mut chips = HashMap::new();
        for (index, path) in paths.into_iter().enumerate() {
            let stable_id = stable_id(&path);
            let name = std::fs::read_to_string(path.join("name"))
                .unwrap_or_default()
                .trim()
                .to_owned();
            let name = if name.is_empty() {
                stable_id.clone()
            } else {
                name
            };
            let mut attributes: Vec<String> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_name().into_string().ok())
                .filter(|attribute| readable_attribute(attribute))
                .collect();
            attributes.sort();
            attributes.dedup();
            let writable_attributes = attributes
                .iter()
                .filter(|attribute| {
                    writable_attribute(attribute) && path_writable(&path.join(attribute))
                })
                .cloned()
                .collect();
            let key = index.to_string();
            let info = HwmonChipInfo {
                key: key.clone(),
                stable_id,
                name,
                attributes,
                writable_attributes,
            };
            chips.insert(
                key,
                HwmonChip {
                    info,
                    storage: HwmonStorage::Fs(path),
                },
            );
        }

        Ok(Self {
            chips: Metered::new(chips, limit),
            original_pwm_enable: Arc::new(Mutex::new(HashMap::new())),
            unrecoverable: Arc::new(Mutex::new(None)),
        })
    }

    #[cfg(feature = "plugin-test")]
    pub fn from_fixture(
        fixtures: Vec<(String, String, HashMap<String, String>, Vec<String>)>,
    ) -> Self {
        let chips = fixtures
            .into_iter()
            .enumerate()
            .map(
                |(index, (stable_id, name, mut values, mut writable_attributes))| {
                    values.insert("name".to_owned(), format!("{name}\n"));
                    let key = index.to_string();
                    let mut attributes: Vec<_> = values
                        .keys()
                        .filter(|attribute| readable_attribute(attribute))
                        .cloned()
                        .collect();
                    attributes.sort();
                    writable_attributes.retain(|attribute| {
                        attributes.contains(attribute) && writable_attribute(attribute)
                    });
                    writable_attributes.sort();
                    writable_attributes.dedup();
                    let info = HwmonChipInfo {
                        key: key.clone(),
                        stable_id,
                        name,
                        attributes,
                        writable_attributes,
                    };
                    (
                        key,
                        HwmonChip {
                            info,
                            storage: HwmonStorage::Memory(Arc::new(Mutex::new(values))),
                        },
                    )
                },
            )
            .collect();
        Self {
            chips: Metered::new(chips, None),
            original_pwm_enable: Arc::new(Mutex::new(HashMap::new())),
            unrecoverable: Arc::new(Mutex::new(None)),
        }
    }

    pub fn list(&self) -> Vec<HwmonChipInfo> {
        let mut chips: Vec<_> = self
            .chips
            .read_access()
            .values()
            .map(|chip| chip.info.clone())
            .collect();
        chips.sort_by(|a, b| a.key.cmp(&b.key));
        chips
    }

    pub fn read(&self, key: &str, attribute: &str) -> Result<Option<String>> {
        if !readable_attribute(attribute) {
            return self.reject(format!("unsupported hwmon attribute '{attribute}'"));
        }
        let chip = self
            .chips
            .read_access()
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("unknown hwmon device key"))?;
        if !chip.info.attributes.iter().any(|item| item == attribute) {
            return Ok(None);
        }
        match chip.read(attribute) {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                let detail = format!("reading hwmon attribute {attribute}: {error}");
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    self.unrecoverable
                        .lock()
                        .unwrap()
                        .get_or_insert(detail.clone());
                }
                Err(error).with_context(|| format!("reading hwmon attribute {attribute}"))
            }
        }
    }

    pub fn write(&self, key: &str, attribute: &str, value: &str) -> Result<()> {
        if !writable_attribute(attribute) {
            return self.reject(format!("hwmon attribute '{attribute}' is read-only"));
        }
        if value.is_empty() || value.len() > 32 || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return self.reject("hwmon value must be a bounded unsigned integer".into());
        }
        let chips = self.chips.write_access_blocking(value.len())?;
        let chip = chips
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("unknown hwmon device key"))?;
        if !chip.info.attributes.iter().any(|item| item == attribute) {
            return self.reject(format!(
                "hwmon attribute '{attribute}' is unavailable on this device"
            ));
        }
        if attribute.ends_with("_enable") {
            let mut originals = self.original_pwm_enable.lock().unwrap();
            let restore_key = (key.to_owned(), attribute.to_owned());
            if let std::collections::hash_map::Entry::Vacant(entry) = originals.entry(restore_key) {
                let original = chip.read(attribute).map_err(|error| {
                    let detail = format!("reading original hwmon attribute {attribute}: {error}");
                    if error.kind() == std::io::ErrorKind::PermissionDenied {
                        self.unrecoverable
                            .lock()
                            .unwrap()
                            .get_or_insert(detail.clone());
                    }
                    anyhow::anyhow!(detail)
                })?;
                entry.insert(original);
            }
        }
        chip.write(attribute, value.to_owned())
            .map_err(|error| self.write_error(key, attribute, error))
    }

    fn reject<T>(&self, detail: String) -> Result<T> {
        self.unrecoverable
            .lock()
            .unwrap()
            .get_or_insert(detail.clone());
        anyhow::bail!(detail)
    }

    pub fn unrecoverable_error(&self) -> Option<String> {
        self.unrecoverable.lock().unwrap().clone()
    }

    /// Convert a failed write without turning a readable hwmon device into an
    /// unrecoverable transport. If switching a PWM to manual mode never
    /// succeeded, there is no state to restore; retaining its saved value would
    /// make every later cleanup pass retry the same denied write.
    fn write_error(&self, key: &str, attribute: &str, error: std::io::Error) -> anyhow::Error {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            self.original_pwm_enable
                .lock()
                .unwrap()
                .remove(&(key.to_owned(), attribute.to_owned()));
            anyhow::anyhow!(
                "writing hwmon attribute {attribute}: permission denied; add the daemon user to the 'halod' group, reload the installed udev rules, and run `sudo udevadm trigger --action=change --subsystem-match=hwmon`"
            )
        } else {
            anyhow::Error::new(error).context(format!("writing hwmon attribute {attribute}"))
        }
    }

    pub fn restore(&self) -> Result<()> {
        let originals = std::mem::take(&mut *self.original_pwm_enable.lock().unwrap());
        let chips = self.chips.read_access();
        let mut first_error = None;
        let mut pending = HashMap::new();
        for ((key, attribute), value) in originals {
            let result = chips
                .get(&key)
                .ok_or_else(|| anyhow::anyhow!("hwmon device disappeared during restore"))
                .and_then(|chip| {
                    chip.write(&attribute, value.clone())
                        .with_context(|| format!("restoring hwmon attribute {attribute}"))
                });
            if let Err(error) = result {
                first_error.get_or_insert(error);
                pending.insert((key, attribute), value);
            }
        }
        if !pending.is_empty() {
            self.original_pwm_enable.lock().unwrap().extend(pending);
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.chips.status()
    }
}

pub fn stable_id(path: &Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let value = canonical.to_string_lossy();
    let relative = value
        .strip_prefix("/sys/devices/")
        .unwrap_or(value.as_ref());
    let without_last = relative
        .rfind('/')
        .map_or(relative, |position| &relative[..position]);
    let base = without_last.strip_suffix("/hwmon").unwrap_or(without_last);
    let mut result = String::with_capacity(base.len());
    let mut underscore = false;
    for character in base.chars() {
        if character.is_ascii_alphanumeric() {
            result.push(character);
            underscore = false;
        } else if !underscore {
            result.push('_');
            underscore = true;
        }
    }
    result
}

fn numbered_attribute(attribute: &str, prefix: &str, suffixes: &[&str]) -> bool {
    let Some(rest) = attribute.strip_prefix(prefix) else {
        return false;
    };
    suffixes.iter().any(|suffix| {
        rest.strip_suffix(suffix)
            .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn readable_attribute(attribute: &str) -> bool {
    attribute == "name"
        || numbered_attribute(attribute, "temp", &["_input", "_label"])
        || numbered_attribute(attribute, "fan", &["_input", "_label"])
        || numbered_attribute(attribute, "pwm", &["", "_enable"])
}

fn writable_attribute(attribute: &str) -> bool {
    numbered_attribute(attribute, "pwm", &["", "_enable"])
}

fn path_writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `path` is NUL-terminated and remains alive for the call.
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, HwmonTransport) {
        let root = tempfile::tempdir().unwrap();
        let chip = root.path().join("hwmon0");
        std::fs::create_dir(&chip).unwrap();
        std::fs::write(chip.join("name"), "nct6798\n").unwrap();
        std::fs::write(chip.join("temp1_input"), "42000\n").unwrap();
        std::fs::write(chip.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(chip.join("pwm1"), "128\n").unwrap();
        std::fs::write(chip.join("pwm1_enable"), "2\n").unwrap();
        let transport = HwmonTransport::discover_at(root.path(), None).unwrap();
        (root, transport)
    }

    #[test]
    fn stable_id_strips_hwmon_suffix() {
        let path = Path::new("/sys/devices/pci0000:00/0000:00:18.3/hwmon/hwmon6");
        assert_eq!(stable_id(path), "pci0000_00_0000_00_18_3");
    }

    #[test]
    fn stable_id_strips_direct_hwmon_suffix() {
        let path = Path::new("/sys/devices/pci0000:00/0000:00:01.2/0000:02:00.0/nvme/nvme0/hwmon0");
        assert_eq!(
            stable_id(path),
            "pci0000_00_0000_00_01_2_0000_02_00_0_nvme_nvme0"
        );
    }

    #[test]
    fn exposes_only_allowlisted_attributes() {
        let (root, transport) = fixture();
        let chip = root.path().join("hwmon0");
        std::fs::write(chip.join("uevent"), "SECRET\n").unwrap();
        let listed = transport.list();
        assert!(!listed[0].attributes.iter().any(|item| item == "uevent"));
        assert!(transport.read("0", "../name").is_err());
        assert!(transport.write("0", "temp1_input", "1").is_err());
        assert!(transport.write("missing", "pwm1", "1").is_err());
    }

    #[test]
    fn invalid_plugin_write_latches_an_unrecoverable_error() {
        let (_root, transport) = fixture();
        assert!(transport.write("0", "temp1_input", "1").is_err());
        assert_eq!(
            transport.unrecoverable_error().as_deref(),
            Some("hwmon attribute 'temp1_input' is read-only")
        );
    }

    #[test]
    fn denied_pwm_write_does_not_latch_an_unrecoverable_error() {
        let (_root, transport) = fixture();
        transport
            .original_pwm_enable
            .lock()
            .unwrap()
            .insert(("0".to_owned(), "pwm1_enable".to_owned()), "2\n".to_owned());
        let error = transport.write_error(
            "0",
            "pwm1_enable",
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );

        assert!(error.to_string().contains("permission denied"));
        assert_eq!(transport.unrecoverable_error(), None);
        assert!(transport.original_pwm_enable.lock().unwrap().is_empty());
        assert!(transport.restore().is_ok());
        assert_eq!(
            transport.read("0", "pwm1").unwrap().as_deref(),
            Some("128\n")
        );
    }

    #[test]
    fn writes_are_metered_and_pwm_enable_is_restored() {
        let (root, transport) = fixture();
        transport.write("0", "pwm1_enable", "1").unwrap();
        transport.write("0", "pwm1_enable", "1").unwrap();
        assert!(transport.rate_status().current_bytes_per_sec > 0.0);
        transport.restore().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("hwmon0/pwm1_enable")).unwrap(),
            "2\n"
        );
    }
}
