// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded, namespaced latest-value snapshots shared by plugin runtimes.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Result};
use mlua::{Lua, Table, Value};

use halod_shared::bus::{
    matches_prefixes, BusEvent, BusEventPayload, BusEventReplay, BusRecord, BusRecordStatus,
    BusSnapshot, BusTransaction, BusValue, EVENT_RING_CAPACITY,
};
use halod_shared::types::{Sensor, SensorType, SensorUnit, VisibilityState};

pub const MAX_PLUGIN_RECORD_BYTES: usize = 32 * 1024;
const MAX_GLOBAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_GLOBAL_RECORDS: usize = 4096;
const MAX_PLUGIN_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 6;
const MAX_MAP_FIELDS: usize = 64;
const MAX_ARRAY_ITEMS: usize = 256;
const MAX_VALUES: usize = 512;
const MAX_STRING_BYTES: usize = 4096;
const MAX_MAP_KEY_BYTES: usize = 64;
const MAX_RECORD_KEY_BYTES: usize = 256;
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

    fn validate(&self) -> Result<usize> {
        let mut count = 0;
        let bytes = validate_value(self, 0, &mut count)?;
        ensure!(
            bytes <= MAX_PLUGIN_RECORD_BYTES,
            "data record exceeds {MAX_PLUGIN_RECORD_BYTES} bytes"
        );
        Ok(bytes)
    }
}

fn validate_value(value: &DataValue, depth: usize, count: &mut usize) -> Result<usize> {
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
        DataValue::Bool(_) => Ok(1),
        DataValue::Integer(_) => Ok(8),
        DataValue::Number(value) => {
            ensure!(
                value.is_finite(),
                "data record contains a non-finite number"
            );
            Ok(8)
        }
        DataValue::String(value) => {
            ensure!(
                value.len() <= MAX_STRING_BYTES,
                "data string exceeds {MAX_STRING_BYTES} bytes"
            );
            Ok(value.len())
        }
        DataValue::Array(values) => {
            ensure!(
                values.len() <= MAX_ARRAY_ITEMS,
                "data array exceeds {MAX_ARRAY_ITEMS} items"
            );
            let mut bytes = values.len() * 2;
            for value in values {
                bytes += validate_value(value, depth + 1, count)?;
            }
            Ok(bytes)
        }
        DataValue::Map(values) => {
            ensure!(
                values.len() <= MAX_MAP_FIELDS,
                "data map exceeds {MAX_MAP_FIELDS} fields"
            );
            let mut bytes = 0;
            for (key, value) in values {
                ensure!(
                    !key.is_empty() && key.len() <= MAX_MAP_KEY_BYTES,
                    "data map key is empty or too long"
                );
                bytes += key.len() + 2 + validate_value(value, depth + 1, count)?;
            }
            Ok(bytes)
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
    pending_notification: bool,
    stale_task: Option<tokio::task::AbortHandle>,
}

struct Inner {
    records: HashMap<String, Record>,
    record_bytes: usize,
    owner_bytes: HashMap<String, usize>,
    state_records: HashMap<String, StateRecord>,
    events: VecDeque<BusEvent>,
    revision: u64,
    next_event_id: u64,
    event_session_id: u64,
}

impl Default for Inner {
    fn default() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self {
            records: HashMap::new(),
            record_bytes: 0,
            owner_bytes: HashMap::new(),
            state_records: HashMap::new(),
            events: VecDeque::new(),
            revision: 0,
            next_event_id: 0,
            event_session_id: now ^ u64::from(std::process::id()),
        }
    }
}

struct StateRecord {
    owner: String,
    value: BusValue,
    revision: u64,
}

pub struct DataBus {
    inner: Arc<Mutex<Inner>>,
    changes: tokio::sync::broadcast::Sender<DataChange>,
    transactions: tokio::sync::broadcast::Sender<BusTransaction>,
    events: tokio::sync::broadcast::Sender<BusEvent>,
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
        let (transactions, _) = tokio::sync::broadcast::channel(256);
        let (events, _) = tokio::sync::broadcast::channel(EVENT_RING_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            changes,
            transactions,
            events,
        }
    }
}

impl DataBus {
    pub fn commit_state(
        &self,
        owner: &str,
        upserts: Vec<(String, BusValue)>,
        tombstones: Vec<String>,
    ) -> Result<BusTransaction> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        for (key, _) in &upserts {
            ensure!(!key.is_empty(), "state topic is empty");
            if let Some(record) = inner.state_records.get(key) {
                ensure!(
                    record.owner == owner,
                    "state topic '{key}' is owned by another producer"
                );
            }
        }
        for key in &tombstones {
            if let Some(record) = inner.state_records.get(key) {
                ensure!(
                    record.owner == owner,
                    "state topic '{key}' is owned by another producer"
                );
            }
        }
        inner.revision = inner.revision.wrapping_add(1).max(1);
        let revision = inner.revision;
        let mut wire_upserts = Vec::with_capacity(upserts.len());
        for (key, value) in upserts {
            inner.state_records.insert(
                key.clone(),
                StateRecord {
                    owner: owner.to_owned(),
                    value: value.clone(),
                    revision,
                },
            );
            wire_upserts.push(BusRecord {
                key,
                value,
                status: BusRecordStatus::Fresh,
                revision,
            });
        }
        for key in &tombstones {
            inner.state_records.remove(key);
        }
        let transaction = BusTransaction {
            revision,
            upserts: wire_upserts,
            tombstones,
        };
        drop(inner);
        let _ = self.transactions.send(transaction.clone());
        Ok(transaction)
    }

    pub fn state_snapshot(&self, prefixes: &[String]) -> BusSnapshot {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let mut records: Vec<_> = inner
            .state_records
            .iter()
            .filter(|(key, _)| matches_prefixes(key, prefixes))
            .map(|(key, record)| BusRecord {
                key: key.clone(),
                value: record.value.clone(),
                status: BusRecordStatus::Fresh,
                revision: record.revision,
            })
            .collect();
        records.sort_by(|left, right| left.key.cmp(&right.key));
        BusSnapshot {
            revision: inner.revision,
            records,
        }
    }

    pub fn state_values<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a str>,
    ) -> HashMap<String, BusValue> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        keys.into_iter()
            .filter_map(|key| {
                inner
                    .state_records
                    .get(key)
                    .map(|record| (key.to_owned(), record.value.clone()))
            })
            .collect()
    }

    pub fn publish_event(&self, payload: BusEventPayload) -> BusEvent {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner.next_event_id = inner.next_event_id.wrapping_add(1).max(1);
        let event = BusEvent {
            id: inner.next_event_id,
            payload,
        };
        inner.events.push_back(event.clone());
        if inner.events.len() > EVENT_RING_CAPACITY {
            inner.events.pop_front();
        }
        drop(inner);
        let _ = self.events.send(event.clone());
        event
    }

    pub fn replay_events(&self, last_event_id: Option<u64>) -> BusEventReplay {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        BusEventReplay {
            session_id: inner.event_session_id,
            oldest_available_id: inner.events.front().map(|event| event.id),
            events: inner
                .events
                .iter()
                .filter(|event| last_event_id.is_none_or(|id| event.id > id))
                .cloned()
                .collect(),
        }
    }

    pub fn subscribe_transactions(&self) -> tokio::sync::broadcast::Receiver<BusTransaction> {
        self.transactions.subscribe()
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<BusEvent> {
        self.events.subscribe()
    }

    pub fn replace_host_sensors(
        &self,
        mut sensors: Vec<(String, Sensor)>,
    ) -> std::collections::HashSet<String> {
        let mut changed_devices = std::collections::HashSet::new();
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
        for (key, _) in self.statuses_for_owner("host") {
            if key.starts_with("host.sensors.")
                && key != "host.sensors.catalog"
                && !expected.contains(&key)
            {
                if let Some(DataValue::Map(old)) = self.read(&key).value {
                    if let Some(device_id) = data_string(&old, "device_id") {
                        changed_devices.insert(device_id.to_owned());
                    }
                }
                self.remove("host", &key);
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
            let next = DataValue::Map(value);
            if self.read(&key).value.as_ref() != Some(&next) {
                changed_devices.insert(device_id.clone());
            }
            let _ = self.publish("host", &key, next, policy);

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
        changed_devices
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
        ensure!(
            !key.is_empty() && key.len() <= MAX_RECORD_KEY_BYTES,
            "data record key is empty or exceeds {MAX_RECORD_KEY_BYTES} bytes"
        );
        let value_bytes = value.validate()?;
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
        ensure!(
            inner.records.contains_key(key) || inner.records.len() < MAX_GLOBAL_RECORDS,
            "host data bus exceeds {MAX_GLOBAL_RECORDS} retained records"
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
                if let Some(task) = record.stale_task.take() {
                    task.abort();
                }
                record.published_at = Some(unix_seconds());
                record.published_instant = Some(now);
                record.policy = policy;
                record.reason = None;
                record.last_publish = Some(now);
                let revision = record.revision;
                let stale_after = policy.stale_after;
                drop(inner);
                self.install_stale_notification(key.to_owned(), revision, now, stale_after);
                return Ok(revision);
            }
        }
        let bytes = retained_bytes(key, owner, value_bytes, 0);
        let existing_bytes = inner.records.get(key).map_or(0, |record| record.bytes);
        let plugin_bytes = inner.owner_bytes.get(owner).copied().unwrap_or(0);
        ensure!(
            owner == "host" || plugin_bytes - existing_bytes + bytes <= MAX_PLUGIN_BYTES,
            "plugin data exceeds retained-byte limit"
        );
        let global_bytes = inner.record_bytes;
        ensure!(
            global_bytes - existing_bytes + bytes <= MAX_GLOBAL_BYTES,
            "host data bus is full"
        );
        inner.revision = inner.revision.wrapping_add(1).max(1);
        let revision = inner.revision;
        let previous_notification = inner
            .records
            .get(key)
            .and_then(|record| record.last_notification);
        let notify = previous_notification
            .is_none_or(|last| now.duration_since(last) >= policy.min_notify_interval);
        let already_pending = inner
            .records
            .get(key)
            .is_some_and(|record| record.pending_notification);
        let schedule_after = (!notify && !already_pending).then(|| {
            policy.min_notify_interval.saturating_sub(
                previous_notification.map_or(Duration::ZERO, |last| now.duration_since(last)),
            )
        });
        if let Some(task) = inner
            .records
            .get_mut(key)
            .and_then(|record| record.stale_task.take())
        {
            task.abort();
        }
        inner.record_bytes = inner.record_bytes - existing_bytes + bytes;
        let owner_bytes = inner.owner_bytes.entry(owner.to_owned()).or_default();
        *owner_bytes = *owner_bytes - existing_bytes + bytes;
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
                last_notification: if notify {
                    Some(now)
                } else {
                    previous_notification
                },
                pending_notification: !notify,
                stale_task: None,
            },
        );
        drop(inner);
        if notify {
            let _ = self.changes.send(DataChange {
                key: key.to_owned(),
                revision,
            });
        } else if let Some(delay) = schedule_after {
            self.schedule_coalesced_notification(key.to_owned(), delay);
        }
        self.install_stale_notification(key.to_owned(), revision, now, policy.stale_after);
        Ok(revision)
    }

    pub fn invalidate(&self, owner: &str, key: &str, reason: &str) -> Result<u64> {
        ensure!(
            !key.is_empty() && key.len() <= MAX_RECORD_KEY_BYTES,
            "data record key is empty or exceeds {MAX_RECORD_KEY_BYTES} bytes"
        );
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = inner.records.get(key) {
            ensure!(
                record.owner == owner,
                "data record '{key}' is owned by another producer"
            );
        }
        ensure!(
            inner.records.contains_key(key) || inner.records.len() < MAX_GLOBAL_RECORDS,
            "host data bus exceeds {MAX_GLOBAL_RECORDS} retained records"
        );
        inner.revision = inner.revision.wrapping_add(1).max(1);
        let revision = inner.revision;
        let policy = inner.records.get(key).map_or(
            DataPolicy {
                stale_after: Duration::from_secs(1),
                min_notify_interval: Duration::from_millis(16),
            },
            |record| record.policy,
        );
        if let Some(task) = inner
            .records
            .get_mut(key)
            .and_then(|record| record.stale_task.take())
        {
            task.abort();
        }
        inner.records.insert(
            key.to_owned(),
            Record {
                owner: owner.to_owned(),
                value: None,
                bytes: retained_bytes(key, owner, 0, reason.len()),
                published_at: None,
                published_instant: None,
                policy,
                revision,
                reason: Some(reason.to_owned()),
                last_publish: None,
                last_notification: Some(Instant::now()),
                pending_notification: false,
                stale_task: None,
            },
        );
        drop(inner);
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
            self.remove(owner, &key);
        }
    }

    fn remove(&self, owner: &str, key: &str) {
        let revision = {
            let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
            if inner
                .records
                .get(key)
                .is_none_or(|record| record.owner != owner)
            {
                return;
            }
            if let Some(record) = inner.records.remove(key) {
                inner.record_bytes -= record.bytes;
                if let Some(bytes) = inner.owner_bytes.get_mut(owner) {
                    *bytes -= record.bytes;
                    if *bytes == 0 {
                        inner.owner_bytes.remove(owner);
                    }
                }
                if let Some(task) = record.stale_task {
                    task.abort();
                }
            }
            inner.revision = inner.revision.wrapping_add(1).max(1);
            inner.revision
        };
        let _ = self.changes.send(DataChange {
            key: key.to_owned(),
            revision,
        });
    }

    fn schedule_coalesced_notification(&self, key: String, delay: Duration) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let changes = self.changes.clone();
        runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            let revision = {
                let mut inner = inner.lock().unwrap_or_else(|error| error.into_inner());
                let Some(record) = inner.records.get_mut(&key) else {
                    return;
                };
                if !record.pending_notification {
                    return;
                }
                record.pending_notification = false;
                record.last_notification = Some(Instant::now());
                record.revision
            };
            let _ = changes.send(DataChange { key, revision });
        });
    }

    fn install_stale_notification(
        &self,
        key: String,
        revision: u64,
        published: Instant,
        delay: Duration,
    ) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = Arc::clone(&self.inner);
        let changes = self.changes.clone();
        let task_key = key.clone();
        let task = runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            let stale_revision = {
                let mut inner = inner.lock().unwrap_or_else(|error| error.into_inner());
                let should_notify = inner.records.get(&task_key).is_some_and(|record| {
                    record.revision == revision
                        && record.value.is_some()
                        && record.published_instant.is_some_and(|published| {
                            published.elapsed() >= record.policy.stale_after
                        })
                });
                if !should_notify {
                    return;
                }
                inner.revision = inner.revision.wrapping_add(1).max(1);
                let stale_revision = inner.revision;
                if let Some(record) = inner.records.get_mut(&task_key) {
                    record.revision = stale_revision;
                    record.stale_task = None;
                }
                stale_revision
            };
            let _ = changes.send(DataChange {
                key: task_key,
                revision: stale_revision,
            });
        });
        let abort = task.abort_handle();
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = inner.records.get_mut(&key) {
            if record.revision == revision && record.published_instant == Some(published) {
                record.stale_task = Some(abort);
                return;
            }
        }
        abort.abort();
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

fn retained_bytes(key: &str, owner: &str, value_bytes: usize, reason_bytes: usize) -> usize {
    key.len()
        .saturating_add(owner.len())
        .saturating_add(value_bytes)
        .saturating_add(reason_bytes)
        .saturating_add(std::mem::size_of::<Record>())
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

    #[test]
    fn common_publish_boundary_rejects_invalid_host_values() {
        let bus = DataBus::default();
        assert!(bus
            .publish(
                "host",
                "host.invalid.number",
                DataValue::Number(f64::NAN),
                policy(1000),
            )
            .is_err());
        assert!(bus
            .publish(
                "host",
                "host.invalid.string",
                DataValue::String("x".repeat(MAX_STRING_BYTES + 1)),
                policy(1000),
            )
            .is_err());
    }

    #[test]
    fn owner_shutdown_reclaims_records_for_a_changed_manifest() {
        let bus = DataBus::default();
        for index in 0..32 {
            bus.publish(
                "telemetry",
                &format!("telemetry.old_{index}"),
                DataValue::Integer(index),
                policy(1000),
            )
            .unwrap();
        }
        bus.invalidate_owner("telemetry");
        assert!(bus.statuses_for_owner("telemetry").is_empty());
        for index in 0..32 {
            bus.publish(
                "telemetry",
                &format!("telemetry.new_{index}"),
                DataValue::Integer(index),
                policy(1000),
            )
            .unwrap();
        }
    }

    #[test]
    fn removed_host_sensor_records_are_reclaimed() {
        let bus = DataBus::default();
        let sensor = Sensor {
            id: "temporary".into(),
            name: "Temporary".into(),
            value: 42.0,
            unit: SensorUnit::Celsius,
            sensor_type: SensorType::Temperature,
            visibility: VisibilityState::Visible,
        };
        let key = sensor_key(&sensor.id);
        bus.replace_host_sensors(vec![("device".into(), sensor)]);
        assert_eq!(bus.read(&key).status, SnapshotStatus::Fresh);
        bus.replace_host_sensors(Vec::new());
        assert_eq!(bus.read(&key).status, SnapshotStatus::Unavailable);
        assert_eq!(bus.statuses_for_owner("host").len(), 1);
    }

    #[tokio::test]
    async fn notifications_are_coalesced_without_resetting_the_throttle() {
        let bus = DataBus::default();
        let mut changes = bus.subscribe();
        let policy = DataPolicy {
            stale_after: Duration::from_secs(1),
            min_notify_interval: Duration::from_millis(80),
        };
        bus.publish(
            "telemetry",
            "telemetry.current",
            DataValue::Integer(1),
            policy,
        )
        .unwrap();
        changes.recv().await.unwrap();
        // Keep Tokio's notification timer deterministic while advancing the
        // std::time::Instant used by the publication rate limit.
        tokio::time::pause();
        std::thread::sleep(Duration::from_millis(20));
        bus.publish(
            "telemetry",
            "telemetry.current",
            DataValue::Integer(2),
            policy,
        )
        .unwrap();
        assert!(matches!(
            changes.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(20));
        let final_revision = bus
            .publish(
                "telemetry",
                "telemetry.current",
                DataValue::Integer(3),
                policy,
            )
            .unwrap();
        assert!(matches!(
            changes.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let change = tokio::time::timeout(Duration::from_millis(100), changes.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change.revision, final_revision);
    }

    #[tokio::test]
    async fn becoming_stale_emits_a_new_revision() {
        let bus = DataBus::default();
        let mut changes = bus.subscribe();
        let initial_revision = bus
            .publish(
                "telemetry",
                "telemetry.current",
                DataValue::Integer(1),
                DataPolicy {
                    stale_after: Duration::from_millis(30),
                    min_notify_interval: Duration::from_millis(16),
                },
            )
            .unwrap();
        changes.recv().await.unwrap();
        let change = tokio::time::timeout(Duration::from_millis(100), changes.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(change.revision > initial_revision);
        assert_eq!(bus.read("telemetry.current").status, SnapshotStatus::Stale);
    }

    #[test]
    fn state_transaction_is_atomic_and_filters_snapshots() {
        let bus = DataBus::default();
        let transaction = bus
            .commit_state(
                "config",
                vec![
                    (
                        halod_shared::bus::topic::GUI.into(),
                        BusValue::Gui(Default::default()),
                    ),
                    (
                        halod_shared::bus::topic::CONFIG_DIR.into(),
                        BusValue::ConfigDir("/tmp/halod".into()),
                    ),
                ],
                Vec::new(),
            )
            .unwrap();
        assert_eq!(transaction.upserts.len(), 2);
        assert!(transaction
            .upserts
            .iter()
            .all(|record| record.revision == transaction.revision));
        let snapshot = bus.state_snapshot(&["config.".into()]);
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].key, halod_shared::bus::topic::GUI);
    }

    #[test]
    fn state_transaction_enforces_ownership_and_tombstones() {
        let bus = DataBus::default();
        let key = halod_shared::bus::topic::CONFIG_DIR.to_owned();
        bus.commit_state(
            "host",
            vec![(key.clone(), BusValue::ConfigDir("one".into()))],
            Vec::new(),
        )
        .unwrap();
        assert!(bus
            .commit_state(
                "other",
                vec![(key.clone(), BusValue::ConfigDir("two".into()))],
                Vec::new(),
            )
            .is_err());
        bus.commit_state("host", Vec::new(), vec![key]).unwrap();
        assert!(bus.state_snapshot(&[]).records.is_empty());
    }

    #[test]
    fn event_ring_is_bounded_and_replays_after_cursor() {
        let bus = DataBus::default();
        for index in 0..EVENT_RING_CAPACITY + 3 {
            bus.publish_event(BusEventPayload::Notification(
                halod_shared::types::Notification {
                    code: halod_shared::types::NotificationCode::Generic {
                        message: index.to_string(),
                    },
                    show_native: false,
                    timestamp_ms: index as u64,
                },
            ));
        }
        let replay = bus.replay_events(None);
        assert_eq!(replay.events.len(), EVENT_RING_CAPACITY);
        let cursor = replay.events[EVENT_RING_CAPACITY - 2].id;
        let tail = bus.replay_events(Some(cursor));
        assert_eq!(tail.events.len(), 1);
    }
}
