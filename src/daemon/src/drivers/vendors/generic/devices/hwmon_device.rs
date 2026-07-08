#![cfg(target_os = "linux")]

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::transports::hwmon::HwmonIo;
use crate::drivers::{
    vendors::generic::devices::common::TaskHandle, CapabilityRef, Device, FanCapability,
    FanStateSlot, SensorCapability, VisibilitySlot,
};
use halod_shared::types::{DeviceType, Sensor, SensorUnit};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

pub struct HwmonDevice {
    io: HwmonIo,
    chip_name: String,
    stable_id: String,
    id: String,
    cached_sensors: Arc<Mutex<Vec<Sensor>>>,
    visibility: VisibilitySlot,
    poll_task: Mutex<Option<TaskHandle>>,
}

impl HwmonDevice {
    pub fn new(path: std::path::PathBuf) -> Self {
        let io = HwmonIo::new(path, None);
        let chip_name = io.read_attr("name").unwrap_or_default().trim().to_string();
        let stable_id = Self::compute_stable_id(io.dir());
        let chip_name = if chip_name.is_empty() {
            stable_id.clone()
        } else {
            chip_name
        };
        let id = format!("hwmon_{}", stable_id);
        Self {
            io,
            chip_name,
            stable_id,
            id,
            cached_sensors: Arc::new(Mutex::new(vec![])),
            visibility: VisibilitySlot::default(),
            poll_task: Mutex::new(None),
        }
    }

    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }

    /// Stable ID from the canonical sysfs path, independent of the dynamic hwmonN index.
    fn compute_stable_id(path: &std::path::Path) -> String {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let s = canonical.to_string_lossy();
        let relative = s.strip_prefix("/sys/devices/").unwrap_or(s.as_ref());
        let without_last = match relative.rfind('/') {
            Some(pos) => &relative[..pos],
            None => relative,
        };
        let base = without_last.strip_suffix("/hwmon").unwrap_or(without_last);
        // Replace non-alphanumeric chars with `_` and collapse consecutive `_`
        // to avoid double underscores (e.g. when `/` and `.` are adjacent).
        let mut result = String::with_capacity(base.len());
        let mut prev_underscore = false;
        for c in base.chars() {
            if c.is_ascii_alphanumeric() {
                result.push(c);
                prev_underscore = false;
            } else if !prev_underscore {
                result.push('_');
                prev_underscore = true;
            }
        }
        result
    }

    fn read_sensors(path: &std::path::Path, stable_id: &str) -> Vec<Sensor> {
        let mut sensors = vec![];
        let mut i = 1u32;
        loop {
            let input_path = path.join(format!("temp{}_input", i));
            if !input_path.exists() {
                break;
            }
            let raw = match std::fs::read_to_string(&input_path)
                .ok()
                .and_then(|s| s.trim().parse::<f64>().ok())
            {
                Some(v) => v,
                None => {
                    log::trace!("[HwmonDevice] failed to read temp{i} for {stable_id}, skipping");
                    i += 1;
                    continue;
                }
            };
            let label = std::fs::read_to_string(path.join(format!("temp{}_label", i)))
                .unwrap_or_default()
                .trim()
                .to_string();
            sensors.push(Sensor {
                id: format!("hwmon_{}_temp{}", stable_id, i),
                name: label,
                value: raw / 1000.0,
                unit: SensorUnit::Celsius,
                sensor_type: halod_shared::types::SensorType::Temperature,
                visibility: Default::default(),
            });
            i += 1;
        }
        sensors
    }
}

#[async_trait]
impl Device for HwmonDevice {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.chip_name
    }
    fn vendor(&self) -> &str {
        "Linux"
    }
    fn model(&self) -> &str {
        "hwmon"
    }

    async fn initialize(&self) -> Result<bool> {
        let cached = Arc::clone(&self.cached_sensors);
        let io = self.io.clone();
        let stable_id = self.stable_id.clone();
        let handle = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let io_c = io.clone();
                let id_c = stable_id.clone();
                let sensors = tokio::task::spawn_blocking(move || {
                    HwmonDevice::read_sensors(io_c.dir(), &id_c)
                })
                .await
                .unwrap_or_default();
                *cached.lock().await = sensors;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
        log::info!(
            "[HwmonDevice] Initialized: {} ({})",
            self.chip_name,
            self.stable_id
        );
        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        vec![CapabilityRef::Sensor(self)]
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Sensor
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("hwmon")
    }
}

#[async_trait]
impl SensorCapability for HwmonDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.cached_sensors.lock().await.clone())
    }
}

/// A single PWM fan header exposed by a Linux hwmon chip.
pub struct HwmonFanDevice {
    io: HwmonIo,
    fan_index: u32,
    stable_id: String,
    id: String,
    label: String,
    controllable: bool,
    cached_rpm: Arc<Mutex<u32>>,
    cached_duty: Arc<Mutex<u8>>,
    original_pwm_enable: std::sync::Mutex<Option<u8>>,
    fan: FanStateSlot,
    visibility: VisibilitySlot,
    poll_task: Mutex<Option<TaskHandle>>,
}

impl HwmonFanDevice {
    pub fn new(path: std::path::PathBuf, fan_index: u32, stable_id: String) -> Self {
        let io = HwmonIo::new(path, None);
        let label = io
            .read_attr(&format!("fan{}_label", fan_index))
            .unwrap_or_default()
            .trim()
            .to_string();
        let label = if label.is_empty() {
            format!("Fan {}", fan_index)
        } else {
            label
        };
        let controllable = io.dir().join(format!("pwm{}", fan_index)).exists();
        let id = format!("hwmon_{}_fan{}", stable_id, fan_index);
        Self {
            io,
            fan_index,
            stable_id,
            id,
            label,
            controllable,
            cached_rpm: Arc::new(Mutex::new(0)),
            cached_duty: Arc::new(Mutex::new(0)),
            original_pwm_enable: std::sync::Mutex::new(None),
            fan: FanStateSlot::default(),
            visibility: VisibilitySlot::default(),
            poll_task: Mutex::new(None),
        }
    }

    fn fan_input_path(&self) -> std::path::PathBuf {
        self.io.dir().join(format!("fan{}_input", self.fan_index))
    }

    fn pwm_path(&self) -> std::path::PathBuf {
        self.io.dir().join(self.pwm_rel())
    }

    fn pwm_enable_path(&self) -> std::path::PathBuf {
        self.io.dir().join(self.pwm_enable_rel())
    }

    fn pwm_rel(&self) -> String {
        format!("pwm{}", self.fan_index)
    }

    fn pwm_enable_rel(&self) -> String {
        format!("pwm{}_enable", self.fan_index)
    }

    fn read_rpm(path: &std::path::Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .parse::<u32>()
            .unwrap_or(0)
    }

    fn read_raw_duty(path: &std::path::Path) -> u8 {
        let raw = std::fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .parse::<u32>()
            .unwrap_or(0)
            .min(255);
        // Round-half-up to match set_duty's write formula (duty*255/100),
        // so a write-then-read round-trip returns the same percentage.
        ((raw * 100 + 127) / 255) as u8
    }
}

#[async_trait]
impl Device for HwmonFanDevice {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.label
    }
    fn vendor(&self) -> &str {
        "Linux"
    }
    fn model(&self) -> &str {
        "hwmon"
    }

    async fn initialize(&self) -> Result<bool> {
        if !self.fan_input_path().exists() {
            return Ok(false);
        }
        if self.controllable {
            let enable = std::fs::read_to_string(self.pwm_enable_path())
                .unwrap_or_default()
                .trim()
                .parse::<u8>()
                .unwrap_or(1);
            *self.original_pwm_enable.lock().unwrap() = Some(enable);
        }
        let cached_rpm = Arc::clone(&self.cached_rpm);
        let cached_duty = Arc::clone(&self.cached_duty);
        let fan_input = self.fan_input_path();
        let pwm = self.pwm_path();
        let controllable = self.controllable;
        let handle = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let fi = fan_input.clone();
                let pw = pwm.clone();
                let (rpm, duty) = tokio::task::spawn_blocking(move || {
                    let rpm = HwmonFanDevice::read_rpm(&fi);
                    let duty = if controllable {
                        HwmonFanDevice::read_raw_duty(&pw)
                    } else {
                        0
                    };
                    (rpm, duty)
                })
                .await
                .unwrap_or((0, 0));
                *cached_rpm.lock().await = rpm;
                if controllable {
                    *cached_duty.lock().await = duty;
                }
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
        log::info!(
            "[HwmonFanDevice] Initialized fan{} ({})",
            self.fan_index,
            self.stable_id
        );
        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
        let original = *self.original_pwm_enable.lock().unwrap();
        if let Some(original) = original {
            if let Err(e) = self
                .io
                .write_attr(&self.pwm_enable_rel(), &original.to_string())
                .await
            {
                log::warn!("[HwmonFanDevice] failed to restore pwm_enable: {e}");
            }
        }
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        if self.controllable {
            vec![CapabilityRef::Fan(self)]
        } else {
            vec![]
        }
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Fan
    }

    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        Some(self.io.rate_status())
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("hwmon")
    }
}

#[async_trait]
impl FanCapability for HwmonFanDevice {
    fn fan_channel_id(&self) -> u8 {
        debug_assert!(self.fan_index <= 255, "fan_index {}", self.fan_index);
        self.fan_index as u8
    }

    async fn fan_controllable(&self) -> bool {
        self.controllable
    }

    async fn get_duty(&self) -> Result<u8> {
        Ok(*self.cached_duty.lock().await)
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        let current_enable = self
            .io
            .read_attr(&self.pwm_enable_rel())
            .unwrap_or_default()
            .trim()
            .parse::<u8>()
            .unwrap_or(1);
        if current_enable != 1 {
            self.io
                .write_attr(&self.pwm_enable_rel(), "1")
                .await
                .map_err(|e| {
                    anyhow::anyhow!("failed to set pwm{}_enable: {}", self.fan_index, e)
                })?;
        }
        let raw = (duty as u32 * 255 / 100).min(255) as u8;
        self.io
            .write_attr(&self.pwm_rel(), &raw.to_string())
            .await?;
        *self.cached_duty.lock().await = duty;
        Ok(())
    }

    async fn get_rpm(&self) -> Option<u32> {
        Some(*self.cached_rpm.lock().await)
    }

    fn fan_state(&self) -> &FanStateSlot {
        &self.fan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::DeviceCapability;

    #[test]
    fn compute_stable_id_strips_hwmon_suffix() {
        let path = std::path::Path::new("/sys/devices/pci0000:00/0000:00:18.3/hwmon/hwmon6");
        let id = HwmonDevice::compute_stable_id(path);
        assert_eq!(id, "pci0000_00_0000_00_18_3");
    }

    #[test]
    fn compute_stable_id_strips_direct_hwmon_suffix() {
        // e.g. nvme: .../nvme0/hwmon0 (no intermediate hwmon/ dir)
        let path = std::path::Path::new(
            "/sys/devices/pci0000:00/0000:00:01.2/0000:02:00.0/nvme/nvme0/hwmon0",
        );
        let id = HwmonDevice::compute_stable_id(path);
        assert_eq!(id, "pci0000_00_0000_00_01_2_0000_02_00_0_nvme_nvme0");
    }

    #[tokio::test]
    async fn get_sensors_returns_empty_on_init() {
        let device = HwmonDevice::new(std::path::PathBuf::from("/nonexistent/hwmon0"));
        let result = device.get_sensors().await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn serialize_produces_sensor_capability() {
        let device = HwmonDevice::new(std::path::PathBuf::from("/nonexistent/hwmon0"));
        let wire = device.serialize().await;
        assert!(matches!(wire.device_type, DeviceType::Sensor));
        assert_eq!(wire.capabilities.len(), 1);
        assert!(matches!(wire.capabilities[0], DeviceCapability::Sensors(_)));
    }

    #[test]
    fn fan_device_id_includes_chip_and_index() {
        let d = HwmonFanDevice::new(
            std::path::PathBuf::from("/nonexistent/hwmon0"),
            1,
            "nct6796".to_string(),
        );
        assert_eq!(d.id(), "hwmon_nct6796_fan1");
    }

    #[test]
    fn fan_device_name_falls_back_to_fan_n() {
        let d = HwmonFanDevice::new(
            std::path::PathBuf::from("/nonexistent/hwmon0"),
            2,
            "chip".to_string(),
        );
        assert_eq!(d.name(), "Fan 2");
    }

    #[tokio::test]
    async fn fan_device_serialize_produces_fan_capability() {
        let dir =
            std::env::temp_dir().join(format!("halod_hwmon_serialize_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fan1_input"), "1000\n").unwrap();
        std::fs::write(dir.join("pwm1"), "128\n").unwrap();

        let d = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        let wire = d.serialize().await;
        assert!(matches!(wire.device_type, DeviceType::Fan));
        assert_eq!(wire.capabilities.len(), 1);
        assert!(matches!(wire.capabilities[0], DeviceCapability::Fan(_)));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn set_duty_writes_pwm_and_enable_files() {
        let dir = std::env::temp_dir().join(format!("halod_hwmon_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(dir.join("pwm1"), "128\n").unwrap();
        std::fs::write(dir.join("pwm1_enable"), "2\n").unwrap();

        let d = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        d.set_duty(50).await.unwrap();

        let written_raw: u32 = std::fs::read_to_string(dir.join("pwm1"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(written_raw, 127); // 50 * 255 / 100

        let written_enable: u8 = std::fs::read_to_string(dir.join("pwm1_enable"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(written_enable, 1); // switched to manual

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn get_duty_returns_cached_value() {
        let d = HwmonFanDevice::new(
            std::path::PathBuf::from("/nonexistent/hwmon0"),
            1,
            "chip".to_string(),
        );
        *d.cached_duty.lock().await = 75;
        assert_eq!(d.get_duty().await.unwrap(), 75);
    }

    #[tokio::test]
    async fn fan_device_save_and_load_state_preserves_sensor_id() {
        use crate::cooling::config::FanCurveRecord;
        use crate::drivers::Device;

        let dir =
            std::env::temp_dir().join(format!("halod_hwmon_curve_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(dir.join("pwm1"), "128\n").unwrap();
        std::fs::write(dir.join("pwm1_enable"), "2\n").unwrap();

        let d = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        d.fan.set_fan_curve(FanCurveRecord {
            sensor_id: Some("hwmon_pci_temp1".to_string()),
            points: vec![(30.0, 20.0), (80.0, 100.0)],
        });

        let saved = crate::drivers::Device::save_state(&d).await;
        assert_eq!(
            saved["fan_curve"]["sensor_id"].as_str(),
            Some("hwmon_pci_temp1"),
            "sensor_id must be present in saved state"
        );

        let d2 = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        d2.load_state(&saved).await;
        let loaded = d2.fan.fan_curve().expect("fan curve should be loaded");
        assert_eq!(
            loaded.sensor_id.as_deref(),
            Some("hwmon_pci_temp1"),
            "sensor_id must survive the round-trip"
        );
        assert_eq!(loaded.points.len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn set_duty_writes_pwm_file() {
        let dir =
            std::env::temp_dir().join(format!("halod_hwmon_duty_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(dir.join("pwm1"), "0\n").unwrap();
        std::fs::write(dir.join("pwm1_enable"), "1\n").unwrap();

        let d = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        d.set_duty(50).await.expect("set_duty should succeed");

        let raw: u32 = std::fs::read_to_string(dir.join("pwm1"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // 50% duty → raw = 50 * 255 / 100 = 127
        assert_eq!(raw, 127);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn set_duty_writes_show_up_in_write_rate_status() {
        let dir = std::env::temp_dir().join(format!(
            "halod_hwmon_rate_status_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(dir.join("pwm1"), "0\n").unwrap();
        std::fs::write(dir.join("pwm1_enable"), "1\n").unwrap();

        let d = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        assert_eq!(d.write_rate_status().unwrap().current_bytes_per_sec, 0.0);
        d.set_duty(50).await.expect("set_duty should succeed");
        assert!(
            d.write_rate_status().unwrap().current_bytes_per_sec > 0.0,
            "the pwm write must be visible in the device's write-rate stats"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn set_duty_writes_are_delayed_once_a_limit_is_set() {
        let dir = std::env::temp_dir().join(format!(
            "halod_hwmon_rate_limit_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(dir.join("pwm1"), "0\n").unwrap();
        std::fs::write(dir.join("pwm1_enable"), "1\n").unwrap();

        let d = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        d.io.set_write_rate_limit(Some(halod_shared::types::WriteRateLimit {
            max_bytes_per_sec: 1,
        }));
        d.set_duty(50).await.expect("set_duty should succeed"); // consumes the initial burst credit

        let before = std::time::Instant::now();
        d.set_duty(60).await.expect("set_duty should succeed");
        assert!(
            std::time::Instant::now() >= before + std::time::Duration::from_millis(400),
            "a configured write-rate limit must actually delay pwm writes"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn set_duty_enables_pwm_when_in_auto_mode() {
        let dir = std::env::temp_dir().join(format!(
            "halod_hwmon_duty_enable_test_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fan1_input"), "1200\n").unwrap();
        std::fs::write(dir.join("pwm1"), "0\n").unwrap();
        // enable=2 means "automatic" — set_duty must flip it to 1 (manual)
        std::fs::write(dir.join("pwm1_enable"), "2\n").unwrap();

        let d = HwmonFanDevice::new(dir.clone(), 1, "chip".to_string());
        d.set_duty(100).await.expect("set_duty should succeed");

        let enable: u8 = std::fs::read_to_string(dir.join("pwm1_enable"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            enable, 1,
            "pwm_enable must be set to 1 (manual) after set_duty"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
