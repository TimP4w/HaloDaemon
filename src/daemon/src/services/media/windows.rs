use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use image::RgbaImage;
use windows::core::Interface;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as WinPlaybackStatus,
};
use windows::Storage::Streams::{Buffer, IRandomAccessStream, InputStreamOptions};

use super::{ArtCache, MediaHandle, MediaInfo, PlaybackStatus, ART_CACHE_CAP};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

fn map_status(status: WinPlaybackStatus) -> PlaybackStatus {
    match status {
        WinPlaybackStatus::Playing => PlaybackStatus::Playing,
        WinPlaybackStatus::Paused => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

fn read_thumbnail(stream: IRandomAccessStream) -> Option<RgbaImage> {
    let size = stream.Size().ok()?;
    if size == 0 || size > 8 * 1024 * 1024 {
        return None;
    }
    let buffer = Buffer::Create(size as u32).ok()?;
    let input = stream.GetInputStreamAt(0).ok()?;
    let op = input
        .ReadAsync(&buffer, size as u32, InputStreamOptions::ReadAhead)
        .ok()?;
    let result = op.get().ok()?;
    let len = result.Length().ok()? as usize;
    let reader = windows::Storage::Streams::DataReader::FromBuffer(&result).ok()?;
    let mut bytes = vec![0u8; len];
    reader.ReadBytes(&mut bytes).ok()?;

    let img = image::load_from_memory(&bytes).ok()?;
    const MAX_SIDE: u32 = 240;
    let (w, h) = (img.width(), img.height());
    let scaled = if w.max(h) > MAX_SIDE {
        img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    Some(scaled.to_rgba8())
}

/// Polls the GSMTC session at 1 Hz. GSMTC has no reliable art URL, so art is
/// cached by `title+album` key rather than a URL like the Linux MPRIS path.
///
/// TODO: switch to MediaPropertiesChanged events; polling at 1 Hz is
/// imperceptible for a now-playing widget as a first pass.
pub fn run(weak: Weak<MediaHandle>) {
    let mut art_cache = ArtCache::new(ART_CACHE_CAP);

    loop {
        let Some(handle) = weak.upgrade() else {
            return;
        };

        match poll_once(&mut art_cache) {
            Ok(info) => handle.publish(info),
            Err(e) => {
                log::debug!("[media/windows] GSMTC poll failed: {e:?}");
                handle.publish(None);
            }
        }

        drop(handle);
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn poll_once(art_cache: &mut ArtCache) -> windows::core::Result<Option<MediaInfo>> {
    let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?.get()?;
    let Ok(session) = manager.GetCurrentSession() else {
        return Ok(None);
    };

    let props = session.TryGetMediaPropertiesAsync()?.get()?;
    let title = props.Title().map(|s| s.to_string()).unwrap_or_default();
    let artist = props.Artist().map(|s| s.to_string()).unwrap_or_default();
    let album = props
        .AlbumTitle()
        .map(|s| s.to_string())
        .unwrap_or_default();

    let status = session
        .GetPlaybackInfo()
        .ok()
        .and_then(|pi| pi.PlaybackStatus().ok())
        .map(map_status)
        .unwrap_or(PlaybackStatus::Stopped);

    let (position, length) = session
        .GetTimelineProperties()
        .ok()
        .map(|tp| {
            let position = tp.Position().ok().and_then(|d| {
                let ticks = d.Duration;
                if ticks >= 0 {
                    Some(Duration::from_nanos(ticks as u64 * 100))
                } else {
                    None
                }
            });
            let end = tp.EndTime().ok().and_then(|d| {
                let ticks = d.Duration;
                if ticks > 0 {
                    Some(Duration::from_nanos(ticks as u64 * 100))
                } else {
                    None
                }
            });
            (position, end)
        })
        .unwrap_or((None, None));

    let art_key = format!("{title}\u{1}{album}");
    let art = if let Some(cached) = art_cache.get(&art_key) {
        Some(cached)
    } else if let Ok(thumb_ref) = props.Thumbnail() {
        match thumb_ref.OpenReadAsync() {
            Ok(op) => match op.get() {
                Ok(stream) => {
                    let loaded = match stream.cast::<IRandomAccessStream>() {
                        Ok(stream) => read_thumbnail(stream),
                        Err(_) => None,
                    };
                    loaded.map(|img| {
                        let arc = Arc::new(img);
                        art_cache.insert(art_key.clone(), arc.clone());
                        arc
                    })
                }
                Err(_) => None,
            },
            Err(_) => None,
        }
    } else {
        None
    };

    Ok(Some(MediaInfo {
        title,
        artist,
        album,
        status,
        position,
        position_at: Some(Instant::now()),
        length,
        art,
    }))
}
