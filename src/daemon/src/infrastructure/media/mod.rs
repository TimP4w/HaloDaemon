// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use image::RgbaImage;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Debug, PartialEq, Hash)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub title: String,
    pub artist: String,
    pub status: PlaybackStatus,
    pub art: Option<Arc<RgbaImage>>,
}

/// Application-facing port for publishing media state. Infrastructure owns
/// media acquisition; it does not know which application bus stores results.
pub trait MediaPublisher: Send + Sync {
    fn publish_media(&self, info: &MediaInfo);
    fn invalidate_media(&self);
}

/// Max album-art images kept per platform watcher before evicting the oldest.
pub(crate) const ART_CACHE_CAP: usize = 8;

/// Bounded, insertion-ordered album-art cache shared by both platform watchers.
/// Re-inserting an existing key updates the value without touching its age, so a
/// still-playing track isn't spuriously evicted; the oldest key drops once `cap`
/// is exceeded.
pub(crate) struct ArtCache {
    map: HashMap<String, Arc<RgbaImage>>,
    order: Vec<String>,
    cap: usize,
}

impl ArtCache {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: Vec::new(),
            cap,
        }
    }

    pub(crate) fn get(&self, key: &str) -> Option<Arc<RgbaImage>> {
        self.map.get(key).cloned()
    }

    pub(crate) fn insert(&mut self, key: String, val: Arc<RgbaImage>) {
        if !self.map.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.map.insert(key, val);
        while self.order.len() > self.cap {
            let oldest = self.order.remove(0);
            self.map.remove(&oldest);
        }
    }
}

pub struct MediaHandle {
    latest: RwLock<Option<MediaInfo>>,
    publisher: RwLock<Option<Arc<dyn MediaPublisher>>>,
}

impl MediaHandle {
    pub fn latest(&self) -> Option<MediaInfo> {
        self.latest
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn publish(&self, info: Option<MediaInfo>) {
        if let Some(publisher) = self
            .publisher
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            if let Some(current) = &info {
                publisher.publish_media(current);
            } else {
                publisher.invalidate_media();
            }
        }
        *self.latest.write().unwrap_or_else(|e| e.into_inner()) = info;
    }
}

/// Lazy singleton, same pattern as the audio capture handle: the module holds
/// only a `Weak`, the platform watcher task holds a `Weak` too — when the last
/// consumer `Arc` drops, the watcher's `upgrade()` fails and it exits. A later
/// call starts a fresh watcher.
pub fn shared_with_publisher(publisher: Arc<dyn MediaPublisher>) -> Arc<MediaHandle> {
    static SLOT: OnceLock<Mutex<Weak<MediaHandle>>> = OnceLock::new();
    let slot = SLOT.get_or_init(|| Mutex::new(Weak::new()));
    let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = guard.upgrade() {
        *existing
            .publisher
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(publisher);
        return existing;
    }
    let handle = Arc::new(MediaHandle {
        latest: RwLock::new(None),
        publisher: RwLock::new(Some(publisher)),
    });
    start_platform(Arc::downgrade(&handle));
    *guard = Arc::downgrade(&handle);
    handle
}

/// Dispatches to the platform watcher. Must return immediately — the watcher
/// runs as a spawned tokio task since zbus (Linux) is async and the daemon
/// already runs a multi-thread runtime.
fn start_platform(weak: Weak<MediaHandle>) {
    #[cfg(target_os = "linux")]
    {
        tokio::spawn(linux::run(weak));
    }
    #[cfg(target_os = "windows")]
    {
        // GSMTC is blocking COM (`.get()`), so it runs on a dedicated OS thread
        // rather than a tokio task to avoid stalling the async runtime.
        std::thread::spawn(move || windows::run(weak));
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = weak;
        log::warn!("media watcher unsupported on this platform");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> MediaInfo {
        MediaInfo {
            title: "t".into(),
            artist: "a".into(),
            status: PlaybackStatus::Stopped,
            art: None,
        }
    }

    #[test]
    fn publish_then_latest_round_trips() {
        let handle = MediaHandle {
            latest: RwLock::new(None),
            publisher: RwLock::new(None),
        };
        assert!(handle.latest().is_none());
        let m = info();
        handle.publish(Some(m.clone()));
        let got = handle.latest().unwrap();
        assert_eq!(got.title, m.title);
        assert_eq!(got.status, m.status);
        handle.publish(None);
        assert!(handle.latest().is_none());
    }

    #[tokio::test]
    async fn shared_returns_same_arc_while_held() {
        struct Sink;
        impl MediaPublisher for Sink {
            fn publish_media(&self, _info: &MediaInfo) {}
            fn invalidate_media(&self) {}
        }
        let publisher: Arc<dyn MediaPublisher> = Arc::new(Sink);
        let a = shared_with_publisher(publisher.clone());
        let b = shared_with_publisher(publisher);
        assert!(Arc::ptr_eq(&a, &b));
    }

    fn img() -> Arc<RgbaImage> {
        Arc::new(RgbaImage::new(1, 1))
    }

    #[test]
    fn art_cache_evicts_oldest_past_cap() {
        let mut cache = ArtCache::new(2);
        cache.insert("a".into(), img());
        cache.insert("b".into(), img());
        cache.insert("c".into(), img());
        assert!(cache.get("a").is_none(), "oldest key should be evicted");
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn art_cache_reinsert_updates_without_evicting_or_reordering() {
        let mut cache = ArtCache::new(2);
        let first = img();
        cache.insert("a".into(), first.clone());
        cache.insert("b".into(), img());
        // Re-inserting "a" must update its value but not push a duplicate order
        // entry, so the next insert still evicts "a" only if it is truly oldest.
        let second = img();
        cache.insert("a".into(), second.clone());
        assert!(Arc::ptr_eq(&cache.get("a").unwrap(), &second));
        assert!(cache.get("b").is_some());
        // "a" was inserted first, so it remains the oldest and is evicted next.
        cache.insert("c".into(), img());
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }
}
