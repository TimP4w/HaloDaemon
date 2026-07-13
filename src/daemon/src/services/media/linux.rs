// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use image::RgbaImage;
use zbus::zvariant::{OwnedValue, Value};
use zbus::{Connection, MatchRule, MessageStream};

use super::{ArtCache, MediaHandle, MediaInfo, PlaybackStatus, ART_CACHE_CAP};

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Pure result of parsing an MPRIS `Metadata` dict. Every field tolerates
/// absence and unexpected wire types (some players are sloppy).
#[derive(Debug, Default, Clone, PartialEq)]
struct ParsedMeta {
    title: String,
    artist: String,
    album: String,
    length: Option<Duration>,
    art_url: Option<String>,
    track_id: Option<String>,
}

fn value_as_str(v: &Value<'_>) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

fn parse_metadata(md: &HashMap<String, OwnedValue>) -> ParsedMeta {
    let title = md
        .get("xesam:title")
        .and_then(|v| value_as_str(v))
        .unwrap_or_default();

    let artist = md
        .get("xesam:artist")
        .and_then(|v| match &**v {
            Value::Array(arr) => {
                let names: Vec<String> = arr.iter().filter_map(value_as_str).collect();
                if names.is_empty() {
                    None
                } else {
                    Some(names.join(", "))
                }
            }
            Value::Str(s) => Some(s.to_string()),
            _ => None,
        })
        .unwrap_or_default();

    let album = md
        .get("xesam:album")
        .and_then(|v| value_as_str(v))
        .unwrap_or_default();

    let length = md.get("mpris:length").and_then(|v| match &**v {
        Value::I64(n) if *n >= 0 => Some(Duration::from_micros(*n as u64)),
        Value::U64(n) => Some(Duration::from_micros(*n)),
        _ => None,
    });

    let art_url = md.get("mpris:artUrl").and_then(|v| value_as_str(v));

    let track_id = md.get("mpris:trackid").and_then(|v| match &**v {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    });

    ParsedMeta {
        title,
        artist,
        album,
        length,
        art_url,
        track_id,
    }
}

fn parse_status(v: Option<&OwnedValue>) -> PlaybackStatus {
    match v.and_then(|v| value_as_str(v)).as_deref() {
        Some("Playing") => PlaybackStatus::Playing,
        Some("Paused") => PlaybackStatus::Paused,
        _ => PlaybackStatus::Stopped,
    }
}

fn parse_position(v: Option<&OwnedValue>) -> Option<Duration> {
    v.and_then(|v| match &**v {
        Value::I64(n) if *n >= 0 => Some(Duration::from_micros(*n as u64)),
        Value::U64(n) => Some(Duration::from_micros(*n)),
        _ => None,
    })
}

/// Decode a `file://` URL's path component (percent-decoded), or `None` for
/// any other scheme.
fn file_url_to_path(url: &str) -> Option<String> {
    let rest = url.strip_prefix("file://")?;
    Some(percent_decode(rest))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Blocking: load art from a `file://` or `http(s)://` URL, downscaled so the
/// longest side is ≤ 240 px (LCD panels are ≤ 240). Any failure → `None`.
fn load_art_from_url(url: &str) -> Option<RgbaImage> {
    let bytes = if let Some(path) = file_url_to_path(url) {
        // Regular files only — no symlinks, fifos, devices.
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.file_type().is_file() {
            return None;
        }
        std::fs::read(&path).ok()?
    } else if url.starts_with("http://") || url.starts_with("https://") {
        fetch_http_bytes(url)?
    } else {
        return None;
    };
    let img = crate::util::image::decode_limited(&bytes).ok()?;
    const MAX_SIDE: u32 = 240;
    let scaled = if img.width().max(img.height()) > MAX_SIDE {
        img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    Some(scaled.to_rgba8())
}

/// Extract `(host, port)` from an `http(s)` URL, or `None` for any other scheme
/// or a malformed authority.
fn http_authority(url: &str) -> Option<(String, u16)> {
    let (rest, default_port) = url
        .strip_prefix("https://")
        .map(|r| (r, 443u16))
        .or_else(|| url.strip_prefix("http://").map(|r| (r, 80u16)))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = if let Some(h) = authority.strip_prefix('[') {
        // [ipv6]:port
        let (h6, tail) = h.split_once(']')?;
        (
            h6.to_string(),
            tail.strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port),
        )
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        (h.to_string(), p.parse().unwrap_or(default_port))
    } else {
        (authority.to_string(), default_port)
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// Split a ureq `host:port` netloc, stripping IPv6 brackets from the host.
fn split_netloc(netloc: &str) -> Option<(String, u16)> {
    let (host, port) = netloc.rsplit_once(':')?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    Some((host.to_string(), port.parse().ok()?))
}

/// Resolve `host:port` and keep only addresses that pass the SSRF policy (same
/// as the plugin TCP transport). Errors if nothing resolves or all are blocked.
fn resolve_vetted(host: &str, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()?
        .filter(|sa| !crate::drivers::plugins::backends::tcp::is_blocked_ip(&sa.ip()))
        .collect();
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "host has no routable address",
        ));
    }
    Ok(addrs)
}

/// True if the URL's host resolves to at least one vetted (non loopback/private/
/// link-local) address. The connection itself is vetted independently by the
/// agent resolver, so this is only a fast pre-check.
fn host_is_vetted(url: &str) -> bool {
    let Some((host, port)) = http_authority(url) else {
        return false;
    };
    resolve_vetted(&host, port).is_ok()
}

/// Blocking http(s) GET with a 4 s timeout, address vetting, no redirects, and a
/// 2 MB size cap (an over-cap body is rejected, not silently truncated).
fn fetch_http_bytes(url: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    const MAX_BYTES: u64 = 2 * 1024 * 1024;
    http_authority(url)?;
    // A custom resolver vets the exact address ureq connects to, so the client
    // can't re-resolve the host to a blocked address after our check (TOCTOU).
    let resp = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(4))
        .redirects(0)
        .resolver(|netloc: &str| match split_netloc(netloc) {
            Some((host, port)) => resolve_vetted(&host, port),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "malformed netloc",
            )),
        })
        .build()
        .get(url)
        .call()
        .ok()?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_BYTES {
        log::debug!("media: art response exceeds {MAX_BYTES} bytes; rejecting");
        return None;
    }
    Some(buf)
}

#[derive(Clone)]
struct PlayerState {
    status: PlaybackStatus,
    meta: ParsedMeta,
    position: Option<Duration>,
    position_at: Instant,
    last_change: Instant,
    art: Option<Arc<RgbaImage>>,
}

/// Selects the active player: prefer any `Playing` player, else the one with
/// the newest `last_change`, else `None`. Pure over the fields that matter.
fn select_player(players: &[(PlaybackStatus, Instant)]) -> Option<usize> {
    if let Some(idx) = players
        .iter()
        .position(|(status, _)| *status == PlaybackStatus::Playing)
    {
        return Some(idx);
    }
    players
        .iter()
        .enumerate()
        .max_by_key(|(_, (_, last_change))| *last_change)
        .map(|(idx, _)| idx)
}

async fn get_player_prop(conn: &Connection, dest: &str, prop: &str) -> Option<OwnedValue> {
    let reply = conn
        .call_method(
            Some(dest),
            "/org/mpris/MediaPlayer2",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &(PLAYER_IFACE, prop),
        )
        .await
        .ok()?;
    reply.body().deserialize::<OwnedValue>().ok()
}

async fn fetch_player_state(conn: &Connection, dest: &str) -> PlayerState {
    let status = parse_status(get_player_prop(conn, dest, "PlaybackStatus").await.as_ref());
    let meta = get_player_prop(conn, dest, "Metadata")
        .await
        .and_then(|v| {
            let md: HashMap<String, OwnedValue> = HashMap::try_from(v).ok()?;
            Some(parse_metadata(&md))
        })
        .unwrap_or_default();
    let position = parse_position(get_player_prop(conn, dest, "Position").await.as_ref());
    PlayerState {
        status,
        meta,
        position,
        position_at: Instant::now(),
        last_change: Instant::now(),
        art: None,
    }
}

fn to_media_info(state: &PlayerState) -> MediaInfo {
    MediaInfo {
        title: state.meta.title.clone(),
        artist: state.meta.artist.clone(),
        album: state.meta.album.clone(),
        status: state.status.clone(),
        position: state.position,
        position_at: Some(state.position_at),
        length: state.meta.length,
        art: state.art.clone(),
    }
}

fn publish_selected(handle: &MediaHandle, players: &HashMap<String, PlayerState>) {
    let names: Vec<&String> = players.keys().collect();
    let sample: Vec<(PlaybackStatus, Instant)> = names
        .iter()
        .map(|n| {
            let p = &players[*n];
            (p.status.clone(), p.last_change)
        })
        .collect();
    match select_player(&sample) {
        Some(idx) => {
            let name = names[idx];
            handle.publish(Some(to_media_info(&players[name])));
        }
        None => handle.publish(None),
    }
}

/// Resolves art for `url` (spawned so it never blocks the event loop),
/// updating the player's cached art and republishing if still selected.
fn spawn_art_resolve(
    weak: Weak<MediaHandle>,
    bus_name: String,
    url: String,
    cache: Arc<tokio::sync::Mutex<ArtCache>>,
    players: Arc<tokio::sync::Mutex<HashMap<String, PlayerState>>>,
) {
    tokio::spawn(async move {
        let Some(handle) = weak.upgrade() else {
            return;
        };
        let art = {
            let mut cache = cache.lock().await;
            if let Some(cached) = cache.get(&url) {
                Some(cached)
            } else {
                let u = url.clone();
                let loaded = tokio::task::spawn_blocking(move || load_art_from_url(&u))
                    .await
                    .ok()
                    .flatten();
                loaded.map(|img| {
                    let arc = Arc::new(img);
                    cache.insert(url.clone(), arc.clone());
                    arc
                })
            }
        };
        let Some(art) = art else { return };
        let mut players = players.lock().await;
        if let Some(state) = players.get_mut(&bus_name) {
            state.art = Some(art);
            publish_selected(&handle, &players);
        }
    });
}

fn build_match_rule(
    interface: &'static str,
    member: &'static str,
) -> zbus::Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(interface)?
        .member(member)?
        .build())
}

pub async fn run(weak: Weak<MediaHandle>) {
    let Some(session) = Connection::session().await.ok() else {
        log::warn!("[media/linux] failed to connect to session bus");
        return;
    };

    let players: Arc<tokio::sync::Mutex<HashMap<String, PlayerState>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let art_cache = Arc::new(tokio::sync::Mutex::new(ArtCache::new(ART_CACHE_CAP)));
    // Maps a unique connection name (`:1.x`) to its well-known MPRIS name, so
    // per-signal sender resolution is an O(1) lookup instead of a bus round-trip.
    let mut owner_cache: HashMap<String, String> = HashMap::new();

    // Seed from whatever MPRIS players are already running.
    if let Ok(reply) = session
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .await
    {
        if let Ok(names) = reply.body().deserialize::<Vec<String>>() {
            let mut guard = players.lock().await;
            for name in names.into_iter().filter(|n| n.starts_with(MPRIS_PREFIX)) {
                let state = fetch_player_state(&session, &name).await;
                if let Some(url) = state.meta.art_url.clone() {
                    spawn_art_resolve(
                        weak.clone(),
                        name.clone(),
                        url,
                        art_cache.clone(),
                        players.clone(),
                    );
                }
                guard.insert(name, state);
            }
        }
    }

    if let Some(handle) = weak.upgrade() {
        let guard = players.lock().await;
        publish_selected(&handle, &guard);
    } else {
        return;
    }

    let Ok(props_rule) = build_match_rule("org.freedesktop.DBus.Properties", "PropertiesChanged")
    else {
        log::warn!("[media/linux] failed to build PropertiesChanged match rule");
        return;
    };
    let Ok(seeked_rule) = build_match_rule(PLAYER_IFACE, "Seeked") else {
        log::warn!("[media/linux] failed to build Seeked match rule");
        return;
    };
    let Ok(name_owner_rule) = build_match_rule("org.freedesktop.DBus", "NameOwnerChanged") else {
        log::warn!("[media/linux] failed to build NameOwnerChanged match rule");
        return;
    };

    let (Ok(mut props_stream), Ok(mut seeked_stream), Ok(mut name_stream)) = (
        MessageStream::for_match_rule(props_rule, &session, Some(32)).await,
        MessageStream::for_match_rule(seeked_rule, &session, Some(32)).await,
        MessageStream::for_match_rule(name_owner_rule, &session, Some(32)).await,
    ) else {
        log::warn!("[media/linux] failed to subscribe to MPRIS signals");
        return;
    };

    loop {
        if weak.upgrade().is_none() {
            break;
        }
        tokio::select! {
            msg = props_stream.next() => {
                let Some(Ok(msg)) = msg else { continue };
                let Some(sender) = msg.header().sender().map(|s| s.to_string()) else { continue };
                if let Ok((iface, changed, _invalidated)) =
                    msg.body().deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
                {
                    if iface != PLAYER_IFACE {
                        continue;
                    }
                    let bus_name = match resolve_owner(&session, &mut owner_cache, &sender).await {
                        Some(n) => n,
                        None => continue,
                    };
                    let mut guard = players.lock().await;
                    let entry = guard.entry(bus_name.clone()).or_insert_with(|| PlayerState {
                        status: PlaybackStatus::Stopped,
                        meta: ParsedMeta::default(),
                        position: None,
                        position_at: Instant::now(),
                        last_change: Instant::now(),
                        art: None,
                    });
                    let mut art_url_changed = None;
                    if let Some(status_val) = changed.get("PlaybackStatus") {
                        entry.status = parse_status(Some(status_val));
                    }
                    if let Some(meta_val) = changed.get("Metadata") {
                        let owned_md = meta_val
                            .try_clone()
                            .ok()
                            .and_then(|v| HashMap::<String, OwnedValue>::try_from(v).ok());
                        if let Some(md) = owned_md {
                            let new_meta = parse_metadata(&md);
                            let track_changed = new_meta.track_id != entry.meta.track_id;
                            if track_changed {
                                entry.position = parse_position(get_player_prop(&session, &bus_name, "Position").await.as_ref());
                                entry.position_at = Instant::now();
                                entry.art = None;
                            }
                            // Re-resolve on track change too, not just URL change:
                            // a same-album track keeps the URL but the art was
                            // just cleared above (the cache makes this cheap).
                            if new_meta.art_url != entry.meta.art_url || track_changed {
                                art_url_changed = new_meta.art_url.clone();
                            }
                            entry.meta = new_meta;
                        }
                    }
                    entry.last_change = Instant::now();
                    if let Some(url) = art_url_changed {
                        spawn_art_resolve(weak.clone(), bus_name.clone(), url, art_cache.clone(), players.clone());
                    }
                    if let Some(h) = weak.upgrade() {
                        publish_selected(&h, &guard);
                    }
                }
            }
            msg = seeked_stream.next() => {
                let Some(Ok(msg)) = msg else { continue };
                let Some(sender) = msg.header().sender().map(|s| s.to_string()) else { continue };
                let bus_name = match resolve_owner(&session, &mut owner_cache, &sender).await {
                    Some(n) => n,
                    None => continue,
                };
                let position_us = msg.body().deserialize::<(i64,)>().ok().map(|(p,)| p);
                let mut guard = players.lock().await;
                if let Some(entry) = guard.get_mut(&bus_name) {
                    entry.position = position_us.and_then(|p| if p >= 0 { Some(Duration::from_micros(p as u64)) } else { None });
                    entry.position_at = Instant::now();
                    entry.last_change = Instant::now();
                    if let Some(h) = weak.upgrade() {
                        publish_selected(&h, &guard);
                    }
                }
            }
            msg = name_stream.next() => {
                let Some(Ok(msg)) = msg else { continue };
                if let Ok((name, old, new)) = msg.body().deserialize::<(String, String, String)>() {
                    if !name.starts_with(MPRIS_PREFIX) {
                        continue;
                    }
                    if !old.is_empty() {
                        owner_cache.remove(&old);
                    }
                    if !new.is_empty() {
                        owner_cache.insert(new.clone(), name.clone());
                    }
                    let mut guard = players.lock().await;
                    if new.is_empty() {
                        guard.remove(&name);
                    } else {
                        let state = fetch_player_state(&session, &name).await;
                        if let Some(url) = state.meta.art_url.clone() {
                            spawn_art_resolve(weak.clone(), name.clone(), url, art_cache.clone(), players.clone());
                        }
                        guard.insert(name, state);
                    }
                    if let Some(h) = weak.upgrade() {
                        publish_selected(&h, &guard);
                    } else {
                        break;
                    }
                }
            }
        }
    }
    log::debug!("[media/linux] watcher stopped (no more consumers)");
}

/// Signal senders are unique connection names (`:1.42`); players are tracked
/// by their well-known `org.mpris.MediaPlayer2.*` name. `cache` maps unique →
/// well-known and is populated incrementally from `NameOwnerChanged`; on a miss
/// we rebuild it once from the bus (`ListNames` + `GetNameOwner`) rather than
/// querying per signal.
async fn resolve_owner(
    conn: &Connection,
    cache: &mut HashMap<String, String>,
    sender: &str,
) -> Option<String> {
    if !sender.starts_with(':') {
        return Some(sender.to_string());
    }
    if let Some(name) = cache.get(sender) {
        return Some(name.clone());
    }
    let reply = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "ListNames",
            &(),
        )
        .await
        .ok()?;
    let names = reply.body().deserialize::<Vec<String>>().ok()?;
    for name in names.into_iter().filter(|n| n.starts_with(MPRIS_PREFIX)) {
        if let Ok(owner_reply) = conn
            .call_method(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "GetNameOwner",
                &(name.as_str(),),
            )
            .await
        {
            if let Ok(owner) = owner_reply.body().deserialize::<String>() {
                cache.insert(owner, name);
            }
        }
    }
    cache.get(sender).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Signature, Str};

    fn str_value(s: &str) -> OwnedValue {
        OwnedValue::try_from(Value::Str(Str::from(s.to_string()))).unwrap()
    }

    #[test]
    fn vetting_rejects_loopback_and_private_and_bad_scheme() {
        assert!(!host_is_vetted("http://127.0.0.1/a.png"));
        assert!(!host_is_vetted("http://localhost:8080/a.png"));
        assert!(!host_is_vetted("https://169.254.169.254/latest"));
        assert!(!host_is_vetted("https://[::1]/a.png"));
        assert!(!host_is_vetted("ftp://example.com/a.png"));
        assert!(!host_is_vetted("http://192.168.1.5/a.png"));
    }

    #[test]
    fn resolver_returns_no_vetted_addrs_for_blocked_hosts() {
        assert!(resolve_vetted("127.0.0.1", 80).is_err());
        assert!(resolve_vetted("192.168.1.5", 443).is_err());
        assert!(resolve_vetted("169.254.169.254", 80).is_err());
        assert!(resolve_vetted("::1", 443).is_err());
    }

    #[test]
    fn split_netloc_strips_ipv6_brackets() {
        assert_eq!(split_netloc("[::1]:443"), Some(("::1".to_string(), 443)));
        assert_eq!(
            split_netloc("1.2.3.4:80"),
            Some(("1.2.3.4".to_string(), 80))
        );
    }

    fn array_value(items: &[&str]) -> OwnedValue {
        let mut arr = Array::new(Signature::from_str_unchecked("s"));
        for item in items {
            arr.append(Value::Str(Str::from(item.to_string()))).unwrap();
        }
        OwnedValue::try_from(Value::Array(arr)).unwrap()
    }

    #[test]
    fn parse_metadata_title_present() {
        let mut md = HashMap::new();
        md.insert("xesam:title".to_string(), str_value("Song"));
        assert_eq!(parse_metadata(&md).title, "Song");
    }

    #[test]
    fn parse_metadata_title_absent_defaults_empty() {
        let md = HashMap::new();
        assert_eq!(parse_metadata(&md).title, "");
    }

    #[test]
    fn parse_metadata_artist_array_joins_with_comma() {
        let mut md = HashMap::new();
        md.insert("xesam:artist".to_string(), array_value(&["A", "B"]));
        assert_eq!(parse_metadata(&md).artist, "A, B");
    }

    #[test]
    fn parse_metadata_artist_string_sloppy_player() {
        let mut md = HashMap::new();
        md.insert("xesam:artist".to_string(), str_value("Solo Artist"));
        assert_eq!(parse_metadata(&md).artist, "Solo Artist");
    }

    #[test]
    fn parse_metadata_artist_absent_defaults_empty() {
        let md = HashMap::new();
        assert_eq!(parse_metadata(&md).artist, "");
    }

    #[test]
    fn parse_metadata_length_i64() {
        let mut md = HashMap::new();
        md.insert(
            "mpris:length".to_string(),
            OwnedValue::try_from(Value::I64(2_000_000)).unwrap(),
        );
        assert_eq!(parse_metadata(&md).length, Some(Duration::from_secs(2)));
    }

    #[test]
    fn parse_metadata_length_u64() {
        let mut md = HashMap::new();
        md.insert(
            "mpris:length".to_string(),
            OwnedValue::try_from(Value::U64(3_000_000)).unwrap(),
        );
        assert_eq!(parse_metadata(&md).length, Some(Duration::from_secs(3)));
    }

    #[test]
    fn parse_metadata_art_url_file_with_percent_encoding() {
        let mut md = HashMap::new();
        md.insert(
            "mpris:artUrl".to_string(),
            str_value("file:///home/user/My%20Music/cover.jpg"),
        );
        let meta = parse_metadata(&md);
        assert_eq!(
            meta.art_url.as_deref(),
            Some("file:///home/user/My%20Music/cover.jpg")
        );
        assert_eq!(
            file_url_to_path(&meta.art_url.unwrap()),
            Some("/home/user/My Music/cover.jpg".to_string())
        );
    }

    #[test]
    fn file_url_to_path_rejects_non_file_scheme() {
        assert_eq!(file_url_to_path("https://example.com/art.jpg"), None);
    }

    #[test]
    fn percent_decode_handles_plain_and_encoded() {
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("trailing%"), "trailing%");
    }

    #[test]
    fn select_player_prefers_playing() {
        let now = Instant::now();
        let players = vec![
            (PlaybackStatus::Paused, now),
            (PlaybackStatus::Playing, now - Duration::from_secs(10)),
        ];
        assert_eq!(select_player(&players), Some(1));
    }

    #[test]
    fn select_player_falls_back_to_newest_change() {
        let now = Instant::now();
        let players = vec![
            (PlaybackStatus::Stopped, now - Duration::from_secs(5)),
            (PlaybackStatus::Paused, now),
        ];
        assert_eq!(select_player(&players), Some(1));
    }

    #[test]
    fn select_player_none_when_empty() {
        let players: Vec<(PlaybackStatus, Instant)> = vec![];
        assert_eq!(select_player(&players), None);
    }
}
