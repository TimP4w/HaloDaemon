// SPDX-License-Identifier: GPL-3.0-or-later
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

use halod_shared::types::EffectParamValue;

use crate::domain::lcd::engine::custom::EditorSession;
use crate::domain::lcd::engine::{video::VideoEngine, LcdEngine};
use crate::domain::plugin::Registry;
use halod_shared::ids::DeviceId;

struct Engine {
    handle: Arc<LcdEngine>,
    video: Arc<VideoEngine>,
}

/// The LCD template engine and its companion video engine (video frames feed
/// into the LCD engine's frame sender, so the two are always set together),
/// plus the saved-template name cache the serializer reads.
pub struct LcdEngineState {
    /// Sorted names of saved LCD templates, cached off the broadcast hot path.
    /// Refreshed via `refresh_templates` after a save/delete touches disk.
    pub templates: RwLock<Vec<String>>,
    pub editor_session: Mutex<Option<EditorSession>>,
    pub editor_rendering: std::sync::atomic::AtomicBool,
    editor_generation: std::sync::atomic::AtomicU64,
    engine: OnceLock<Engine>,
}

impl LcdEngineState {
    pub fn new() -> Self {
        Self {
            templates: RwLock::new(crate::application::usecases::lcd::templates::list_templates()),
            editor_session: Mutex::new(None),
            editor_rendering: std::sync::atomic::AtomicBool::new(false),
            editor_generation: std::sync::atomic::AtomicU64::new(0),
            engine: OnceLock::new(),
        }
    }

    pub fn editor_generation(&self) -> u64 {
        self.editor_generation
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn invalidate_editor_session(&self) {
        self.editor_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    pub fn set_engine(&self, handle: Arc<LcdEngine>, video: Arc<VideoEngine>) {
        let _ = self.engine.set(Engine { handle, video });
    }

    pub fn engine(&self) -> Option<&Arc<LcdEngine>> {
        self.engine.get().map(|e| &e.handle)
    }

    pub fn video(&self) -> Option<&Arc<VideoEngine>> {
        self.engine.get().map(|e| &e.video)
    }

    /// Re-read the saved LCD template names from disk into the cache. Call
    /// after a save/delete mutates `lcd/`; the serializer reads the cache
    /// instead of hitting the filesystem on every broadcast.
    pub async fn refresh_templates(&self) {
        let names = crate::application::usecases::lcd::templates::list_templates();
        *self.templates.write().await = names;
    }

    pub async fn snapshot(
        &self,
        registry: &Registry,
        device_templates: HashMap<DeviceId, String>,
        device_template_params: HashMap<DeviceId, HashMap<String, EffectParamValue>>,
    ) -> halod_shared::types::LcdState {
        halod_shared::types::LcdState {
            engine: LcdEngine::wire_state(registry, device_templates, device_template_params),
            templates: self.templates.read().await.clone(),
            // Overwritten by the serializer from the persisted config.
            config: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_carries_templates_cache_and_device_maps() {
        let state = LcdEngineState::new();
        *state.templates.write().await = vec!["saved_template".to_string()];

        let mut device_templates = HashMap::new();
        device_templates.insert("dev1".parse().unwrap(), "tmpl_a".to_string());

        let registry = crate::domain::plugin::Registry::default();
        let wire = state
            .snapshot(&registry, device_templates, HashMap::new())
            .await;

        assert_eq!(wire.templates, vec!["saved_template".to_string()]);
        assert_eq!(
            wire.engine.device_templates.get("dev1").map(String::as_str),
            Some("tmpl_a")
        );
    }

    #[test]
    fn invalidating_editor_session_changes_generation() {
        let state = LcdEngineState::new();
        let before = state.editor_generation();
        state.invalidate_editor_session();
        assert_ne!(state.editor_generation(), before);
    }
}
