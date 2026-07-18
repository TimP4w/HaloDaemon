// SPDX-License-Identifier: GPL-3.0-or-later
//! A synthetic device representing the host PC / OS itself. It is not tied to a
//! physical transport: a [`crate::registry::discovery::TransportScanner`] constructs and
//! registers it during discovery.
//!
//! Capabilities today:
//! - **Power profile** (Choice) — performance / balanced / power saver, when the
//!   OS exposes one.
//! - **Host metrics** (Sensors) — CPU load, memory, CPU frequency, uptime.
//! - **Keep awake** (Boolean) — inhibit idle/sleep while on.
//!
//! It's the home for other PC-generic / OS-specific things as they land.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::drivers::vendors::generic::devices::common::TaskHandle;
use crate::drivers::{
    BooleanCapability, CapabilityRef, ChoiceCapability, ChoiceStateCache, Device, SensorCapability,
    VisibilitySlot,
};
use halod_shared::types::{
    Boolean, Choice, ChoiceDisplay, ChoiceOption, DeviceCapability, DeviceType, Sensor,
};

pub mod keep_awake;
pub mod metrics;
pub mod power_profile;

use keep_awake::{KeepAwake, KEEP_AWAKE_KEY};
use metrics::HostMetricsBackend;
use power_profile::{index_of, PowerProfileBackend, POWER_PROFILE_KEY, PROFILES};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

fn os_label() -> &'static str {
    match std::env::consts::OS {
        "linux" => "Linux",
        "windows" => "Windows",
        "macos" => "macOS",
        other => other,
    }
}

/// The PC/OS device. Grows a new field + capability impl per OS-specific feature.
pub struct ComputerDevice {
    id: String,
    /// `None` when the host exposes no switchable power profile.
    power_profile: Option<Box<dyn PowerProfileBackend>>,
    choice_cache: ChoiceStateCache,
    /// Index into [`PROFILES`] of the selected power profile (default: balanced).
    selected_profile: Mutex<usize>,
    metrics: Arc<dyn HostMetricsBackend>,
    cached_sensors: Arc<Mutex<Vec<Sensor>>>,
    poll_task: Mutex<Option<TaskHandle>>,
    keep_awake: Box<dyn KeepAwake>,
    visibility: VisibilitySlot,
}

impl ComputerDevice {
    pub fn new(
        power_profile: Option<Box<dyn PowerProfileBackend>>,
        metrics: Arc<dyn HostMetricsBackend>,
        keep_awake: Box<dyn KeepAwake>,
    ) -> Self {
        Self {
            id: "computer".to_string(),
            power_profile,
            choice_cache: ChoiceStateCache::default(),
            selected_profile: Mutex::new(1),
            metrics,
            cached_sensors: Arc::new(Mutex::new(Vec::new())),
            poll_task: Mutex::new(None),
            keep_awake,
            visibility: VisibilitySlot::default(),
        }
    }
}

#[async_trait]
impl Device for ComputerDevice {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        "Computer"
    }
    fn vendor(&self) -> &str {
        "System"
    }
    fn model(&self) -> &str {
        os_label()
    }
    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Computer
    }

    async fn initialize(&self) -> Result<bool> {
        if let Some(backend) = &self.power_profile {
            if let Some(idx) = backend.current().await.and_then(index_of) {
                *self.selected_profile.lock().await = idx;
            }
        }

        // Eager first read so get_sensors() returns a non-empty list immediately.
        let first = self.metrics.read().await;
        *self.cached_sensors.lock().await = metrics::to_sensors(&first);

        let metrics = Arc::clone(&self.metrics);
        let cache = Arc::clone(&self.cached_sensors);
        let handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(POLL_INTERVAL).await;
                let snapshot = metrics.read().await;
                *cache.lock().await = metrics::to_sensors(&snapshot);
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));

        log::info!("[ComputerDevice] initialized ({})", os_label());
        Ok(true)
    }

    async fn close(&self) {
        self.poll_task.lock().await.take();
        if self.keep_awake.is_active() {
            if let Err(e) = self.keep_awake.set(false).await {
                log::warn!("[ComputerDevice] failed to release keep-awake on close: {e:#}");
            }
        }
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let mut caps = Vec::new();
        if self.power_profile.is_some() {
            caps.push(CapabilityRef::Choice(self));
        }
        caps.push(CapabilityRef::Boolean(self));
        caps.push(CapabilityRef::Sensor(self));
        caps
    }
}

#[async_trait]
impl SensorCapability for ComputerDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.cached_sensors.lock().await.clone())
    }
}

#[async_trait]
impl BooleanCapability for ComputerDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        Ok(vec![Boolean {
            key: KEEP_AWAKE_KEY.to_string(),
            label: "Keep awake".to_string(),
            value: self.keep_awake.is_active(),
            read_only: false,
            category: "Power".to_string(),
            visible_when: None,
        }])
    }

    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        if key != KEEP_AWAKE_KEY {
            anyhow::bail!("Unknown boolean key: {key}");
        }
        self.keep_awake.set(value).await
    }
}

#[async_trait]
impl ChoiceCapability for ComputerDevice {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.choice_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.power_profile.as_ref()?;
        let selected = *self.selected_profile.lock().await;
        let options = PROFILES
            .iter()
            .map(|(id, label)| ChoiceOption {
                id: (*id).to_string(),
                label: (*label).to_string(),
            })
            .collect();
        Some(DeviceCapability::Choice(vec![Choice {
            key: POWER_PROFILE_KEY.to_string(),
            label: "Power profile".to_string(),
            options,
            selected,
            category: "Power".to_string(),
            display: ChoiceDisplay::Inline,
            visible_when: None,
        }]))
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        if key != POWER_PROFILE_KEY {
            anyhow::bail!("Unknown choice key: {key}");
        }
        let backend = self
            .power_profile
            .as_ref()
            .context("device has no power-profile backend")?;
        let (id, _) = PROFILES
            .get(selected)
            .ok_or_else(|| anyhow::anyhow!("power profile index {selected} out of range"))?;
        backend.apply(id).await?;
        // Record only after a successful apply, so a failed switch never persists
        // a profile the host isn't actually using.
        self.choice_cache.record(key, selected);
        *self.selected_profile.lock().await = selected;
        Ok(())
    }
}

inventory::submit!(crate::registry::discovery::TransportScanner {
    name: "computer",
    detail: halod_shared::types::DiscoveryDetail::Computer,
    platform: None,
    scan: |app| Box::pin(async move { discover(app).await }),
});

async fn discover(app: Arc<crate::state::AppState>) {
    // Metrics are the always-available baseline; without them (unsupported OS)
    // there's nothing to register.
    let Some(metrics) = metrics::make_backend() else {
        return;
    };
    let keep_awake = keep_awake::make_backend();
    let power_profile = power_profile::make_backend().await;
    let device: Arc<dyn Device> = Arc::new(ComputerDevice::new(power_profile, metrics, keep_awake));
    crate::registry::usecases::registration::register_device(&app, device).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPower {
        applied: std::sync::Mutex<Vec<String>>,
        current: Option<&'static str>,
    }

    #[async_trait]
    impl PowerProfileBackend for MockPower {
        async fn current(&self) -> Option<&'static str> {
            self.current
        }
        async fn apply(&self, id: &str) -> Result<()> {
            self.applied.lock().unwrap().push(id.to_string());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockMetrics;
    #[async_trait]
    impl HostMetricsBackend for MockMetrics {
        async fn read(&self) -> metrics::HostMetrics {
            metrics::HostMetrics {
                cpu_load_pct: Some(10.0),
                ..Default::default()
            }
        }
    }

    #[derive(Default)]
    struct MockKeepAwake {
        active: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl KeepAwake for MockKeepAwake {
        async fn set(&self, on: bool) -> Result<()> {
            self.active.store(on, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
        fn is_active(&self) -> bool {
            self.active.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn device(power: Option<&'static str>) -> ComputerDevice {
        ComputerDevice::new(
            power.map(|c| {
                Box::new(MockPower {
                    applied: std::sync::Mutex::new(Vec::new()),
                    current: Some(c),
                }) as Box<dyn PowerProfileBackend>
            }),
            Arc::new(MockMetrics),
            Box::new(MockKeepAwake::default()),
        )
    }

    #[tokio::test]
    async fn capabilities_include_choice_only_when_power_profile_present() {
        assert_eq!(device(Some("balanced")).capabilities().len(), 3);
        // No power profile -> Boolean + Sensor only.
        assert_eq!(device(None).capabilities().len(), 2);
    }

    #[test]
    fn reports_computer_device_type() {
        assert_eq!(device(None).wire_device_type(), DeviceType::Computer);
    }

    #[tokio::test]
    async fn choice_hidden_without_backend() {
        assert!(ChoiceCapability::to_wire(&device(None)).await.is_none());
        assert!(ChoiceCapability::to_wire(&device(Some("balanced")))
            .await
            .is_some());
    }

    #[tokio::test]
    async fn keep_awake_boolean_reflects_and_toggles_state() {
        let dev = device(None);
        let before = dev.get_booleans().await.unwrap();
        assert_eq!(before[0].key, KEEP_AWAKE_KEY);
        assert!(!before[0].value);

        dev.set_boolean(KEEP_AWAKE_KEY, true).await.unwrap();
        assert!(dev.get_booleans().await.unwrap()[0].value);

        assert!(dev.set_boolean("nope", true).await.is_err());
    }

    #[tokio::test]
    async fn set_choice_applies_and_rejects_bad_input() {
        let dev = device(Some("balanced"));
        dev.set_choice(POWER_PROFILE_KEY, 0).await.unwrap();
        assert_eq!(*dev.selected_profile.lock().await, 0);
        assert!(dev.set_choice(POWER_PROFILE_KEY, 99).await.is_err());
        assert!(dev.set_choice("unknown_key", 0).await.is_err());
    }

    #[tokio::test]
    async fn initialize_seeds_sensor_cache() {
        let dev = device(None);
        dev.initialize().await.unwrap();
        let sensors = dev.get_sensors().await.unwrap();
        assert!(
            !sensors.is_empty(),
            "sensors must be non-empty right after initialize()"
        );
    }
}
