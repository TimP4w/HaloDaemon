// SPDX-License-Identifier: GPL-3.0-or-later
//! The `dev.audio` object: lets a device plugin create virtual audio sinks
//! (a PulseAudio/PipeWire null-sink looped into the device's physical sink) and
//! drive their volume — the plugin-facing form of the native ChatMix routing.
//!
//! Sinks are **host-owned**: the worker tears every one down on `close`, and a
//! stale daemon's leftovers are reclaimed at startup (`cleanup_orphaned_sinks`),
//! so a plugin can't leak them. Creation is scoped to the device's *own* USB id,
//! so a plugin can never open sinks for hardware it doesn't drive.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mlua::{AnyUserData, Lua, UserData, UserDataMethods};
use tokio::runtime::Handle;

use crate::services::audio::sink::{register_sink, Sink};

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

/// Per-worker registry of every sink the plugin created, drained on teardown.
/// `Rc<RefCell>` because the plugin VM (and this) never leaves its worker thread.
pub type SinkRegistry = Rc<RefCell<Vec<Arc<ManagedSink>>>>;

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
            if this.registry.borrow().len() >= MAX_SINKS {
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
                    this.registry.borrow_mut().push(managed.clone());
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
            this.registry
                .borrow_mut()
                .retain(|s| !Arc::ptr_eq(s, &this.sink));
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

/// Remove every sink the worker created — called when the device closes.
pub fn teardown(registry: &SinkRegistry, handle: &Handle) {
    for sink in registry.borrow().iter() {
        handle.block_on(sink.remove_once());
    }
}
