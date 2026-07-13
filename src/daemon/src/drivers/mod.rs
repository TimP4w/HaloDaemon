// SPDX-License-Identifier: GPL-3.0-or-later
pub mod chain;
pub mod plugins;
pub mod transports;
pub mod vendors;

use crate::drivers::vendors::generic::devices::common::WireDeviceBuilder;
use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{
    ConnectionType, DeviceCapability, DeviceType, VisibilityState, WireDevice, WriteRateStatus,
};

mod slots;
pub use slots::*;
mod capabilities;
pub use capabilities::*;
mod rate_limit;
pub use rate_limit::*;

/// Capability types a Device can expose.
pub enum CapabilityRef<'a> {
    Fan(&'a dyn FanCapability),
    Rgb(&'a dyn RgbCapability),
    Sensor(&'a dyn SensorCapability),
    Range(&'a dyn RangeCapability),
    Choice(&'a dyn ChoiceCapability),
    Boolean(&'a dyn BooleanCapability),
    Action(&'a dyn ActionCapability),
    Battery(&'a dyn BatteryCapability),
    Connection(&'a dyn ConnectionCapability),
    Equalizer(&'a dyn EqualizerCapability),
    Dpi(&'a dyn DpiCapability),
    OnboardProfiles(&'a dyn OnboardProfilesCapability),
    Lcd(&'a dyn LcdCapability),
    KeyRemap(&'a dyn KeyRemapCapability),
    Chain(&'a dyn ChainCapability),
    Controller(&'a dyn Controller),
    Pairing(&'a dyn PairingCapability),
    TransportSwitchable(&'a dyn TransportSwitchable),
}

macro_rules! capability_dispatch {
    (
        persisting: [$($P:ident),* $(,)?],
        wire_only:  [$($W:ident),* $(,)?] $(,)?
    ) => {
        // No wildcard arms: an unclassified capability is a compile error, not a silent skip.
        impl CapabilityRef<'_> {
            pub fn state_key(&self) -> &'static str {
                match self {
                    $( CapabilityRef::$P(c) => c.state_key(), )*
                    $( CapabilityRef::$W(_) => "", )*
                }
            }

            pub fn save_state(&self) -> serde_json::Value {
                match self {
                    $( CapabilityRef::$P(c) => c.save_state(), )*
                    $( CapabilityRef::$W(_) => serde_json::Value::Null, )*
                }
            }

            pub async fn restore_state(&self, v: &serde_json::Value) {
                match self {
                    $( CapabilityRef::$P(c) => c.restore_state(v).await, )*
                    $( CapabilityRef::$W(_) => {} )*
                }
            }

            pub async fn to_wire(&self) -> Option<DeviceCapability> {
                match self {
                    $( CapabilityRef::$P(c) => c.to_wire().await, )*
                    $( CapabilityRef::$W(c) => c.to_wire().await, )*
                }
            }
        }
    };
}

capability_dispatch!(
    persisting: [Fan, Rgb, Range, Choice, Boolean, Equalizer, Dpi, Lcd, KeyRemap, OnboardProfiles],
    wire_only:  [Sensor, Action, Battery, Connection, Chain, Controller, Pairing, TransportSwitchable],
);

macro_rules! as_capability {
    ($method:ident, $variant:ident, $trait:path) => {
        fn $method(&self) -> Option<&dyn $trait> {
            self.capabilities().into_iter().find_map(|c| match c {
                CapabilityRef::$variant(x) => Some(x),
                _ => None,
            })
        }
    };
}

#[async_trait]
pub trait Device: Send + Sync {
    /// Stable identifier, unique per physical device across runs.
    fn id(&self) -> &str;

    fn name(&self) -> &str;

    /// True if the device's display name is owned by a parent (e.g. a
    /// `ChainHost`) rather than the descriptor/`DeviceRecord`. `set_device_name`
    /// routes these through `ChainCapability::rename_chain_link`, and the
    /// serializer's name-patch skips them so the parent's name wins.
    fn has_external_name(&self) -> bool {
        false
    }

    fn vendor(&self) -> &str;

    fn model(&self) -> &str;

    /// Initializes the device, e.g. opens connections, starts polling tasks, etc. Returns whether the device is currently connected.
    async fn initialize(&self) -> Result<bool>;
    async fn close(&self);

    /// Serializes the device into a format that can be sent to the client.
    async fn serialize(&self) -> WireDevice {
        let mut caps = Vec::new();
        for cap_ref in self.capabilities() {
            if let Some(w) = cap_ref.to_wire().await {
                caps.push(w);
            }
        }
        if let Some(chain) = self.as_chain() {
            chain.enrich_wire_capabilities(&mut caps);
        }
        WireDeviceBuilder::from_parts(
            self.id().to_owned(),
            self.wire_device_name().await,
            self.vendor().to_string(),
            self.model().to_string(),
        )
        .device_type(self.wire_device_type())
        .connection_type(self.wire_connection_type().await)
        .serial_number(self.wire_serial_number())
        .connected(self.wire_device_connected().await)
        .capabilities(caps)
        .integration_id(self.integration_id())
        .build()
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Other
    }

    /// The owning plugin id when this device *is* an integration's root
    /// (e.g. the OpenRGB SDK client) rather than a real device. `None` for
    /// every other device, including the devices an integration exposes as
    /// children.
    fn integration_id(&self) -> Option<String> {
        None
    }

    fn owning_plugin_id(&self) -> Option<String> {
        None
    }

    async fn wire_connection_type(&self) -> Option<ConnectionType> {
        None
    }

    fn wire_serial_number(&self) -> Option<String> {
        None
    }

    async fn wire_device_connected(&self) -> bool {
        true
    }

    /// `false` marks the device present-but-offline (e.g. an integration whose
    /// server dropped): engines skip it and the GUI greys it. Default `true`.
    fn is_live(&self) -> bool {
        true
    }

    async fn wire_device_name(&self) -> String {
        self.name().to_string()
    }

    /// Transport-independent hardware serial (e.g. Logitech unit ID).
    /// Used to detect the same physical device appearing on a different transport.
    fn hardware_serial(&self) -> Option<String> {
        None
    }

    /// All capabilities this device exposes.  Add a `CapabilityRef` variant when
    /// a new capability is introduced; this method never grows for existing ones.
    fn capabilities(&self) -> Vec<CapabilityRef<'_>>;

    as_capability!(as_fan, Fan, FanCapability);
    as_capability!(as_rgb, Rgb, RgbCapability);
    as_capability!(as_sensor_capability, Sensor, SensorCapability);
    as_capability!(as_range, Range, RangeCapability);
    as_capability!(as_choice, Choice, ChoiceCapability);
    as_capability!(as_boolean, Boolean, BooleanCapability);
    as_capability!(as_action, Action, ActionCapability);
    #[cfg(test)]
    as_capability!(as_battery, Battery, BatteryCapability);
    as_capability!(as_equalizer, Equalizer, EqualizerCapability);
    as_capability!(as_dpi, Dpi, DpiCapability);
    as_capability!(
        as_onboard_profiles,
        OnboardProfiles,
        OnboardProfilesCapability
    );
    as_capability!(as_lcd, Lcd, LcdCapability);
    as_capability!(as_key_remap, KeyRemap, KeyRemapCapability);
    as_capability!(as_chain, Chain, ChainCapability);
    as_capability!(as_controller, Controller, Controller);
    as_capability!(as_pairing, Pairing, PairingCapability);
    as_capability!(
        as_transport_switchable,
        TransportSwitchable,
        TransportSwitchable
    );

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        None
    }

    fn active_state(&self) -> VisibilityState {
        self.visibility_slot().map(|s| s.get()).unwrap_or_default()
    }

    fn set_active_state(&self, state: VisibilityState) {
        if let Some(slot) = self.visibility_slot() {
            slot.set(state);
        }
    }

    async fn save_state(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        for cap in self.capabilities() {
            let key = cap.state_key();
            if key.is_empty() {
                continue;
            }
            let v = cap.save_state();
            if !v.is_null() {
                obj.insert(key.to_string(), v);
            }
        }
        if obj.is_empty() {
            serde_json::Value::Null
        } else {
            obj.into()
        }
    }

    async fn load_state(&self, state: &serde_json::Value) {
        for cap in self.capabilities() {
            let key = cap.state_key();
            if key.is_empty() {
                continue;
            }
            if let Some(v) = state.get(key) {
                cap.restore_state(v).await;
            }
        }
    }

    /// Driver-specific diagnostic key/value pairs surfaced to the debug UI.
    /// Default is empty; the generic transport info (vid/pid/path/interface)
    /// is added by the daemon-side debug usecase, not by the device itself.
    fn debug_info_extra(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Transport label for the debug UI. `None` lets the daemon fall back to
    /// HID-tracking + id-prefix heuristics; drivers whose transport can't be
    /// inferred from those (e.g. ENE GPU lives on a `SmbusBusKind::Gpu` bus
    /// served by NvAPI, not the chipset SMBus) should override this.
    fn debug_transport(&self) -> Option<&'static str> {
        None
    }

    /// Live write-rate limit and throughput for the debug/Info UI. `None`
    /// when the device hasn't wired up live stats from its transport.
    fn write_rate_status(&self) -> Option<WriteRateStatus> {
        None
    }

    /// Devices that need application-level setup after registration (e.g. starting
    /// notification watchers, registering dynamic children) implement
    /// [`PostRegisterHook`] and return `Some(self)` here. The registration usecase
    /// calls the hook after the device is pushed to `AppState::devices`, so the
    /// device itself never holds a direct reference to `AppState`.
    fn as_post_register_hook(&self) -> Option<&dyn PostRegisterHook> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_state_slot_round_trip() {
        use crate::cooling::config::FanCurveRecord;

        let slot = FanStateSlot::default();
        assert!(slot.fan_curve().is_none(), "starts None");

        let curve = FanCurveRecord {
            sensor_id: Some("cpu".to_string()),
            points: vec![(0.0, 30.0), (100.0, 100.0)],
        };
        slot.set_fan_curve(curve.clone());

        let got = slot.fan_curve().expect("should have a curve");
        assert_eq!(got.sensor_id, curve.sensor_id);
        assert_eq!(got.points, curve.points);

        slot.clear_fan_curve();
        assert!(slot.fan_curve().is_none(), "cleared to None");
    }

    #[test]
    fn rgb_state_slot_canvas_zones_round_trip() {
        use crate::config::PlacedZone;

        let slot = RgbStateSlot::default();
        assert!(slot.canvas_zones().is_empty(), "starts empty");

        let zones = vec![PlacedZone {
            device_id: "dev1".to_string(),
            zone_id: "z1".to_string(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            rotation: 0.0,
            effect: None,
            sampling_mode: Default::default(),
        }];
        slot.set_canvas_zones(zones.clone());

        let got = slot.canvas_zones();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].device_id, zones[0].device_id);
        assert_eq!(got[0].zone_id, zones[0].zone_id);

        slot.set_canvas_zones(vec![]);
        assert!(slot.canvas_zones().is_empty(), "cleared to empty");
    }

    #[test]
    fn lcd_state_slot_round_trip() {
        let slot = LcdStateSlot::default();
        assert!(slot.lcd_template_id().is_none(), "starts None");

        slot.set_lcd_template_id(Some("my-template".to_string()));
        assert_eq!(slot.lcd_template_id().as_deref(), Some("my-template"));

        slot.set_brightness(75);
        assert_eq!(slot.brightness(), 75);

        slot.set_rotation(halod_shared::types::ScreenRotation::R90);
        assert_eq!(slot.rotation(), halod_shared::types::ScreenRotation::R90);

        slot.set_active_image(Some("test.gif".to_string()));
        assert_eq!(slot.active_image().as_deref(), Some("test.gif"));

        slot.set_lcd_template_id(None);
        assert!(slot.lcd_template_id().is_none(), "cleared to None");
    }

    #[tokio::test]
    async fn default_save_state_uses_capabilities() {
        use crate::cooling::config::FanCurveRecord;

        struct MockFanDevice {
            fan: FanStateSlot,
        }
        #[async_trait::async_trait]
        impl FanCapability for MockFanDevice {
            async fn get_duty(&self) -> anyhow::Result<u8> {
                Ok(0)
            }
            async fn set_duty(&self, _: u8) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get_rpm(&self) -> Option<u32> {
                None
            }
            fn fan_state(&self) -> &FanStateSlot {
                &self.fan
            }
        }

        struct MockDevice {
            fan: MockFanDevice,
        }
        #[async_trait::async_trait]
        impl Device for MockDevice {
            fn id(&self) -> &str {
                "mock"
            }
            fn name(&self) -> &str {
                "mock"
            }
            fn vendor(&self) -> &str {
                "test"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn initialize(&self) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn close(&self) {}
            fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
                vec![CapabilityRef::Fan(&self.fan)]
            }
        }

        let dev = MockDevice {
            fan: MockFanDevice {
                fan: FanStateSlot::default(),
            },
        };
        dev.fan.set_fan_curve(FanCurveRecord {
            sensor_id: Some("cpu".into()),
            points: vec![(30.0, 25.0), (70.0, 75.0)],
        });

        let saved = dev.save_state().await;
        assert!(!saved.is_null());
        assert!(!saved["fan_curve"].is_null());

        let dev2 = MockDevice {
            fan: MockFanDevice {
                fan: FanStateSlot::default(),
            },
        };
        dev2.load_state(&saved).await;
        assert_eq!(
            dev2.fan.fan_curve().unwrap().sensor_id.as_deref(),
            Some("cpu")
        );
    }

    #[test]
    fn kv_state_cache_round_trip_i32() {
        let cache: KvStateCache<i32> = KvStateCache::default();
        cache.record("brightness", 80);
        cache.record("volume", 50);
        let saved = cache.save();
        assert_eq!(saved["brightness"], 80);
        assert_eq!(saved["volume"], 50);
        let pairs = cache.load_pairs(&saved);
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map["brightness"], 80);
        assert_eq!(map["volume"], 50);
    }

    #[test]
    fn kv_state_cache_empty_returns_null() {
        let cache: KvStateCache<i32> = KvStateCache::default();
        assert!(cache.save().is_null());
    }

    #[test]
    fn kv_state_cache_get() {
        let cache: KvStateCache<i32> = KvStateCache::default();
        cache.record("vol", 42);
        assert_eq!(cache.get("vol"), Some(42));
        assert_eq!(cache.get("missing"), None);
    }

    #[tokio::test]
    async fn capability_ref_dispatches_state_key() {
        struct MockFan {
            fan: FanStateSlot,
        }
        #[async_trait::async_trait]
        impl FanCapability for MockFan {
            async fn get_duty(&self) -> anyhow::Result<u8> {
                Ok(50)
            }
            async fn set_duty(&self, _: u8) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get_rpm(&self) -> Option<u32> {
                None
            }
            fn fan_state(&self) -> &FanStateSlot {
                &self.fan
            }
        }
        let fan = MockFan {
            fan: FanStateSlot::default(),
        };
        let cap = CapabilityRef::Fan(&fan);
        assert_eq!(cap.state_key(), "fan_curve");
        assert!(cap.save_state().is_null());
        fan.set_fan_curve(crate::cooling::config::FanCurveRecord {
            sensor_id: Some("cpu".into()),
            points: vec![(30.0, 20.0), (100.0, 100.0)],
        });
        let saved = cap.save_state();
        assert!(!saved.is_null());
        fan.clear_fan_curve();
        cap.restore_state(&saved).await;
        assert!(fan.fan_curve().is_some());
    }

    #[tokio::test]
    async fn rgb_save_restore_preserves_zone_transforms() {
        use crate::test_support::MockDevice;
        use halod_shared::zone_transform::ZoneContentTransform;

        let dev = MockDevice::new("dev1").with_rgb();
        let rgb = dev.as_rgb().expect("has rgb");

        let transform = ZoneContentTransform {
            flip_h: true,
            flip_v: false,
            reverse: false,
            led_offset: 0,
            swap_rings: false,
        };
        rgb.set_zone_transform("zone_a".to_string(), transform);

        let saved = rgb.save_state();
        assert!(
            !saved.is_null(),
            "state should be non-null with a transform"
        );

        // Simulate restore on a fresh slot.
        let dev2 = MockDevice::new("dev2").with_rgb();
        let rgb2 = dev2.as_rgb().expect("has rgb");
        rgb2.restore_state(&saved).await;

        let restored = rgb2.zone_transforms();
        let t = restored
            .get("zone_a")
            .expect("zone_a transform should be preserved across save/restore");
        assert!(t.flip_h, "flip_h should be true after restore");
    }
}
