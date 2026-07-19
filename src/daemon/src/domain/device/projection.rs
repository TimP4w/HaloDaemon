// SPDX-License-Identifier: GPL-3.0-or-later
//! Projects internal device objects into typed bus-facing read models.
use std::collections::HashMap;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::config::{Config, PlacedZone};
use crate::domain::cooling::model::FanCurveRecord;
use crate::domain::registry::identity::{detect_conflicts, ConflictEntry};
use crate::infrastructure::drivers::Device;
use halod_shared::types::{DeviceCapability, EffectParamValue, VisibilityState, WireDevice};

/// One pass over the device registry: wire-serialize each device, apply the
/// config/HID overlay, and collect the per-domain engine inputs the cooling,
/// lighting, and LCD snapshots need.
#[derive(Default)]
pub struct DevicesSnapshot {
    pub devices: Vec<WireDevice>,
    pub fan_curves: Vec<(String, String, FanCurveRecord)>,
    pub placed_zones: Vec<PlacedZone>,
    pub lcd_templates: HashMap<String, String>,
    pub lcd_template_params: HashMap<String, HashMap<String, EffectParamValue>>,
}

impl AppState {
    pub async fn snapshot_devices(&self, cfg: &Config) -> DevicesSnapshot {
        self.snapshot_selected_devices(cfg, None).await
    }

    pub async fn snapshot_selected_devices(
        &self,
        cfg: &Config,
        selected_ids: Option<&std::collections::HashSet<String>>,
    ) -> DevicesSnapshot {
        let all_devices: Vec<Arc<dyn Device>> = self.device_registry.read().await.clone();
        let device_list: Vec<Arc<dyn Device>> = all_devices
            .iter()
            .filter(|device| selected_ids.is_none_or(|ids| ids.contains(device.id())))
            .cloned()
            .collect();
        let mut devices = Vec::with_capacity(device_list.len());
        for d in &device_list {
            devices.push(d.serialize().await);
        }

        // Collect per-device engine state from device_list (already owned — no extra locks).
        let mut fan_curves = Vec::new();
        for device in &device_list {
            if let Some(cooling) = device.as_cooling() {
                fan_curves.extend(
                    cooling
                        .curves()
                        .into_iter()
                        .map(|(channel_id, curve)| (device.id().to_owned(), channel_id, curve)),
                );
            }
        }

        let placed_zones: Vec<PlacedZone> = device_list
            .iter()
            .filter_map(|d| d.as_lighting())
            .flat_map(|s| s.placed_channels())
            .collect();

        let lcd_templates: HashMap<String, String> = device_list
            .iter()
            .filter_map(|d| {
                let lcd = d.as_lcd()?;
                Some((d.id().to_owned(), lcd.lcd_template_id()?))
            })
            .collect();

        let lcd_template_params: HashMap<String, HashMap<String, EffectParamValue>> = device_list
            .iter()
            .filter_map(|d| {
                let lcd = d.as_lcd()?;
                lcd.lcd_template_id()?;
                Some((d.id().to_owned(), lcd.lcd_template_params()))
            })
            .collect();

        // Overlay pass: record name/state, LCD-engine mode, and per-zone RGB
        // transforms onto each device's wire struct.
        let hid_tracked = self.hid.tracked_ids().await;
        let applied_duties = self.cooling.applied_duties.lock().await.clone();

        for (device, wire) in device_list.iter().zip(devices.iter_mut()) {
            // Byte-movement transport: a driver-declared label wins; otherwise HID
            // when the device is HID-tracked, else left unset (internal/unknown).
            wire.transport = device
                .debug_transport()
                .map(|t| t.to_string())
                .or_else(|| hid_tracked.contains(device.id()).then(|| "hid".to_string()));

            // Live write-rate ceiling/throughput, when the device has wired up
            // stats from its transport; `None` otherwise (e.g. a chain accessory
            // sharing its parent's transport, or a not-yet-enforced transport).
            wire.write_rate = device.write_rate_status();

            // Name/active_state from the persisted DeviceRecord. Devices with
            // externally-owned names (chain links) keep their serialize()'d name.
            if let Some(record) = cfg.known_devices.get(device.id()) {
                wire.active_state = record.active_state.clone();
                if !device.has_external_name() && !record.name.is_empty() {
                    wire.name = record.name.clone();
                }
            }

            let transforms = device.as_lighting().map(|r| r.channel_transforms());
            let cached_cooling = device
                .as_cooling()
                .map(|cooling| cooling.cached_cooling_status())
                .unwrap_or_default();

            wire.capabilities
                .retain(|capability| !matches!(capability, DeviceCapability::Sensors(_)));
            let mut sensors = if wire.active_state == VisibilityState::Disabled {
                Vec::new()
            } else {
                self.data_bus.sensors_for_device(device.id())
            };
            for sensor in &mut sensors {
                if let Some(state) = cfg.sensor_visibility.get(&sensor.id) {
                    sensor.visibility = state.clone();
                }
            }
            for cap in &mut wire.capabilities {
                match cap {
                    DeviceCapability::Lcd(_) => {}
                    DeviceCapability::Lighting(status) => {
                        if let Some(t) = &transforms {
                            if !t.is_empty() {
                                status.channel_transforms = t.clone();
                            }
                        }
                    }
                    DeviceCapability::Cooling(status) => {
                        for channel in &mut status.channels {
                            let prefix = format!("cooling_{}_{}", device.id(), channel.id);
                            if let Some(sensor) = sensors
                                .iter()
                                .find(|sensor| sensor.id == format!("{prefix}_rpm"))
                            {
                                channel.rpm = Some(sensor.value.max(0.0).round() as u32);
                            }
                            if let Some(sensor) = sensors
                                .iter()
                                .find(|sensor| sensor.id == format!("{prefix}_duty"))
                            {
                                channel.duty = Some(sensor.value.clamp(0.0, 100.0).round() as u8);
                            }
                            // A device poll/event cache is newer than the
                            // separately retained synthesized sensors that may
                            // still represent the previous sampling cycle.
                            overlay_cached_cooling(channel, &cached_cooling);
                            if let Some(duty) = applied_duties.get(
                                &crate::domain::cooling::state::curve_key(device.id(), &channel.id),
                            ) {
                                channel.duty = Some(*duty);
                            }
                        }
                    }
                    DeviceCapability::Sensors(_) => {}
                    _ => {}
                }
            }
            if !sensors.is_empty() {
                wire.capabilities.push(DeviceCapability::Sensors(sensors));
            }
        }

        let conflict_entries: Vec<_> = all_devices
            .iter()
            .map(|device| {
                let active_state = cfg
                    .known_devices
                    .get(device.id())
                    .map(|record| record.active_state.clone())
                    .unwrap_or_else(|| device.active_state());
                ConflictEntry {
                    id: device.id().to_owned(),
                    identity: device.identity(),
                    origin: device.conflict_origin(),
                    connected: device.is_live(),
                    active_state,
                    integration_root: device.integration_id().is_some(),
                }
            })
            .collect();
        let all_conflicts = detect_conflicts(&conflict_entries);
        let conflicts_by_id: HashMap<_, _> = conflict_entries
            .iter()
            .zip(all_conflicts)
            .map(|(entry, conflict)| (entry.id.clone(), conflict))
            .collect();
        for wire in &mut devices {
            wire.conflict = conflicts_by_id.get(&wire.id).cloned().flatten();
        }

        DevicesSnapshot {
            devices,
            fan_curves,
            placed_zones,
            lcd_templates,
            lcd_template_params,
        }
    }
}

fn overlay_cached_cooling(
    channel: &mut halod_shared::types::CoolingChannel,
    cached: &[halod_shared::types::CoolingChannel],
) {
    if let Some(current) = cached.iter().find(|current| current.id == channel.id) {
        channel.rpm = current.rpm;
        channel.duty = current.duty;
        channel.controllable = current.controllable;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::state::AppState;
    use crate::config::Config;
    use crate::test_support::MockDevice;
    use async_trait::async_trait;

    #[tokio::test]
    async fn snapshot_marks_matching_serials_as_a_confirmed_conflict() {
        use crate::domain::registry::identity::{DeviceIdentity, DeviceOrigin, IdentifiedDevice};
        let app = Arc::new(AppState::new(Config::default()));
        for (id, origin) in [
            ("builtin", DeviceOrigin::Builtin),
            ("openrgb", DeviceOrigin::Integration("openrgb".into())),
        ] {
            let identity = DeviceIdentity {
                scope: Some(crate::domain::registry::identity::IdentityScope::Local),
                serial: Some("unit-1".into()),
                ..Default::default()
            };
            let dev: Arc<dyn Device> = Arc::new(IdentifiedDevice::new(
                Arc::new(MockDevice::new(id).with_name(id)),
                identity,
                origin,
            ));
            app.device_registry.write().await.push(dev);
        }
        let cfg = app.config.read().await.clone();
        let snapshot = app.snapshot_devices(&cfg).await;
        let builtin = snapshot.devices.iter().find(|d| d.id == "builtin").unwrap();
        let openrgb = snapshot.devices.iter().find(|d| d.id == "openrgb").unwrap();
        assert_eq!(builtin.conflict.as_ref().unwrap().recommended_id, "builtin");
        assert_eq!(openrgb.conflict.as_ref().unwrap().recommended_id, "builtin");
        assert_eq!(
            builtin.conflict.as_ref().unwrap().confidence,
            halod_shared::types::ConflictConfidence::Confirmed
        );
    }

    #[tokio::test]
    async fn serialize_synthesizes_fan_sensors() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan_rpm(1500),
        );
        app.device_registry.write().await.push(dev);
        crate::domain::device::usecases::telemetry::observe(&app).await;
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        let sensors = snap.devices[0].sensors().expect("fan sensors present");
        assert!(sensors
            .iter()
            .any(|s| s.id == "cooling_test_device_default_duty"));
        assert!(sensors
            .iter()
            .any(|s| s.id == "cooling_test_device_default_rpm" && s.value == 1500.0));
    }

    #[tokio::test]
    async fn retained_rpm_observation_overlays_cooling_channel() {
        use halod_shared::types::{Sensor, SensorType, SensorUnit, VisibilityState};

        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(MockDevice::new("fan").with_fan_rpm(900));
        app.device_registry.write().await.push(dev);
        app.data_bus.replace_host_sensors(vec![(
            "fan".into(),
            Sensor {
                id: "cooling_fan_default_rpm".into(),
                name: "Fan RPM".into(),
                value: 1777.0,
                unit: SensorUnit::Rpm,
                sensor_type: SensorType::FanSpeed,
                visibility: VisibilityState::Visible,
            },
        )]);

        let snapshot = app.snapshot_devices(&app.config.read().await.clone()).await;
        let rpm = snapshot.devices[0]
            .capabilities
            .iter()
            .find_map(|capability| match capability {
                DeviceCapability::Cooling(cooling) => cooling.channels[0].rpm,
                _ => None,
            });
        assert_eq!(rpm, Some(1777));
    }

    #[test]
    fn fresh_device_cache_wins_over_stale_retained_rpm() {
        use halod_shared::types::{CoolingChannel, CoolingChannelKind};
        let mut projected = CoolingChannel {
            id: "fan1".into(),
            name: "Fan".into(),
            kind: CoolingChannelKind::Fan,
            controllable: true,
            rpm: Some(900),
            duty: Some(28),
        };
        let fresh = CoolingChannel {
            rpm: Some(2200),
            duty: Some(100),
            ..projected.clone()
        };

        overlay_cached_cooling(&mut projected, &[fresh]);

        assert_eq!(projected.rpm, Some(2200));
        assert_eq!(projected.duty, Some(100));
    }

    #[tokio::test]
    async fn successfully_applied_duty_overlays_stale_telemetry() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(MockDevice::new("fan").with_fan_rpm(900));
        app.device_registry.write().await.push(dev);
        crate::domain::device::usecases::telemetry::observe(&app).await;
        assert!(app.cooling.record_applied_duty("fan", "default", 100).await);

        let snapshot = app.snapshot_devices(&app.config.read().await.clone()).await;
        let duty =
            snapshot.devices[0]
                .capabilities
                .iter()
                .find_map(|capability| match capability {
                    DeviceCapability::Cooling(cooling) => cooling.channels[0].duty,
                    _ => None,
                });
        assert_eq!(duty, Some(100));
    }

    #[tokio::test]
    async fn serialize_applies_saved_visibility_to_fan_sensors() {
        let mut cfg = Config::default();
        cfg.sensor_visibility.insert(
            "cooling_test_device_default_duty".to_string(),
            halod_shared::types::VisibilityState::Hidden,
        );
        let app = Arc::new(AppState::new(cfg));
        let dev: Arc<dyn Device> = Arc::new(MockDevice::new("test_device").with_fan());
        app.device_registry.write().await.push(dev);
        crate::domain::device::usecases::telemetry::observe(&app).await;
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        let sensors = snap.devices[0].sensors().expect("fan sensors present");
        let duty = sensors
            .iter()
            .find(|s| s.id == "cooling_test_device_default_duty")
            .unwrap();
        assert_eq!(
            duty.visibility,
            halod_shared::types::VisibilityState::Hidden
        );
    }

    #[tokio::test]
    async fn serialize_applies_saved_visibility_to_device_native_sensors() {
        // Device SensorCapability sensors report
        // Visible from get_sensors; the config overlay must hide them at snapshot.
        use halod_shared::types::{Sensor, SensorType, SensorUnit, VisibilityState};
        let mut cfg = Config::default();
        cfg.sensor_visibility
            .insert("ccd1".to_string(), VisibilityState::Hidden);
        let app = Arc::new(AppState::new(cfg));
        let dev: Arc<dyn Device> = Arc::new(MockDevice::new("cpu").with_sensor(vec![Sensor {
            id: "ccd1".into(),
            name: "CCD1 (Tdie)".into(),
            value: 50.0,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::Temperature,
            visibility: VisibilityState::Visible,
        }]));
        app.device_registry.write().await.push(dev);
        crate::domain::device::usecases::telemetry::observe(&app).await;
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        let sensors = snap.devices[0].sensors().expect("sensors present");
        let ccd1 = sensors.iter().find(|s| s.id == "ccd1").unwrap();
        assert_eq!(ccd1.visibility, VisibilityState::Hidden);
    }

    #[tokio::test]
    async fn serialize_strips_sensors_from_disabled_device() {
        use crate::domain::registry::model::DeviceRecord;
        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "test_device".to_string(),
            DeviceRecord {
                name: String::new(),
                vendor: String::new(),
                model: String::new(),
                active_state: halod_shared::types::VisibilityState::Disabled,
            },
        );
        let app = Arc::new(AppState::new(cfg));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_fan_rpm(1500)
                .with_sensor(vec![halod_shared::types::Sensor {
                    id: "temp1".into(),
                    name: "Liquid".into(),
                    value: 33.0,
                    unit: halod_shared::types::SensorUnit::Celsius,
                    sensor_type: halod_shared::types::SensorType::Temperature,
                    visibility: halod_shared::types::VisibilityState::Visible,
                }]),
        );
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        assert!(snap.devices[0].sensors().is_none());
    }

    #[tokio::test]
    async fn serialize_patches_name_from_device_record() {
        use crate::domain::registry::model::DeviceRecord;
        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "test_device".to_string(),
            DeviceRecord {
                name: "My Fan".into(),
                vendor: "Acme".into(),
                model: "Fan 3000".into(),
                active_state: Default::default(),
            },
        );
        let app = Arc::new(AppState::new(cfg));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan()
                .with_rgb(),
        );
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        assert_eq!(snap.devices[0].name, "My Fan");
    }

    #[tokio::test]
    async fn serialize_keeps_device_name_when_no_record() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan()
                .with_rgb(),
        );
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        assert_eq!(snap.devices[0].name, "Test Fan");
    }

    #[tokio::test]
    async fn serialize_keeps_device_name_when_device_has_external_name() {
        use crate::domain::registry::model::DeviceRecord;
        struct ExternalNameDevice {
            inner: MockDevice,
        }
        #[async_trait]
        impl Device for ExternalNameDevice {
            fn id(&self) -> &str {
                self.inner.id()
            }
            fn name(&self) -> &str {
                self.inner.name()
            }
            fn vendor(&self) -> &str {
                self.inner.vendor()
            }
            fn model(&self) -> &str {
                self.inner.model()
            }
            fn capabilities(&self) -> Vec<crate::infrastructure::drivers::CapabilityRef<'_>> {
                vec![]
            }
            fn has_external_name(&self) -> bool {
                true
            }
            async fn initialize(&self) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn close(&self) {}
            async fn serialize(&self) -> WireDevice {
                let mut w = self.inner.serialize().await;
                w.name = "Chain Link 1".to_string();
                w
            }
        }
        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "chain_dev".to_string(),
            DeviceRecord {
                name: "ARGB Strip".into(),
                vendor: "Generic".into(),
                model: "Chain Link".into(),
                active_state: Default::default(),
            },
        );
        let app = Arc::new(AppState::new(cfg));
        let dev: Arc<dyn Device> = Arc::new(ExternalNameDevice {
            inner: MockDevice::new("chain_dev")
                .with_name("ARGB Strip")
                .with_vendor("Generic")
                .with_model("Chain Link"),
        });
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        assert_eq!(snap.devices[0].name, "Chain Link 1");
    }

    #[tokio::test]
    async fn overlay_applies_write_rate_status_when_device_reports_it() {
        use halod_shared::types::{WriteRateLimit, WriteRateStatus};

        struct RateReportingDevice {
            inner: MockDevice,
        }
        #[async_trait]
        impl Device for RateReportingDevice {
            fn id(&self) -> &str {
                self.inner.id()
            }
            fn name(&self) -> &str {
                self.inner.name()
            }
            fn vendor(&self) -> &str {
                self.inner.vendor()
            }
            fn model(&self) -> &str {
                self.inner.model()
            }
            fn capabilities(&self) -> Vec<crate::infrastructure::drivers::CapabilityRef<'_>> {
                vec![]
            }
            async fn initialize(&self) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn close(&self) {}
            fn write_rate_status(&self) -> Option<WriteRateStatus> {
                Some(WriteRateStatus {
                    limit: Some(WriteRateLimit {
                        max_bytes_per_sec: 42,
                    }),
                    current_writes_per_sec: 10.0,
                    current_bytes_per_sec: 500.0,
                    rejected_total: 2,
                })
            }
        }

        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(RateReportingDevice {
            inner: MockDevice::new("rate_dev")
                .with_name("Rate Device")
                .with_vendor("Acme")
                .with_model("Widget"),
        });
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;

        let write_rate = snap.devices[0]
            .write_rate
            .expect("device reported write_rate");
        assert_eq!(
            write_rate.limit,
            Some(WriteRateLimit {
                max_bytes_per_sec: 42
            })
        );
        assert_eq!(write_rate.current_writes_per_sec, 10.0);
        assert_eq!(write_rate.rejected_total, 2);
    }

    #[tokio::test]
    async fn write_rate_is_none_when_device_does_not_report_it() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(MockDevice::new("plain_dev"));
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;
        assert!(snap.devices[0].write_rate.is_none());
    }

    #[tokio::test]
    async fn overlay_applies_name_rgb_transform_and_lcd_mode_together() {
        use crate::domain::registry::model::DeviceRecord;
        use halod_shared::types::{DeviceCapability, LcdMode};
        use halod_shared::zone_transform::ZoneContentTransform;

        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "combo".to_string(),
            DeviceRecord {
                name: "Renamed".into(),
                vendor: "Acme".into(),
                model: "Combo".into(),
                active_state: Default::default(),
            },
        );
        let app = Arc::new(AppState::new(cfg));

        let dev = MockDevice::new("combo")
            .with_name("Original")
            .with_vendor("Acme")
            .with_model("Combo")
            .with_rgb()
            .with_lcd();
        dev.rgb.as_ref().unwrap().set_zone_transform(
            "z1".into(),
            ZoneContentTransform {
                flip_h: true,
                ..Default::default()
            },
        );
        dev.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("tmpl".into()));
        let dev: Arc<dyn Device> = Arc::new(dev);
        app.device_registry.write().await.push(dev);
        let cfg = app.config.read().await.clone();
        let snap = app.snapshot_devices(&cfg).await;

        let d = &snap.devices[0];
        assert_eq!(d.name, "Renamed");
        let mut saw_rgb = false;
        let mut saw_lcd = false;
        for cap in &d.capabilities {
            match cap {
                DeviceCapability::Lighting(s) => {
                    saw_rgb = true;
                    assert_eq!(s.channel_transforms.get("z1").map(|t| t.flip_h), Some(true));
                }
                DeviceCapability::Lcd(s) => {
                    saw_lcd = true;
                    assert_eq!(s.mode, LcdMode::Engine);
                }
                _ => {}
            }
        }
        assert!(saw_rgb && saw_lcd, "expected both RGB and LCD capabilities");
    }
}
