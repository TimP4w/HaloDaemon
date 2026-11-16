// SPDX-License-Identifier: GPL-3.0-or-later
//! Process icon lookup (absolute path or XDG theme name), decoded off the UI thread.

use std::collections::HashMap;

/// State of a background icon load. `Failed` is terminal so a missing icon is
/// not re-resolved every frame.
pub enum IconState {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

pub type IconCache = std::sync::Arc<std::sync::Mutex<HashMap<String, IconState>>>;

// ── Icon loading ─────────────────────────────────────────────────────────────

/// Resolve an icon identifier (theme name or absolute path) to an image file.
/// Returns `None` if the icon cannot be found.
fn resolve_icon_path(name: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_owned());
    }
    #[cfg(unix)]
    return find_xdg_icon(name);
    #[cfg(not(unix))]
    None
}

/// Parse the pixel size from a hicolor theme directory name
/// ("48x48" -> 48, "scalable" -> 0).
#[cfg(unix)]
fn parse_icon_dir_size(dir_name: &str) -> u32 {
    dir_name
        .split('x')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Accumulates the best icon candidate across the filesystem walk, applying the
/// raster > svg > pixmap precedence (larger raster wins).
#[cfg(unix)]
#[derive(Default)]
struct IconRanker {
    best_raster: Option<(u32, std::path::PathBuf)>,
    svg: Option<std::path::PathBuf>,
    pixmap: Option<std::path::PathBuf>,
}

#[cfg(unix)]
impl IconRanker {
    /// Offer a raster candidate of the given pixel size; keeps it if it is the
    /// largest seen so far.
    fn offer_raster(&mut self, size: u32, path: std::path::PathBuf) {
        if self.best_raster.as_ref().is_none_or(|(s, _)| size > *s) {
            self.best_raster = Some((size, path));
        }
    }

    /// Offer an SVG candidate; the first one seen is kept.
    fn offer_svg(&mut self, path: std::path::PathBuf) {
        if self.svg.is_none() {
            self.svg = Some(path);
        }
    }

    /// Offer a pixmaps-dir candidate; the first one seen is kept.
    fn offer_pixmap(&mut self, path: std::path::PathBuf) {
        if self.pixmap.is_none() {
            self.pixmap = Some(path);
        }
    }

    /// Resolve to the winning path: largest raster, else svg, else pixmap.
    fn winner(self) -> Option<std::path::PathBuf> {
        self.best_raster
            .map(|(_, p)| p)
            .or(self.svg)
            .or(self.pixmap)
    }
}

/// Data directories to search for icons, user-level first. `XDG_DATA_HOME` is
/// separate from `XDG_DATA_DIRS` and is where Steam writes per-game icons.
#[cfg(unix)]
fn icon_data_dirs() -> Vec<String> {
    icon_data_dirs_from(
        std::env::var("XDG_DATA_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
        std::env::var("XDG_DATA_DIRS").ok().as_deref(),
    )
}

#[cfg(unix)]
fn icon_data_dirs_from(
    data_home: Option<&str>,
    home: Option<&str>,
    data_dirs: Option<&str>,
) -> Vec<String> {
    let mut dirs = Vec::new();
    match data_home.filter(|d| !d.is_empty()) {
        Some(d) => dirs.push(d.to_string()),
        None => {
            if let Some(home) = home.filter(|h| !h.is_empty()) {
                dirs.push(format!("{home}/.local/share"));
            }
        }
    }
    dirs.extend(
        data_dirs
            .filter(|d| !d.is_empty())
            .unwrap_or("/usr/local/share:/usr/share")
            .split(':')
            .filter(|d| !d.is_empty())
            .map(String::from),
    );
    dirs
}

/// Walk the icon data directories for `name`, returning the best match.
/// Prefers the largest raster (PNG/JPEG) so it stays crisp; falls back to a
/// scalable SVG (rasterized at load time) and finally the pixmaps directory.
#[cfg(unix)]
fn find_xdg_icon(name: &str) -> Option<std::path::PathBuf> {
    let mut ranker = IconRanker::default();

    for base in icon_data_dirs() {
        let hicolor = std::path::PathBuf::from(&base).join("icons/hicolor");
        if let Ok(entries) = std::fs::read_dir(&hicolor) {
            for entry in entries.flatten() {
                let apps_dir = entry.path().join("apps");
                for ext in ["png", "jpg", "jpeg"] {
                    let candidate = apps_dir.join(format!("{name}.{ext}"));
                    if candidate.exists() {
                        let size = parse_icon_dir_size(&entry.file_name().to_string_lossy());
                        ranker.offer_raster(size, candidate);
                    }
                }
                let candidate = apps_dir.join(format!("{name}.svg"));
                if candidate.exists() {
                    ranker.offer_svg(candidate);
                }
            }
        }
        for ext in ["png", "jpg", "jpeg", "svg"] {
            let p = std::path::PathBuf::from(&base)
                .join("pixmaps")
                .join(format!("{name}.{ext}"));
            if p.exists() {
                ranker.offer_pixmap(p);
                break;
            }
        }
    }
    ranker.winner()
}

/// Rasterize an SVG file to RGBA at roughly `target` pixels on the long edge.
fn rasterize_svg(path: &std::path::Path, target: u32) -> Option<egui::ColorImage> {
    let data = std::fs::read(path).ok()?;
    crate::svg::color_image(&data, target)
}

/// Texture size icons are normalized to. Larger than the on-screen render size
/// (16–32px) so the final linear downscale stays crisp on HiDPI.
const ICON_PX: u32 = 64;

/// Load an icon into an egui texture. Handles both raster and SVG sources.
/// Raster sources are downscaled with a high-quality filter so a large source
/// (e.g. a 1024² PNG) doesn't alias when drawn at 16–32px.
fn load_icon_texture(ctx: &egui::Context, name: &str) -> Option<egui::TextureHandle> {
    let path = resolve_icon_path(name)?;
    let color_image = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"))
    {
        rasterize_svg(&path, ICON_PX)?
    } else {
        let mut img = image::open(&path).ok()?;
        if img.width().max(img.height()) > ICON_PX {
            img = img.resize(ICON_PX, ICON_PX, image::imageops::FilterType::Lanczos3);
        }
        let rgba = img.into_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        egui::ColorImage::from_rgba_unmultiplied(size, &rgba.into_raw())
    };
    Some(ctx.load_texture(name, color_image, egui::TextureOptions::LINEAR))
}

/// Get a ready icon texture, kicking off a background load on first request.
/// Returns `None` while loading, on failure, or for an unknown icon — the call
/// never blocks the UI thread on filesystem or decode work.
pub fn get_icon(
    cache: &IconCache,
    ctx: &egui::Context,
    icon_name: &str,
) -> Option<egui::TextureHandle> {
    if icon_name.is_empty() {
        return None;
    }
    let mut map = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(state) = map.get(icon_name) {
        return match state {
            IconState::Ready(tex) => Some(tex.clone()),
            IconState::Loading | IconState::Failed => None,
        };
    }
    map.insert(icon_name.to_string(), IconState::Loading);
    drop(map);
    spawn_icon_load(cache.clone(), ctx.clone(), icon_name.to_string());
    None
}

/// Resolve and decode an icon off the UI thread, then store the result and
/// request a repaint so the loaded texture appears on the next frame.
fn spawn_icon_load(cache: IconCache, ctx: egui::Context, name: String) {
    std::thread::spawn(move || {
        let state = match load_icon_texture(&ctx, &name) {
            Some(tex) => IconState::Ready(tex),
            None => IconState::Failed,
        };
        if let Ok(mut map) = cache.lock() {
            map.insert(name, state);
        }
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn icon_data_dirs_searches_data_home_before_system_dirs() {
        assert_eq!(
            icon_data_dirs_from(None, Some("/home/u"), Some("/usr/share")),
            vec!["/home/u/.local/share".to_string(), "/usr/share".to_string()]
        );
        assert_eq!(
            icon_data_dirs_from(Some("/xdg/data"), Some("/home/u"), Some("/usr/share")),
            vec!["/xdg/data".to_string(), "/usr/share".to_string()]
        );
        assert_eq!(
            icon_data_dirs_from(None, None, Some("")),
            vec!["/usr/local/share".to_string(), "/usr/share".to_string()]
        );
    }

    #[test]
    fn get_icon_never_blocks_and_reflects_cache_state() {
        let ctx = egui::Context::default();
        let cache: IconCache = Default::default();

        // Empty name resolves to nothing and registers no load.
        assert!(get_icon(&cache, &ctx, "").is_none());
        assert!(cache.lock().unwrap().is_empty());

        // Pending and failed entries yield None without spawning a load.
        cache.lock().unwrap().insert("a".into(), IconState::Loading);
        cache.lock().unwrap().insert("b".into(), IconState::Failed);
        assert!(get_icon(&cache, &ctx, "a").is_none());
        assert!(get_icon(&cache, &ctx, "b").is_none());

        // A ready entry yields its texture.
        let tex = ctx.load_texture("t", egui::ColorImage::example(), Default::default());
        cache
            .lock()
            .unwrap()
            .insert("c".into(), IconState::Ready(tex.clone()));
        assert_eq!(get_icon(&cache, &ctx, "c").map(|t| t.id()), Some(tex.id()));
    }

    #[test]
    fn get_icon_first_request_registers_a_background_load() {
        let ctx = egui::Context::default();
        let cache: IconCache = Default::default();
        assert!(get_icon(&cache, &ctx, "halod-no-such-icon-zzz").is_none());
        assert!(cache.lock().unwrap().contains_key("halod-no-such-icon-zzz"));
    }

    #[cfg(unix)]
    #[test]
    fn parse_icon_dir_size_extracts_dimension() {
        assert_eq!(parse_icon_dir_size("48x48"), 48);
        assert_eq!(parse_icon_dir_size("256x256"), 256);
        assert_eq!(parse_icon_dir_size("scalable"), 0);
        assert_eq!(parse_icon_dir_size(""), 0);
    }

    #[cfg(unix)]
    #[test]
    fn icon_ranker_precedence() {
        use std::path::PathBuf;

        // Raster always wins over svg and pixmap.
        let mut r = IconRanker::default();
        r.offer_svg(PathBuf::from("a.svg"));
        r.offer_pixmap(PathBuf::from("a.png"));
        r.offer_raster(48, PathBuf::from("48.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("48.png")));

        // Largest raster wins regardless of offer order.
        let mut r = IconRanker::default();
        r.offer_raster(48, PathBuf::from("48.png"));
        r.offer_raster(256, PathBuf::from("256.png"));
        r.offer_raster(32, PathBuf::from("32.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("256.png")));

        // Svg wins over pixmap when no raster present; first svg kept.
        let mut r = IconRanker::default();
        r.offer_svg(PathBuf::from("first.svg"));
        r.offer_svg(PathBuf::from("second.svg"));
        r.offer_pixmap(PathBuf::from("p.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("first.svg")));

        // Pixmap is the last resort; first pixmap kept.
        let mut r = IconRanker::default();
        r.offer_pixmap(PathBuf::from("first.png"));
        r.offer_pixmap(PathBuf::from("second.png"));
        assert_eq!(r.winner(), Some(PathBuf::from("first.png")));

        // Nothing offered yields None.
        assert_eq!(IconRanker::default().winner(), None);
    }
}
