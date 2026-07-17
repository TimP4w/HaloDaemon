// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mlua::{Lua, Table, Value};

use crate::plugin::manifest::DataProvideDef;
use crate::services::data_bus::{self, DataBus, DataPolicy, DataValue};

#[derive(Clone, Default)]
pub struct DataRuntime {
    pub bus: Arc<DataBus>,
    pub plugin_id: String,
    pub provides: HashMap<String, DataPolicy>,
    pub consumes: Vec<String>,
    _media: Option<Arc<crate::services::media::MediaHandle>>,
}

impl DataRuntime {
    pub fn new(
        bus: Arc<DataBus>,
        plugin_id: String,
        provides: &[DataProvideDef],
        consumes: Vec<String>,
    ) -> Self {
        crate::services::data_bus::publish_environment(&bus);
        let media = consumes
            .iter()
            .any(|key| key == "host.media.playback")
            .then(|| crate::services::media::shared_with_bus(bus.clone()));
        Self {
            bus,
            plugin_id,
            provides: provides
                .iter()
                .map(|item| {
                    (
                        item.key.clone(),
                        DataPolicy {
                            stale_after: Duration::from_millis(item.stale_after_ms),
                            min_notify_interval: Duration::from_millis(item.min_notify_interval_ms),
                        },
                    )
                })
                .collect(),
            consumes,
            _media: media,
        }
    }

    fn can_read(&self, key: &str) -> bool {
        self.consumes.iter().any(|scope| {
            scope == key || (scope == "host.sensors.*" && key.starts_with("host.sensors."))
        })
    }
}

pub fn register(lua: &Lua, runtime: DataRuntime) -> mlua::Result<()> {
    let halod: Table = lua.globals().get("halod")?;
    let publish_runtime = runtime.clone();
    halod.set(
        "publish",
        lua.create_function(move |_, (key, value): (String, Value)| {
            let policy = publish_runtime.provides.get(&key).copied().ok_or_else(|| {
                mlua::Error::RuntimeError(format!("data key '{key}' is not declared in provides"))
            })?;
            let value = DataValue::from_lua(value).map_err(data_bus::mlua_error)?;
            publish_runtime
                .bus
                .publish(&publish_runtime.plugin_id, &key, value, policy)
                .map_err(data_bus::mlua_error)
        })?,
    )?;
    let invalidate_runtime = runtime.clone();
    halod.set(
        "invalidate",
        lua.create_function(move |_, key: String| {
            if !invalidate_runtime.provides.contains_key(&key) {
                return Err(mlua::Error::RuntimeError(format!(
                    "data key '{key}' is not declared in provides"
                )));
            }
            invalidate_runtime
                .bus
                .invalidate(&invalidate_runtime.plugin_id, &key, "invalidated")
                .map_err(data_bus::mlua_error)
        })?,
    )?;
    let read_runtime = runtime;
    halod.set(
        "data",
        lua.create_function(move |lua, key: String| {
            if !read_runtime.can_read(&key) {
                return Err(mlua::Error::RuntimeError(format!(
                    "data key '{key}' is not declared in consumes"
                )));
            }
            data_bus::snapshot_to_lua(lua, &read_runtime.bus.read(&key))
        })?,
    )
}

pub fn add_ctx_method<M, T>(methods: &mut M)
where
    M: mlua::UserDataMethods<T>,
    T: HasDataRuntime,
{
    methods.add_method("data", |lua, this, key: String| {
        let runtime = this.data_runtime();
        if !runtime.can_read(&key) {
            return Err(mlua::Error::RuntimeError(format!(
                "data key '{key}' is not declared in consumes"
            )));
        }
        data_bus::snapshot_to_lua(lua, &runtime.bus.read(&key))
    });
}

pub trait HasDataRuntime {
    fn data_runtime(&self) -> &DataRuntime;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_publish_and_read_share_an_immutable_snapshot() {
        let lua = Lua::new();
        let halod = lua.create_table().unwrap();
        lua.globals().set("halod", halod).unwrap();
        let runtime = DataRuntime::new(
            Arc::new(DataBus::default()),
            "telemetry".into(),
            &[DataProvideDef {
                key: "telemetry.current".into(),
                stale_after_ms: 60_000,
                min_notify_interval_ms: 250,
            }],
            vec!["telemetry.current".into()],
        );
        register(&lua, runtime).unwrap();
        lua.load("halod.publish('telemetry.current', { state = 'ready' })")
            .exec()
            .unwrap();
        let state: String = lua
            .load("local s = halod.data('telemetry.current'); s.value.state = 'changed'; return halod.data('telemetry.current').value.state")
            .eval()
            .unwrap();
        assert_eq!(state, "ready");
        assert!(lua
            .load("return halod.data('other.key')")
            .eval::<Value>()
            .is_err());
    }
}
