// SPDX-License-Identifier: GPL-3.0-or-later
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use halod_shared::types::{
    ConflictConfidence, ConflictDeviceSource, ConflictParticipant, DeviceConflictSummary,
    VisibilityState,
};

use crate::{
    domain::device::{CapabilityRef, Device, KeyboardLayoutSlot, VisibilitySlot},
    domain::registry::observers::discovery::DiscoveryHandle,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdentityScope {
    Local,
    Remote(String),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LocationKey {
    HidPath(String),
    UsbPort {
        bus: u8,
        port_path: Vec<u8>,
        interface: u8,
    },
    Smbus {
        bus: u8,
        address: u8,
    },
    Opaque(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DeviceIdentity {
    pub scope: Option<IdentityScope>,
    pub serial: Option<String>,
    pub location: Option<LocationKey>,
    pub usb: Option<(u16, u16)>,
    /// Ephemeral libusb address, retained for diagnostics/routing but excluded
    /// from the stable physical location key.
    pub usb_address: Option<u8>,
}

impl DeviceIdentity {
    pub fn local() -> Self {
        Self {
            scope: Some(IdentityScope::Local),
            ..Default::default()
        }
    }

    pub fn is_empty(&self) -> bool {
        self.serial.is_none() && self.location.is_none() && self.usb.is_none()
    }

    pub fn strength(&self) -> u8 {
        if self.serial.is_some() {
            3
        } else if self.location.is_some() {
            2
        } else if self.usb.is_some() {
            1
        } else {
            0
        }
    }

    pub fn serial(value: Option<String>) -> Self {
        Self {
            scope: Some(IdentityScope::Local),
            serial: normalize_serial(value.as_deref()),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceOrigin {
    Builtin,
    Plugin(String),
    Integration(String),
}

impl DeviceOrigin {
    fn conflict_source(&self) -> ConflictDeviceSource {
        match self {
            Self::Builtin => ConflictDeviceSource::Builtin,
            Self::Plugin(id) => ConflictDeviceSource::Plugin(id.clone()),
            Self::Integration(id) => ConflictDeviceSource::Integration(id.clone()),
        }
    }
}

impl DeviceOrigin {
    fn weakly_distinct_from(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Builtin, Self::Builtin) => false,
            (Self::Plugin(a), Self::Plugin(b)) | (Self::Integration(a), Self::Integration(b)) => {
                a != b
            }
            _ => true,
        }
    }

    fn is_integration(&self) -> bool {
        matches!(self, Self::Integration(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchEvidence {
    ConfirmedSerial,
    ConfirmedLocation,
    PossibleUsb,
    ContradictedSerial,
    None,
}

pub fn normalize_serial(value: Option<&str>) -> Option<String> {
    value
        .map(|v| {
            v.trim_matches(|c: char| c.is_whitespace() || c == '\0')
                .to_ascii_lowercase()
        })
        .filter(|v| !v.is_empty())
}

pub fn location_from_openrgb(value: Option<&str>) -> Option<LocationKey> {
    let value = value?.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if value.is_empty() {
        return None;
    }
    let path = value
        .strip_prefix("HID:")
        .or_else(|| value.strip_prefix("hid:"))
        .map(str::trim)
        .unwrap_or(value);
    if path.starts_with("/dev/hidraw")
        || path.starts_with("\\\\?\\hid#")
        || path.starts_with("\\\\?\\HID#")
    {
        return Some(LocationKey::HidPath(normalize_hid_path(path)));
    }
    Some(LocationKey::Opaque(value.to_owned()))
}

fn normalize_hid_path(path: &str) -> String {
    #[cfg(windows)]
    {
        path.trim().to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        path.trim().to_owned()
    }
}

pub fn identity_from_handle(handle: &DiscoveryHandle<'_>) -> DeviceIdentity {
    let mut identity = DeviceIdentity::local();
    match handle {
        DiscoveryHandle::Hid {
            vid,
            pid,
            path,
            serial,
            ..
        } => {
            identity.serial = normalize_serial(*serial);
            identity.usb = Some((*vid, *pid));
            identity.location = Some(LocationKey::HidPath(normalize_hid_path(path)));
        }
        DiscoveryHandle::UsbNonHid {
            vid,
            pid,
            bus,
            address,
            port_path,
            serial,
            interface_number,
        } => {
            identity.usb = Some((*vid, *pid));
            identity.serial = normalize_serial(*serial);
            identity.location = Some(LocationKey::UsbPort {
                bus: *bus,
                port_path: port_path.to_vec(),
                interface: *interface_number,
            });
            identity.usb_address = Some(*address);
        }
        DiscoveryHandle::Smbus {
            addr, bus_number, ..
        } => {
            identity.location = Some(LocationKey::Smbus {
                bus: *bus_number,
                address: *addr,
            });
        }
        DiscoveryHandle::Command { .. } => {}
        #[cfg(target_os = "windows")]
        DiscoveryHandle::AmdSmn { .. } | DiscoveryHandle::Lpcio { .. } => {}
    }
    identity
}

pub fn integration_scope(host: Option<&str>, port: Option<&str>) -> IdentityScope {
    let host = host
        .unwrap_or_default()
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    let local = host == "localhost" || host == "::1" || host.starts_with("127.");
    if local {
        IdentityScope::Local
    } else {
        IdentityScope::Remote(format!("{host}:{}", port.unwrap_or_default().trim()))
    }
}

pub fn compare(
    a: &DeviceIdentity,
    a_origin: &DeviceOrigin,
    b: &DeviceIdentity,
    b_origin: &DeviceOrigin,
) -> MatchEvidence {
    if a.scope != b.scope {
        return MatchEvidence::None;
    }
    match (&a.serial, &b.serial) {
        (Some(a), Some(b)) if a != b => return MatchEvidence::ContradictedSerial,
        (Some(_), Some(_)) => return MatchEvidence::ConfirmedSerial,
        _ => {}
    }
    if a.location.is_some() && a.location == b.location {
        let opaque = matches!(a.location, Some(LocationKey::Opaque(_)));
        if !opaque || (a_origin == b_origin && !matches!(a_origin, DeviceOrigin::Builtin)) {
            return MatchEvidence::ConfirmedLocation;
        }
    }
    if a.usb.is_some() && a.usb == b.usb && a_origin.weakly_distinct_from(b_origin) {
        return MatchEvidence::PossibleUsb;
    }
    MatchEvidence::None
}

pub struct ConflictEntry {
    pub id: String,
    pub identity: DeviceIdentity,
    pub origin: DeviceOrigin,
    pub connected: bool,
    pub active_state: VisibilityState,
    pub integration_root: bool,
}

pub fn detect_conflicts(entries: &[ConflictEntry]) -> Vec<Option<DeviceConflictSummary>> {
    let mut out = vec![None; entries.len()];
    let eligible: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            (e.connected
                && e.active_state != VisibilityState::Disabled
                && !e.integration_root
                && !e.identity.is_empty())
            .then_some(i)
        })
        .collect();
    let mut confirmed = UnionFind::new(entries.len());
    let mut possible: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();

    for (offset, &a) in eligible.iter().enumerate() {
        for &b in &eligible[offset + 1..] {
            match compare(
                &entries[a].identity,
                &entries[a].origin,
                &entries[b].identity,
                &entries[b].origin,
            ) {
                MatchEvidence::ConfirmedSerial => confirmed.union(a, b),
                MatchEvidence::ConfirmedLocation => {}
                MatchEvidence::PossibleUsb => {
                    possible.entry(a).or_default().insert(b);
                    possible.entry(b).or_default().insert(a);
                }
                MatchEvidence::ContradictedSerial | MatchEvidence::None => {}
            }
        }
    }

    // Location equality is not transitive when an identity without a serial
    // sits between two identities that have different serials. Only make a
    // location bucket confirmed when its concrete serial evidence agrees.
    let mut locations: BTreeMap<(IdentityScope, LocationKey), Vec<usize>> = BTreeMap::new();
    for &i in &eligible {
        let Some(scope) = entries[i].identity.scope.clone() else {
            continue;
        };
        let Some(location) = entries[i].identity.location.clone() else {
            continue;
        };
        locations.entry((scope, location)).or_default().push(i);
    }
    for members in locations.into_values() {
        let serials: BTreeSet<&String> = members
            .iter()
            .filter_map(|&i| entries[i].identity.serial.as_ref())
            .collect();
        if serials.len() <= 1 {
            for pair in members.windows(2) {
                let a = pair[0];
                let b = pair[1];
                if matches!(
                    compare(
                        &entries[a].identity,
                        &entries[a].origin,
                        &entries[b].identity,
                        &entries[b].origin
                    ),
                    MatchEvidence::ConfirmedLocation
                ) {
                    confirmed.union(a, b);
                }
            }
        }
    }

    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for &i in &eligible {
        groups.entry(confirmed.find(i)).or_default().push(i);
    }
    for group in groups.values().filter(|g| g.len() > 1) {
        let recommended = recommended(group, entries);
        for &i in group {
            out[i] = Some(DeviceConflictSummary {
                peer_ids: group
                    .iter()
                    .copied()
                    .filter(|&j| j != i)
                    .map(|j| entries[j].id.clone())
                    .collect(),
                recommended_id: entries[recommended].id.clone(),
                confidence: ConflictConfidence::Confirmed,
                participants: participants(group, entries),
            });
        }
    }
    for (&i, peers) in &possible {
        if out[i].is_some() {
            continue;
        }
        let mut possible_participants = vec![i];
        possible_participants.extend(peers.iter().copied());
        let recommended = recommended(&possible_participants, entries);
        out[i] = Some(DeviceConflictSummary {
            peer_ids: peers.iter().map(|&j| entries[j].id.clone()).collect(),
            recommended_id: entries[recommended].id.clone(),
            confidence: ConflictConfidence::Possible,
            participants: participants(&possible_participants, entries),
        });
    }
    out
}

fn participants(indices: &[usize], entries: &[ConflictEntry]) -> Vec<ConflictParticipant> {
    indices
        .iter()
        .map(|&i| ConflictParticipant {
            id: entries[i].id.clone(),
            source: entries[i].origin.conflict_source(),
        })
        .collect()
}

fn recommended(group: &[usize], entries: &[ConflictEntry]) -> usize {
    *group
        .iter()
        .min_by_key(|&&i| {
            (
                std::cmp::Reverse(entries[i].identity.strength()),
                entries[i].origin.is_integration(),
                i,
            )
        })
        .expect("non-empty group")
}

struct UnionFind {
    parents: Vec<usize>,
}
impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parents: (0..n).collect(),
        }
    }
    fn find(&mut self, i: usize) -> usize {
        if self.parents[i] != i {
            let p = self.find(self.parents[i]);
            self.parents[i] = p;
        }
        self.parents[i]
    }
    fn union(&mut self, a: usize, b: usize) {
        let a = self.find(a);
        let b = self.find(b);
        if a != b {
            self.parents[b] = a;
        }
    }
}

pub struct IdentifiedDevice {
    inner: Arc<dyn Device>,
    identity: DeviceIdentity,
    origin: DeviceOrigin,
}

impl IdentifiedDevice {
    pub fn new(inner: Arc<dyn Device>, mut identity: DeviceIdentity, origin: DeviceOrigin) -> Self {
        if identity.serial.is_none() {
            identity.serial = normalize_serial(inner.hardware_serial().as_deref());
        }
        Self {
            inner,
            identity,
            origin,
        }
    }
}

#[async_trait]
impl Device for IdentifiedDevice {
    fn id(&self) -> &str {
        self.inner.id()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn has_external_name(&self) -> bool {
        self.inner.has_external_name()
    }
    fn vendor(&self) -> &str {
        self.inner.vendor()
    }
    fn model(&self) -> &str {
        self.inner.model()
    }
    async fn initialize(&self) -> anyhow::Result<bool> {
        self.inner.initialize().await
    }
    async fn close(&self) {
        self.inner.close().await
    }
    async fn serialize(&self) -> halod_shared::types::WireDevice {
        self.inner.serialize().await
    }
    fn wire_device_type(&self) -> halod_shared::types::DeviceType {
        self.inner.wire_device_type()
    }
    fn integration_id(&self) -> Option<String> {
        self.inner.integration_id()
    }
    fn owning_plugin_id(&self) -> Option<String> {
        self.inner.owning_plugin_id()
    }
    async fn wire_connection_type(&self) -> Option<halod_shared::types::ConnectionType> {
        self.inner.wire_connection_type().await
    }
    fn wire_serial_number(&self) -> Option<String> {
        self.inner
            .wire_serial_number()
            .or_else(|| self.identity.serial.clone())
    }
    fn wire_device_connected(&self) -> bool {
        self.inner.wire_device_connected()
    }
    fn is_live(&self) -> bool {
        self.inner.is_live()
    }
    async fn wire_device_name(&self) -> String {
        self.inner.wire_device_name().await
    }
    fn hardware_serial(&self) -> Option<String> {
        self.inner
            .hardware_serial()
            .or_else(|| self.identity.serial.clone())
    }
    fn identity(&self) -> DeviceIdentity {
        self.identity.clone()
    }
    fn conflict_origin(&self) -> DeviceOrigin {
        self.origin.clone()
    }
    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        self.inner.capabilities()
    }
    fn chain_host(&self) -> Option<&Arc<crate::domain::device::chain::LightingDivisionHost>> {
        self.inner.chain_host()
    }
    fn keyboard_layout_slot(&self) -> Option<&KeyboardLayoutSlot> {
        self.inner.keyboard_layout_slot()
    }
    fn is_unrecoverable(&self) -> bool {
        self.inner.is_unrecoverable()
    }
    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        self.inner.visibility_slot()
    }
    fn active_state(&self) -> VisibilityState {
        self.inner.active_state()
    }
    fn set_active_state(&self, state: VisibilityState) {
        self.inner.set_active_state(state)
    }
    async fn save_state(&self) -> serde_json::Value {
        self.inner.save_state().await
    }
    async fn load_state(&self, state: &serde_json::Value) {
        self.inner.load_state(state).await
    }
    fn debug_info_extra(&self) -> Vec<(String, String)> {
        let mut fields = self.inner.debug_info_extra();
        if let Some(serial) = &self.identity.serial {
            fields.push(("identity_serial".into(), serial.clone()));
        }
        if let Some(location) = &self.identity.location {
            fields.push((
                "identity_location".into(),
                match location {
                    LocationKey::HidPath(path) => format!("hid:{path}"),
                    LocationKey::UsbPort {
                        bus,
                        port_path,
                        interface,
                    } => format!(
                        "usb:{bus}-{}:if{interface}",
                        port_path
                            .iter()
                            .map(u8::to_string)
                            .collect::<Vec<_>>()
                            .join(".")
                    ),
                    LocationKey::Smbus { bus, address } => {
                        format!("smbus:i2c-{bus}@0x{address:02x}")
                    }
                    LocationKey::Opaque(value) => format!("opaque:{value}"),
                },
            ));
        }
        if let Some((vid, pid)) = self.identity.usb {
            fields.push(("identity_usb".into(), format!("{vid:04x}:{pid:04x}")));
        }
        if let Some(address) = self.identity.usb_address {
            fields.push(("identity_usb_address".into(), address.to_string()));
        }
        if let Some(scope) = &self.identity.scope {
            fields.push((
                "identity_scope".into(),
                match scope {
                    IdentityScope::Local => "local".into(),
                    IdentityScope::Remote(endpoint) => format!("remote:{endpoint}"),
                },
            ));
        }
        fields.push((
            "identity_strength".into(),
            self.identity.strength().to_string(),
        ));
        fields.push((
            "identity_origin".into(),
            match &self.origin {
                DeviceOrigin::Builtin => "builtin".into(),
                DeviceOrigin::Plugin(id) => format!("plugin:{id}"),
                DeviceOrigin::Integration(id) => format!("integration:{id}"),
            },
        ));
        fields
    }
    fn debug_transport(&self) -> Option<&'static str> {
        self.inner.debug_transport()
    }
    fn write_rate_status(&self) -> Option<halod_shared::types::WriteRateStatus> {
        self.inner.write_rate_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, serial: Option<&str>, location: Option<&str>) -> ConflictEntry {
        ConflictEntry {
            id: id.into(),
            identity: DeviceIdentity {
                scope: Some(IdentityScope::Local),
                serial: normalize_serial(serial),
                location: location_from_openrgb(location),
                usb: None,
                usb_address: None,
            },
            origin: DeviceOrigin::Builtin,
            connected: true,
            active_state: VisibilityState::Visible,
            integration_root: false,
        }
    }
    #[test]
    fn serial_is_normalized() {
        assert_eq!(normalize_serial(Some(" A\0 ")), Some("a".into()));
    }
    #[test]
    fn hid_location_is_normalized() {
        assert_eq!(
            location_from_openrgb(Some("HID: /dev/hidraw6\0")),
            Some(LocationKey::HidPath("/dev/hidraw6".into()))
        );
    }
    #[tokio::test]
    async fn wrapper_forwards_chain_host_to_inner() {
        use crate::domain::device::chain::{
            ChannelDescriptor, LightingDivisionAdapter, LightingDivisionHost,
        };
        use anyhow::Result;
        use async_trait::async_trait;

        struct StubAdapter;
        #[async_trait]
        impl LightingDivisionAdapter for StubAdapter {
            fn parent_id(&self) -> String {
                "p".into()
            }
            fn channels(&self) -> Vec<ChannelDescriptor> {
                vec![]
            }
            async fn write_divided_frame(&self, _: &str, _: &[u8]) -> Result<()> {
                Ok(())
            }
        }

        struct ChainDevice(Arc<LightingDivisionHost>);
        #[async_trait]
        impl Device for ChainDevice {
            fn id(&self) -> &str {
                "d"
            }
            fn name(&self) -> &str {
                "d"
            }
            fn vendor(&self) -> &str {
                "v"
            }
            fn model(&self) -> &str {
                "m"
            }
            async fn initialize(&self) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn close(&self) {}
            fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
                vec![]
            }
            fn chain_host(&self) -> Option<&Arc<LightingDivisionHost>> {
                Some(&self.0)
            }
        }

        let host = LightingDivisionHost::new(Arc::new(StubAdapter));
        let inner: Arc<dyn Device> = Arc::new(ChainDevice(host));
        let identity = DeviceIdentity {
            scope: Some(IdentityScope::Local),
            serial: None,
            location: None,
            usb: None,
            usb_address: None,
        };
        let wrapped = IdentifiedDevice::new(inner, identity, DeviceOrigin::Builtin);
        assert!(
            wrapped.chain_host().is_some(),
            "IdentifiedDevice must forward chain_host so chain usecases reach the host"
        );
    }

    #[test]
    fn conflicting_serials_do_not_merge_through_location() {
        let entries = vec![
            entry("a", Some("x"), Some("HID: /dev/hidraw0")),
            entry("b", None, Some("HID: /dev/hidraw0")),
            entry("c", Some("y"), Some("HID: /dev/hidraw0")),
        ];
        let found = detect_conflicts(&entries);
        assert!(found[0].is_none());
        assert!(found[2].is_none());
    }

    fn smbus_identity(bus: u8, address: u8) -> DeviceIdentity {
        DeviceIdentity {
            scope: Some(IdentityScope::Local),
            location: Some(LocationKey::Smbus { bus, address }),
            ..Default::default()
        }
    }

    #[test]
    fn smbus_location_confirms_the_same_hardware_across_sources() {
        // Same (bus, address) is the same physical device, confirmed even across
        // differing origins — unlike an opaque location, which requires a
        // matching non-builtin origin.
        assert_eq!(
            compare(
                &smbus_identity(1, 0x70),
                &DeviceOrigin::Plugin("ene_smbus".into()),
                &smbus_identity(1, 0x70),
                &DeviceOrigin::Integration("openrgb".into()),
            ),
            MatchEvidence::ConfirmedLocation
        );
        // A different address (or bus) is a different device.
        assert_eq!(
            compare(
                &smbus_identity(1, 0x70),
                &DeviceOrigin::Builtin,
                &smbus_identity(1, 0x71),
                &DeviceOrigin::Builtin,
            ),
            MatchEvidence::None
        );
    }

    #[test]
    fn smbus_duplicates_across_sources_form_a_conflict_group() {
        let smbus_entry = |id: &str, origin| ConflictEntry {
            id: id.into(),
            identity: smbus_identity(1, 0x70),
            origin,
            connected: true,
            active_state: VisibilityState::Visible,
            integration_root: false,
        };
        let entries = vec![
            smbus_entry("ene_smbus-addr70", DeviceOrigin::Plugin("ene_smbus".into())),
            smbus_entry(
                "openrgb-ctrl_0",
                DeviceOrigin::Integration("openrgb".into()),
            ),
        ];
        let found = detect_conflicts(&entries);
        assert_eq!(
            found[0].as_ref().unwrap().peer_ids,
            vec!["openrgb-ctrl_0".to_string()]
        );
        assert_eq!(
            found[1].as_ref().unwrap().peer_ids,
            vec!["ene_smbus-addr70".to_string()]
        );
    }

    #[test]
    fn usb_identity_is_stable_across_address_changes_and_distinct_by_port() {
        let a = identity_from_handle(&DiscoveryHandle::UsbNonHid {
            vid: 0x1234,
            pid: 0x5678,
            bus: 1,
            address: 4,
            port_path: &[2, 3],
            serial: None,
            interface_number: 1,
        });
        let moved = identity_from_handle(&DiscoveryHandle::UsbNonHid {
            vid: 0x1234,
            pid: 0x5678,
            bus: 1,
            address: 9,
            port_path: &[2, 3],
            serial: None,
            interface_number: 1,
        });
        let other = identity_from_handle(&DiscoveryHandle::UsbNonHid {
            vid: 0x1234,
            pid: 0x5678,
            bus: 1,
            address: 5,
            port_path: &[2, 4],
            serial: None,
            interface_number: 1,
        });
        assert_eq!(a.location, moved.location);
        assert_ne!(a.usb_address, moved.usb_address);
        assert_ne!(a.location, other.location);
    }
}
