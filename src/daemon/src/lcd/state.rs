// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{watch, RwLock};

use halod_shared::types::EffectParamValue;

use crate::lcd::engine::custom::EditorSession;
use crate::lcd::engine::{video::VideoEngine, LcdEngine};
use crate::run_loop::EngineRunConfig;

struct Engine {
    handle: Arc<LcdEngine>,
    video: Arc<VideoEngine>,
    cfg_tx: watch::Sender<EngineRunConfig>,
}

/// The LCD template engine and its companion video engine (video frames feed
/// into the LCD engine's frame sender, so the two are always set together),
/// plus the saved-template name cache the serializer reads.
pub struct LcdEngineState {
    /// Sorted names of saved LCD templates, cached off the broadcast hot path.
    /// Refreshed via `refresh_templates` after a save/delete touches disk.
    pub templates: RwLock<Vec<String>>,
    /// The one live editor preview session, if the editor is open somewhere.
    /// A `std::sync::Mutex` because it's only ever locked synchronously inside
    /// `spawn_blocking` (render) or a quick non-blocking op (invalidate/evict);
    /// never held across an `.await`. Poison is recoverable: a panic cannot
    /// invalidate `EditorSession`'s safe Rust invariants, so callers retain and
    /// repair the inner value rather than taking down the daemon.
    pub editor_session: Mutex<Option<EditorSession>>,
    engine: OnceLock<Engine>,
}

impl LcdEngineState {
    pub fn new() -> Self {
        Self {
            templates: RwLock::new(crate::lcd::usecases::templates::list_templates()),
            editor_session: Mutex::new(None),
            engine: OnceLock::new(),
        }
    }

    pub fn editor_session(&self) -> std::sync::MutexGuard<'_, Option<EditorSession>> {
        self.editor_session.lock().unwrap_or_else(|poisoned| {
            log::warn!("LCD editor-session lock poisoned; recovering state");
            poisoned.into_inner()
        })
    }

    pub fn set_engine(
        &self,
        handle: Arc<LcdEngine>,
        video: Arc<VideoEngine>,
        cfg_tx: watch::Sender<EngineRunConfig>,
    ) {
        let _ = self.engine.set(Engine {
            handle,
            video,
            cfg_tx,
        });
    }

    pub fn engine(&self) -> Option<&Arc<LcdEngine>> {
        self.engine.get().map(|e| &e.handle)
    }

    pub fn video(&self) -> Option<&Arc<VideoEngine>> {
        self.engine.get().map(|e| &e.video)
    }

    pub fn cfg_tx(&self) -> Option<&watch::Sender<EngineRunConfig>> {
        self.engine.get().map(|e| &e.cfg_tx)
    }

    /// Re-read the saved LCD template names from disk into the cache. Call
    /// after a save/delete mutates `lcd/`; the serializer reads the cache
    /// instead of hitting the filesystem on every broadcast.
    pub async fn refresh_templates(&self) {
        let names = crate::lcd::usecases::templates::list_templates();
        *self.templates.write().await = names;
    }

    pub async fn snapshot(
        &self,
        device_templates: HashMap<String, String>,
        device_template_params: HashMap<String, HashMap<String, EffectParamValue>>,
    ) -> halod_shared::types::LcdState {
        halod_shared::types::LcdState {
            engine: LcdEngine::wire_state(device_templates, device_template_params),
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
        device_templates.insert("dev1".to_string(), "tmpl_a".to_string());

        let wire = state.snapshot(device_templates, HashMap::new()).await;

        assert_eq!(wire.templates, vec!["saved_template".to_string()]);
        assert_eq!(
            wire.engine.device_templates.get("dev1").map(String::as_str),
            Some("tmpl_a")
        );
    }

    #[test]
    fn editor_session_recovers_after_poison() {
        let state = Arc::new(LcdEngineState::new());
        let poisoner = Arc::clone(&state);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.editor_session.lock().unwrap();
            panic!("poison editor-session lock for policy test");
        })
        .join();

        assert!(state.editor_session().is_none());
    }
}
