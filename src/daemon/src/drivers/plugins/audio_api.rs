// SPDX-License-Identifier: GPL-3.0-or-later
//! The `dev.audio` object: lets a device plugin create virtual audio sinks
//! (a PulseAudio/PipeWire null-sink looped into the device's physical sink) and
//! drive their volume — the plugin-facing form of the native ChatMix routing.
//!
//! Sinks are **host-owned**: the worker tears every one down on `close`, and a
//! stale daemon's leftovers are reclaimed at startup (`cleanup_orphaned_sinks`),
//! so a plugin can't leak them. Creation is scoped to the device's *own* USB id,
//! so a plugin can never open sinks for hardware it doesn't drive.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use mlua::{AnyUserData, Lua, UserData, UserDataMethods};
use tokio::runtime::Handle;

use crate::services::audio::sink::{register_sink, Sink};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A host-created sink plus a one-shot removal guard, so an explicit `:remove()`
/// and the worker's teardown never double-unload the backing pactl modules.
pub struct ManagedSink {
    sink: Sink,
    removed: AtomicBool,
}

impl ManagedSink {
    async fn set_volume(&self, pct: u8) {
        self.sink.set_volume(pct).await;
    }

    async fn remove_once(&self) {
        if !self.removed.swap(true, Ordering::Relaxed) {
            self.sink.remove().await;
        }
    }
}

/// Registry of every sink a plugin created. `Arc<Mutex>` (Send + Sync) so the
/// owning device can tear sinks down without going through the Lua worker, which
/// may be wedged/dead by the time cleanup is needed.
pub type SinkRegistry = Arc<Mutex<Vec<Arc<ManagedSink>>>>;

/// Max sinks one plugin worker may hold open at once.
const MAX_SINKS: usize = 4;
/// Max length of a sink display name.
const MAX_SINK_NAME: usize = 64;

/// `dev.audio`: creates virtual sinks scoped to this device's own USB id.
pub struct AudioApi {
    vid: Option<u16>,
    pid: Option<u16>,
    handle: Handle,
    registry: SinkRegistry,
}

impl UserData for AudioApi {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // register(name) -> sink handle, or nil when the device has no physical
        // sink or the OS can't create one (e.g. Windows).
        methods.add_method("register", |_, this, name: String| {
            let (Some(vid), Some(pid)) = (this.vid, this.pid) else {
                return Err(mlua::Error::RuntimeError(
                    "dev.audio requires a USB (HID) device".into(),
                ));
            };
            if name.is_empty() || name.len() > MAX_SINK_NAME {
                return Err(mlua::Error::RuntimeError(format!(
                    "sink name must be 1..={MAX_SINK_NAME} bytes"
                )));
            }
            if lock(&this.registry).len() >= MAX_SINKS {
                return Err(mlua::Error::RuntimeError(format!(
                    "at most {MAX_SINKS} sinks per plugin"
                )));
            }
            match this.handle.block_on(register_sink(vid, pid, &name)) {
                Some(sink) => {
                    let managed = Arc::new(ManagedSink {
                        sink,
                        removed: AtomicBool::new(false),
                    });
                    lock(&this.registry).push(managed.clone());
                    Ok(Some(SinkHandle {
                        sink: managed,
                        handle: this.handle.clone(),
                        registry: this.registry.clone(),
                    }))
                }
                None => Ok(None),
            }
        });
    }
}

/// A single sink handle returned by `dev.audio:register`.
pub struct SinkHandle {
    sink: Arc<ManagedSink>,
    handle: Handle,
    registry: SinkRegistry,
}

impl UserData for SinkHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("set_volume", |_, this, pct: u8| {
            this.handle.block_on(this.sink.set_volume(pct));
            Ok(())
        });
        methods.add_method("remove", |_, this, ()| {
            this.handle.block_on(this.sink.remove_once());
            lock(&this.registry).retain(|s| !Arc::ptr_eq(s, &this.sink));
            Ok(())
        });
    }
}

/// Build the `dev.audio` userdata for a device worker.
pub fn build(
    lua: &Lua,
    vid: Option<u16>,
    pid: Option<u16>,
    handle: Handle,
    registry: SinkRegistry,
) -> mlua::Result<AnyUserData> {
    lua.create_userdata(AudioApi {
        vid,
        pid,
        handle,
        registry,
    })
}

/// Remove every sink still in the registry, awaiting each teardown. Runs on the
/// owning device (Send side), so it works even when the Lua worker is dead.
pub async fn drain_and_remove(registry: &SinkRegistry) {
    let sinks: Vec<Arc<ManagedSink>> = std::mem::take(&mut *lock(registry));
    for sink in sinks {
        sink.remove_once().await;
    }
}

/// Owns the sink registry on the device side and guarantees teardown on drop,
/// covering exit paths where the graceful `close` never runs (timeout,
/// quarantine, panic). A leftover module is also reclaimed at next startup by
/// `cleanup_orphaned_sinks`.
pub struct AudioGuard {
    registry: SinkRegistry,
    handle: Handle,
}

impl AudioGuard {
    pub fn new(registry: SinkRegistry, handle: Handle) -> Self {
        Self { registry, handle }
    }
}

impl Drop for AudioGuard {
    fn drop(&mut self) {
        let sinks: Vec<Arc<ManagedSink>> = std::mem::take(&mut *lock(&self.registry));
        if sinks.is_empty() {
            return;
        }
        let handle = Handle::try_current().unwrap_or_else(|_| self.handle.clone());
        handle.spawn(async move {
            for sink in sinks {
                sink.remove_once().await;
            }
        });
    }
}
