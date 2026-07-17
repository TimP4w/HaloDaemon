// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded, namespaced latest-value snapshots shared by plugin runtimes.

use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Result};
use mlua::{Lua, Table, Value};

use halod_shared::types::{Sensor, SensorType, SensorUnit, VisibilityState};

pub const MAX_PLUGIN_RECORD_BYTES: usize = 32 * 1024;
const MAX_GLOBAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_PLUGIN_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 6;
const MAX_MAP_FIELDS: usize = 64;
const MAX_ARRAY_ITEMS: usize = 256;
const MAX_VALUES: usize = 512;
const MAX_STRING_BYTES: usize = 4096;
const MAX_MAP_KEY_BYTES: usize = 64;
const MIN_PUBLISH_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Debug, PartialEq)]
pub enum DataValue {
    Bool(bool),
    Integer(i64),
    Number(f64),
    String(String),
    Array(Vec<DataValue>),
    Map(BTreeMap<String, DataValue>),
}

impl DataValue {
    pub fn from_lua(value: Value) -> Result<Self> {
        let mut count = 0;
        let (value, bytes) = parse_lua(value, 0, &mut count)?;
        ensure!(
            bytes <= MAX_PLUGIN_RECORD_BYTES,
            "data record exceeds 32768 bytes"
        );
        Ok(value)
    }

    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<Value> {
        match self {
            Self::Bool(value) => Ok(Value::Boolean(*value)),
            Self::Integer(value) => Ok(Value::Integer(*value)),
            Self::Number(value) => Ok(Value::Number(*value)),
            Self::String(value) => Ok(Value::String(lua.create_string(value)?)),
            Self::Array(values) => {
                let table = lua.create_table_with_capacity(values.len(), 0)?;
                for (index, value) in values.iter().enumerate() {
                    table.set(index + 1, value.to_lua(lua)?)?;
                }
                Ok(Value::Table(table))
            }
            Self::Map(values) => {
                let table = lua.create_table_with_capacity(0, values.len())?;
                for (key, value) in values {
                    table.set(key.as_str(), value.to_lua(lua)?)?;
                }
                Ok(Value::Table(table))
            }
        }
    }

    fn accounted_bytes(&self) -> usize {
        match self {
            Self::Bool(_) => 1,
            Self::Integer(_) | Self::Number(_) => 8,
            Self::String(value) => value.len(),
            Self::Array(values) => {
                values.iter().map(Self::accounted_bytes).sum::<usize>() + values.len() * 2
            }
            Self::Map(values) => values
                .iter()
                .map(|(key, value)| key.len() + value.accounted_bytes() + 2)
                .sum(),
        }
    }
}

fn parse_lua(value: Value, depth: usize, count: &mut usize) -> Result<(DataValue, usize)> {
    ensure!(
        depth <= MAX_DEPTH,
        "data record exceeds maximum depth {MAX_DEPTH}"
    );
    *count += 1;
    ensure!(
        *count <= MAX_VALUES,
        "data record exceeds {MAX_VALUES} values"
    );
    match value {
        Value::Boolean(value) => Ok((DataValue::Bool(value), 1)),
        Value::Integer(value) => Ok((DataValue::Integer(value), 8)),
        Value::Number(value) if value.is_finite() => Ok((DataValue::Number(value), 8)),
        Value::Number(_) => bail!("data record contains a non-finite number"),
        Value::String(value) => {
            let value = value
                .to_str()
                .map_err(|error| anyhow!(error.to_string()))?
                .to_owned();
            ensure!(
                value.len() <= MAX_STRING_BYTES,
                "data string exceeds {MAX_STRING_BYTES} bytes"
            );
            let len = value.len();
            Ok((DataValue::String(value), len))
        }
        Value::Table(table) => parse_table(table, depth, count),
        Value::Nil => bail!("nil is not a data value"),
        _ => bail!("unsupported Lua value in data record"),
    }
}

fn parse_table(table: Table, depth: usize, count: &mut usize) -> Result<(DataValue, usize)> {
    let mut integer_values = BTreeMap::new();
    let mut map_values = BTreeMap::new();
    let mut bytes = 0;
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| anyhow!(error.to_string()))?;
        let (parsed, value_bytes) = parse_lua(value, depth + 1, count)?;
        bytes += value_bytes + 2;
        match key {
            Value::Integer(index) if index > 0 => {
                integer_values.insert(index as usize, parsed);
            }
            Value::String(key) => {
                ensure!(
                    integer_values.is_empty(),
                    "data tables cannot mix array and map keys"
                );
                let key = key
                    .to_str()
                    .map_err(|error| anyhow!(error.to_string()))?
                    .to_owned();
                ensure!(
                    !key.is_empty() && key.len() <= MAX_MAP_KEY_BYTES,
                    "data map key is empty or too long"
                );
                bytes += key.len();
                map_values.insert(key, parsed);
            }
            _ => bail!("data table keys must be strings or positive contiguous integers"),
        }
    }
    ensure!(
        integer_values.is_empty() || map_values.is_empty(),
        "data tables cannot mix array and map keys"
    );
    if !integer_values.is_empty() {
        ensure!(
            integer_values.len() <= MAX_ARRAY_ITEMS,
            "data array exceeds {MAX_ARRAY_ITEMS} items"
        );
        let len = integer_values.len();
        ensure!(
            integer_values.keys().copied().eq(1..=len),
            "data arrays must be contiguous and one-based"
        );
        Ok((
            DataValue::Array(integer_values.into_values().collect()),
            bytes,
        ))
    } else {
        ensure!(
            map_values.len() <= MAX_MAP_FIELDS,
            "data map exceeds {MAX_MAP_FIELDS} fields"
        );
        Ok((DataValue::Map(map_values), bytes))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataPolicy {
    pub stale_after: Duration,
    pub min_notify_interval: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotStatus {
    Fresh,
    Stale,
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct DataSnapshot {
    pub status: SnapshotStatus,
    pub value: Option<DataValue>,
    pub published_at: Option<u64>,
    pub stale_at: Option<u64>,
    pub revision: u64,
    pub reason: Option<String>,
}

struct Record {
    owner: String,
    value: Option<DataValue>,
    bytes: usize,
    published_at: Option<u64>,
    published_instant: Option<Instant>,
    policy: DataPolicy,
    revision: u64,
    reason: Option<String>,
    last_publish: Option<Instant>,
    last_notification: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    records: HashMap<String, Record>,
    revision: u64,
}

pub struct DataBus {
    inner: Mutex<Inner>,
    changes: tokio::sync::broadcast::Sender<DataChange>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)] // Phase 12 consumes the bounded change stream.
pub struct DataChange {
    pub key: String,
    pub revision: u64,
}

impl Default for DataBus {
    fn default() -> Self {
        let (changes, _) = tokio::sync::broadcast::channel(256);
        Self {
            inner: Mutex::new(Inner::default()),
            changes,
        }
    }
}

impl DataBus {
    pub fn replace_host_sensors(&self, mut sensors: Vec<(String, Sensor)>) {
        sensors.sort_by(|(left_owner, left), (right_owner, right)| {
            left.id
                .cmp(&right.id)
                .then_with(|| left_owner.cmp(right_owner))
        });
        let policy = host_policy(Duration::from_secs(3));
        let expected: std::collections::HashSet<String> = sensors
            .iter()
            .map(|(_, sensor)| sensor_key(&sensor.id))
            .collect();
        for (key, snapshot) in self.statuses_for_owner("host") {
            if key.starts_with("host.sensors.")
                && key != "host.sensors.catalog"
                && !expected.contains(&key)
                && snapshot.status != SnapshotStatus::Unavailable
            {
                let _ = self.invalidate("host", &key, "sensor_removed");
            }
        }

        let mut catalog = Vec::with_capacity(sensors.len());
        for (device_id, sensor) in sensors {
            let key = sensor_key(&sensor.id);
            let mut value = BTreeMap::new();
            value.insert("device_id".into(), DataValue::String(device_id.clone()));
            value.insert("id".into(), DataValue::String(sensor.id.clone()));
            value.insert("label".into(), DataValue::String(sensor.name.clone()));
            value.insert("value".into(), DataValue::Number(sensor.value));
            value.insert(
                "unit".into(),
                DataValue::String(sensor_unit_name(&sensor.unit).into()),
            );
            value.insert(
                "sensor_type".into(),
                DataValue::String(sensor_type_name(&sensor.sensor_type).into()),
            );
            value.insert(
                "visibility".into(),
                DataValue::String(visibility_name(&sensor.visibility).into()),
            );
            let _ = self.publish("host", &key, DataValue::Map(value), policy);

            let mut item = BTreeMap::new();
            item.insert("device_id".into(), DataValue::String(device_id));
            item.insert("id".into(), DataValue::String(sensor.id));
            item.insert("key".into(), DataValue::String(key));
            catalog.push(DataValue::Map(item));
        }
        let _ = self.publish(
            "host",
            "host.sensors.catalog",
            DataValue::Array(catalog),
            policy,
        );
    }

    pub fn sensors(&self) -> HashMap<String, Sensor> {
        self.sensor_entries()
            .into_iter()
            .map(|(_, sensor)| (sensor.id.clone(), sensor))
            .collect()
    }

    pub fn sensors_for_device(&self, device_id: &str) -> Vec<Sensor> {
        self.sensor_entries()
            .into_iter()
            .filter_map(|(owner, sensor)| (owner == device_id).then_some(sensor))
            .collect()
    }

    pub fn sensor_owner(&self, sensor_id: &str) -> Option<String> {
        self.sensor_entries()
            .into_iter()
            .find_map(|(owner, sensor)| (sensor.id == sensor_id).then_some(owner))
    }

    fn sensor_entries(&self) -> Vec<(String, Sensor)> {
        let catalog = self.read("host.sensors.catalog");
        if catalog.status != SnapshotStatus::Fresh {
            return Vec::new();
        }
        let Some(DataValue::Array(items)) = catalog.value else {
            return Vec::new();
        };
        items
            .into_iter()
            .filter_map(|item| {
                let DataValue::Map(item) = item else {
                    return None;
                };
                let key = data_string(&item, "key")?;
                let snapshot = self.read(key);
                if snapshot.status != SnapshotStatus::Fresh {
                    return None;
                }
                let DataValue::Map(value) = snapshot.value? else {
                    return None;
                };
                let device_id = data_string(&value, "device_id")?.to_owned();
                let sensor = Sensor {
                    id: data_string(&value, "id")?.to_owned(),
                    name: data_string(&value, "label")?.to_owned(),
                    value: data_number(&value, "value")?,
                    unit: parse_sensor_unit(data_string(&value, "unit")?)?,
                    sensor_type: parse_sensor_type(data_string(&value, "sensor_type")?)?,
                    visibility: parse_visibility(data_string(&value, "visibility")?)?,
                };
                Some((device_id, sensor))
            })
            .collect()
    }

    /// Stable render-cache signature and availability for an exact key or the
    /// bounded `host.sensors.*` scope.
    pub fn scope_state(&self, scope: &str) -> (u64, bool) {
        let mut keys = {
            let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            inner
                .records
                .keys()
                .filter(|key| {
                    key.as_str() == scope
                        || (scope == "host.sensors.*" && key.starts_with("host.sensors."))
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        keys.sort_unstable();
        let mut hasher = DefaultHasher::new();
        let mut available = false;
        for key in keys {
            let snapshot = self.read(&key);
            key.hash(&mut hasher);
            snapshot.revision.hash(&mut hasher);
            match snapshot.status {
                SnapshotStatus::Fresh => {
                    1u8.hash(&mut hasher);
                    available = true;
                }
                SnapshotStatus::Stale => {
                    2u8.hash(&mut hasher);
                    available = true;
                }
                SnapshotStatus::Unavailable => 3u8.hash(&mut hasher),
            }
        }
        (hasher.finish(), available)
    }

    pub fn publish(
        &self,
        owner: &str,
        key: &str,
        value: DataValue,
        policy: DataPolicy,
    ) -> Result<u64> {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = inner.records.get(key) {
            ensure!(
                record.owner == owner,
                "data record '{key}' is owned by another producer"
            );
        }
        let owner_records = inner
            .records
            .values()
            .filter(|record| record.owner == owner)
            .count();
        ensure!(
            owner == "host" || inner.records.contains_key(key) || owner_records < 32,
            "plugin declares more than 32 retained data records"
        );
        if owner != "host"
            && inner
                .records
                .get(key)
                .and_then(|record| record.last_publish)
                .is_some_and(|last| now.duration_since(last) < MIN_PUBLISH_INTERVAL)
        {
            bail!("data publication rate limit exceeded for '{key}'");
        }
        if let Some(record) = inner.records.get_mut(key) {
            let still_fresh = record
                .published_instant
                .is_some_and(|published| now.duration_since(published) < record.policy.stale_after);
            if still_fresh && record.value.as_ref() == Some(&value) {
                record.published_at = Some(unix_seconds());
                record.published_instant = Some(now);
                record.policy = policy;
                record.reason = None;
                record.last_publish = Some(now);
                return Ok(record.revision);
            }
        }
        let bytes = value.accounted_bytes();
        let existing_bytes = inner.records.get(key).map_or(0, |record| record.bytes);
        let plugin_bytes: usize = inner
            .records
            .values()
            .filter(|record| record.owner == owner)
            .map(|record| record.bytes)
            .sum();
        ensure!(
            owner == "host" || plugin_bytes - existing_bytes + bytes <= MAX_PLUGIN_BYTES,
            "plugin data exceeds retained-byte limit"
        );
        let global_bytes: usize = inner.records.values().map(|record| record.bytes).sum();
        ensure!(
            global_bytes - existing_bytes + bytes <= MAX_GLOBAL_BYTES,
            "host data bus is full"
        );
        inner.revision = inner.revision.wrapping_add(1).max(1);
        let revision = inner.revision;
        let notify = inner
            .records
            .get(key)
            .and_then(|record| record.last_notification)
            .is_none_or(|last| now.duration_since(last) >= policy.min_notify_interval);
        inner.records.insert(
            key.to_owned(),
            Record {
                owner: owner.to_owned(),
                value: Some(value),
                bytes,
                published_at: Some(unix_seconds()),
                published_instant: Some(now),
                policy,
                revision,
                reason: None,
                last_publish: Some(now),
                last_notification: notify.then_some(now),
            },
        );
        if notify {
            let _ = self.changes.send(DataChange {
                key: key.to_owned(),
                revision,
            });
        }
        Ok(revision)
    }

    pub fn invalidate(&self, owner: &str, key: &str, reason: &str) -> Result<u64> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = inner.records.get(key) {
            ensure!(
                record.owner == owner,
                "data record '{key}' is owned by another producer"
            );
        }
        inner.revision = inner.revision.wrapping_add(1).max(1);
        let revision = inner.revision;
        let policy = inner.records.get(key).map_or(
            DataPolicy {
                stale_after: Duration::from_secs(1),
                min_notify_interval: Duration::from_millis(16),
            },
            |record| record.policy,
        );
        inner.records.insert(
            key.to_owned(),
            Record {
                owner: owner.to_owned(),
                value: None,
                bytes: 0,
                published_at: None,
                published_instant: None,
                policy,
                revision,
                reason: Some(reason.to_owned()),
                last_publish: None,
                last_notification: Some(Instant::now()),
            },
        );
        let _ = self.changes.send(DataChange {
            key: key.to_owned(),
            revision,
        });
        Ok(revision)
    }

    pub fn invalidate_owner(&self, owner: &str) {
        let keys: Vec<String> = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .records
            .iter()
            .filter(|(_, record)| record.owner == owner)
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            let _ = self.invalidate(owner, &key, "producer_stopped");
        }
    }

    pub fn read(&self, key: &str) -> DataSnapshot {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = inner.records.get(key) else {
            return DataSnapshot {
                status: SnapshotStatus::Unavailable,
                value: None,
                published_at: None,
                stale_at: None,
                revision: 0,
                reason: Some("never_published".into()),
            };
        };
        let stale = record
            .published_instant
            .is_some_and(|published| published.elapsed() >= record.policy.stale_after);
        DataSnapshot {
            status: if record.value.is_none() {
                SnapshotStatus::Unavailable
            } else if stale {
                SnapshotStatus::Stale
            } else {
                SnapshotStatus::Fresh
            },
            value: record.value.clone(),
            published_at: record.published_at,
            stale_at: record
                .published_at
                .map(|time| time.saturating_add(record.policy.stale_after.as_secs())),
            revision: record.revision,
            reason: record.reason.clone(),
        }
    }

    pub fn statuses_for_owner(&self, owner: &str) -> Vec<(String, DataSnapshot)> {
        let keys: Vec<String> = self
            .inner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .records
            .iter()
            .filter(|(_, record)| record.owner == owner)
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .map(|key| {
                let snapshot = self.read(&key);
                (key, snapshot)
            })
            .collect()
    }

    #[allow(dead_code)] // Phase 12 wires this into reactive render scheduling.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<DataChange> {
        self.changes.subscribe()
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn snapshot_to_lua(lua: &Lua, snapshot: &DataSnapshot) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set(
        "status",
        match snapshot.status {
            SnapshotStatus::Fresh => "fresh",
            SnapshotStatus::Stale => "stale",
            SnapshotStatus::Unavailable => "unavailable",
        },
    )?;
    table.set("revision", snapshot.revision)?;
    if let Some(value) = &snapshot.value {
        table.set("value", value.to_lua(lua)?)?;
    }
    if let Some(value) = snapshot.published_at {
        table.set("published_at", value)?;
    }
    if let Some(value) = snapshot.stale_at {
        table.set("stale_at", value)?;
    }
    if let Some(value) = &snapshot.reason {
        table.set("reason", value.as_str())?;
    }
    Ok(table)
}

pub fn mlua_error(error: anyhow::Error) -> mlua::Error {
    mlua::Error::RuntimeError(error.to_string())
}

pub fn sensor_key(id: &str) -> String {
    let mut encoded = String::with_capacity(id.len() * 2);
    for byte in id.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    format!("host.sensors.{encoded}")
}

fn data_string<'a>(value: &'a BTreeMap<String, DataValue>, key: &str) -> Option<&'a str> {
    match value.get(key)? {
        DataValue::String(value) => Some(value),
        _ => None,
    }
}

fn data_number(value: &BTreeMap<String, DataValue>, key: &str) -> Option<f64> {
    match value.get(key)? {
        DataValue::Number(value) => Some(*value),
        DataValue::Integer(value) => Some(*value as f64),
        _ => None,
    }
}

fn parse_sensor_unit(value: &str) -> Option<SensorUnit> {
    match value {
        "celsius" => Some(SensorUnit::Celsius),
        "fahrenheit" => Some(SensorUnit::Fahrenheit),
        "percent" => Some(SensorUnit::Percent),
        "megahertz" => Some(SensorUnit::Megahertz),
        "hours" => Some(SensorUnit::Hours),
        "rpm" => Some(SensorUnit::Rpm),
        _ => None,
    }
}

fn sensor_unit_name(value: &SensorUnit) -> &'static str {
    match value {
        SensorUnit::Celsius => "celsius",
        SensorUnit::Fahrenheit => "fahrenheit",
        SensorUnit::Percent => "percent",
        SensorUnit::Megahertz => "megahertz",
        SensorUnit::Hours => "hours",
        SensorUnit::Rpm => "rpm",
    }
}

fn parse_sensor_type(value: &str) -> Option<SensorType> {
    match value {
        "temperature" => Some(SensorType::Temperature),
        "load" => Some(SensorType::Load),
        "memory" => Some(SensorType::Memory),
        "frequency" => Some(SensorType::Frequency),
        "uptime" => Some(SensorType::Uptime),
        "fan_speed" => Some(SensorType::FanSpeed),
        "fan_duty" => Some(SensorType::FanDuty),
        _ => None,
    }
}

fn sensor_type_name(value: &SensorType) -> &'static str {
    match value {
        SensorType::Temperature => "temperature",
        SensorType::Load => "load",
        SensorType::Memory => "memory",
        SensorType::Frequency => "frequency",
        SensorType::Uptime => "uptime",
        SensorType::FanSpeed => "fan_speed",
        SensorType::FanDuty => "fan_duty",
    }
}

fn parse_visibility(value: &str) -> Option<VisibilityState> {
    match value {
        "visible" => Some(VisibilityState::Visible),
        "hidden" => Some(VisibilityState::Hidden),
        "disabled" => Some(VisibilityState::Disabled),
        _ => None,
    }
}

fn visibility_name(value: &VisibilityState) -> &'static str {
    match value {
        VisibilityState::Visible => "visible",
        VisibilityState::Hidden => "hidden",
        VisibilityState::Disabled => "disabled",
    }
}

pub fn host_policy(stale_after: Duration) -> DataPolicy {
    DataPolicy {
        stale_after,
        min_notify_interval: Duration::from_millis(250),
    }
}

pub fn publish_environment(bus: &DataBus) {
    let mut value = BTreeMap::new();
    value.insert(
        "locale".into(),
        DataValue::String(std::env::var("LANG").unwrap_or_else(|_| "C".into())),
    );
    value.insert(
        "timezone".into(),
        DataValue::String(chrono::Local::now().format("%Z").to_string()),
    );
    value.insert(
        "platform".into(),
        DataValue::String(std::env::consts::OS.into()),
    );
    let _ = bus.publish(
        "host",
        "host.environment",
        DataValue::Map(value),
        host_policy(Duration::from_secs(86_400)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(ms: u64) -> DataPolicy {
        DataPolicy {
            stale_after: Duration::from_millis(ms),
            min_notify_interval: Duration::from_millis(16),
        }
    }

    #[test]
    fn publication_round_trips_and_invalidates() {
        let bus = DataBus::default();
        bus.publish(
            "telemetry",
            "telemetry.current",
            DataValue::Bool(true),
            policy(1000),
        )
        .unwrap();
        assert_eq!(bus.read("telemetry.current").status, SnapshotStatus::Fresh);
        bus.invalidate("telemetry", "telemetry.current", "invalidated")
            .unwrap();
        let got = bus.read("telemetry.current");
        assert_eq!(got.status, SnapshotStatus::Unavailable);
        assert_eq!(got.reason.as_deref(), Some("invalidated"));
    }

    #[test]
    fn lua_schema_rejects_sparse_and_non_finite_values() {
        let lua = Lua::new();
        let sparse: Value = lua.load("return {[1] = true, [3] = false}").eval().unwrap();
        assert!(DataValue::from_lua(sparse).is_err());
        let nan: Value = lua.load("return 0/0").eval().unwrap();
        assert!(DataValue::from_lua(nan).is_err());
    }
}
