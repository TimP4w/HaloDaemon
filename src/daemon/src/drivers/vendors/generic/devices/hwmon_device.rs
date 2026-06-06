#![cfg(target_os = "linux")]

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::drivers::{
    vendors::generic::devices::common::TaskHandle, CapabilityRef, Device, FanCapability,
    FanStateSlot, SensorCapability, VisibilitySlot,
};
use halod_protocol::types::{DeviceType, Sensor, SensorUnit, VisibilityState};

pub struct HwmonDevice {
    path: std::path::PathBuf,
    chip_name: String,
    stable_id: String,
    cached_sensors: Arc<Mutex<Vec<Sensor>>>,
    sensor_visibility: std::sync::Mutex<std::collections::HashMap<String, VisibilityState>>,
    visibility: VisibilitySlot,
    poll_task: Mutex<Option<TaskHandle>>,
}

impl HwmonDevice {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn new(path: std::path::PathBuf) -> Self {
        let chip_name = std::fs::read_to_string(path.join("name"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let stable_id = Self::compute_stable_id(&path);
        let chip_name = if chip_name.is_empty() {
            stable_id.clone()
        } else {
            chip_name
        };
        Self {
            path,
            chip_name,
            stable_id,
            cached_sensors: Arc::new(Mutex::new(vec![])),
            sensor_visibility: std::sync::Mutex::new(std::collections::HashMap::new()),
            visibility: VisibilitySlot::default(),
            poll_task: Mutex::new(None),
        }
    }

    pub fn stable_id(&self) -> &str {
        &self.stable_id
    }

    /// Derive a stable ID from the hardware device path by resolving the sysfs
    /// symlink to its canonical target (e.g. /sys/devices/pci0000:00/0000:00:18.3)
    /// and sanitizing it. The result is independent of the dynamic hwmonN index.
    fn compute_stable_id(path: &std::path::Path) -> String {
        let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let s = canonical.to_string_lossy();
        let relative = s.strip_prefix("/sys/devices/").unwrap_or(s.as_ref());
        // Strip the final hwmonN segment (always the last path component)
        let without_last = match relative.rfind('/') {
            Some(pos) => &relative[..pos],
            None => relative,
        };
        // Some chips sit at .../hwmon/hwmonN; strip the intermediate /hwmon too
        let base = without_last.strip_suffix("/hwmon").unwrap_or(without_last);
        base.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    }

    fn read_sensors(path: &std::path::Path, stable_id: &str) -> Vec<Sensor> {
        let mut sensors = vec![];
        let mut i = 1u32;
        loop {
            let input_path = path.join(format!("temp{}_input", i));
            if !input_path.exists() {
                break;
            }
            let raw = std::fs::read_to_string(&input_path)
                .unwrap_or_default()
                .trim()
                .parse::<f64>()
                .unwrap_or(0.0);
            let label = std::fs::read_to_string(path.join(format!("temp{}_label", i)))
                .unwrap_or_default()
                .trim()
                .to_string();
            sensors.push(Sensor {
                id: format!("hwmon_{}_temp{}", stable_id, i),
                name: label,
                value: raw / 1000.0,
                unit: SensorUnit::Celsius,
                sensor_type: halod_protocol::types::SensorType::Temperature,
                visibility: Default::default(),
            });
            i += 1;
        }
        sensors
    }
}

#[async_trait]
impl Device for HwmonDevice {
    fn id(&self) -> String {
        format!("hwmon_{}", self.stable_id)
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
        let path = self.path.clone();
        let stable_id = self.stable_id.clone();
        let handle = tokio::task::spawn(async move {
            loop {
                tokio::time::sleep(HwmonDevice::POLL_INTERVAL).await;
                let sensors = HwmonDevice::read_sensors(&path, &stable_id);
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

    fn set_sensor_visibility(&self, sensor_id: &str, state: VisibilityState) {
        let mut vis = self.sensor_visibility.lock().unwrap();
        if state == VisibilityState::Visible {
            vis.remove(sensor_id);
        } else {
            vis.insert(sensor_id.to_string(), state);
        }
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
        let mut sensors = self.cached_sensors.lock().await.clone();
        let vis = self.sensor_visibility.lock().unwrap();
        for sensor in &mut sensors {
            if let Some(state) = vis.get(&sensor.id) {
                sensor.visibility = state.clone();
            }
        }
        Ok(sensors)
    }
}

// ---------------------------------------------------------------------------
// HwmonFanDevice — a single PWM fan header exposed by a Linux hwmon chip
// ---------------------------------------------------------------------------

pub struct HwmonFanDevice {
    path: std::path::PathBuf,
    fan_index: u32,
    stable_id: String,
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
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

    pub fn new(path: std::path::PathBuf, fan_index: u32, stable_id: String) -> Self {
        let label = std::fs::read_to_string(path.join(format!("fan{}_label", fan_index)))
            .unwrap_or_default()
            .trim()
            .to_string();
        let label = if label.is_empty() {
            format!("Fan {}", fan_index)
        } else {
            label
        };
        let controllable = path.join(format!("pwm{}", fan_index)).exists();
        Self {
            path,
            fan_index,
            stable_id,
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
        self.path.join(format!("fan{}_input", self.fan_index))
    }

    fn pwm_path(&self) -> std::path::PathBuf {
        self.path.join(format!("pwm{}", self.fan_index))
    }

    fn pwm_enable_path(&self) -> std::path::PathBuf {
        self.path.join(format!("pwm{}_enable", self.fan_index))
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
        (raw * 100 / 255) as u8
    }
}

#[async_trait]
impl Device for HwmonFanDevice {
    fn id(&self) -> String {
        format!("hwmon_{}_fan{}", self.stable_id, self.fan_index)
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
                tokio::time::sleep(HwmonFanDevice::POLL_INTERVAL).await;
                *cached_rpm.lock().await = HwmonFanDevice::read_rpm(&fan_input);
                if controllable {
                    *cached_duty.lock().await = HwmonFanDevice::read_raw_duty(&pwm);
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
        if let Some(original) = *self.original_pwm_enable.lock().unwrap() {
            let _ = std::fs::write(self.pwm_enable_path(), original.to_string());
        }
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        if self.controllable && self.active_state() == VisibilityState::Visible {
            vec![CapabilityRef::Fan(self)]
        } else {
            vec![]
        }
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Fan
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some("hwmon")
    }
}

#[async_trait]
impl FanCapability for HwmonFanDevice {
    fn fan_channel_id(&self) -> u8 {
        self.fan_index as u8
    }

    async fn fan_controllable(&self) -> bool {
        self.controllable
    }

    async fn get_duty(&self) -> Result<u8> {
        Ok(*self.cached_duty.lock().await)
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        let enable_path = self.pwm_enable_path();
        let current_enable = std::fs::read_to_string(&enable_path)
            .unwrap_or_default()
            .trim()
            .parse::<u8>()
            .unwrap_or(1);
        if current_enable != 1 {
            std::fs::write(&enable_path, "1").map_err(|e| {
                anyhow::anyhow!("failed to set pwm{}_enable: {}", self.fan_index, e)
            })?;
        }
        let raw = (duty as u32 * 255 / 100).min(255) as u8;
        std::fs::write(self.pwm_path(), raw.to_string())?;
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
    use halod_protocol::types::DeviceCapability;

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
        let dir = std::env::temp_dir().join(format!(
            "halod_hwmon_serialize_test_{}",
            std::process::id()
        ));
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
        use crate::config::FanCurveRecord;
        use crate::drivers::Device;

        // Create a temp dir with fan input + pwm files so `controllable = true`
        // and the Fan capability appears in `capabilities()`.
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

        // Create a fresh device and load the state back in.
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
}
