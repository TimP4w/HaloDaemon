use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use halod_shared::types::RunningApp;

const WINE_BINARIES: &[&str] = &["wine", "wine64", "wine-preloader", "wine64-preloader"];

struct DesktopEntry {
    id: String,
    name: String,
    icon: String,
    exec_basename: Option<String>,
}

fn process_name_for_pid(pid_path: &Path) -> Option<String> {
    let exe = std::fs::read_link(pid_path.join("exe")).ok()?;
    let basename = exe.file_name()?.to_string_lossy().to_lowercase();
    if WINE_BINARIES.contains(&basename.as_str()) {
        // Drop Wine system processes; keep only ones resolving to a real game exe.
        return wine_exe_name(pid_path);
    }
    if basename.is_empty() {
        return None;
    }
    // Nix wraps binaries as `.name-wrapped`; unwrap to match desktop Exec basenames.
    let name = match basename
        .strip_prefix('.')
        .and_then(|s| s.strip_suffix("-wrapped"))
    {
        Some(inner) => inner.to_owned(),
        None => basename,
    };
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn wine_exe_name(pid_path: &Path) -> Option<String> {
    let raw = std::fs::read(pid_path.join("cmdline")).ok()?;
    let args: Vec<&str> = raw
        .split(|&b| b == 0u8)
        .filter_map(|a| std::str::from_utf8(a).ok())
        .collect();
    // Reject Windows system paths — they're Wine-internal processes, not real apps.
    if let Some(arg) = args.iter().find(|a| {
        let lower = a.to_lowercase();
        lower.ends_with(".exe") && !lower.contains("\\windows\\")
    }) {
        let filename = arg.rsplit('\\').next().unwrap_or(arg);
        let stem = Path::new(filename)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !stem.is_empty() {
            return Some(stem);
        }
    }
    if let Some(arg) = args.iter().find(|a| {
        let lower = a.to_lowercase();
        a.len() >= 3
            && a.chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            && a[1..].starts_with(":\\")
            && !lower.contains("\\windows\\")
    }) {
        let base = arg.rsplit('\\').next()?.to_lowercase();
        if !base.is_empty() {
            return Some(base);
        }
    }
    None
}

fn extract_exec_basename(exec: &str) -> Option<String> {
    let mut tokens = exec.split_whitespace();
    let first = tokens.next()?;
    // Skip "env" wrapper and KEY=VALUE arguments to find the real command.
    let cmd = if first.ends_with("/env") || first == "env" {
        loop {
            let t = tokens.next()?;
            if !t.contains('=') {
                break t;
            }
        }
    } else {
        first
    };
    // Flatpak execs are matched by cgroup/app-id, not exec basename.
    if cmd.ends_with("flatpak") {
        return None;
    }
    let base = Path::new(cmd).file_name()?.to_string_lossy().to_lowercase();
    if base.is_empty() {
        None
    } else {
        Some(base)
    }
}

fn parse_desktop_file(path: &Path) -> Option<DesktopEntry> {
    let content = std::fs::read_to_string(path).ok()?;
    let id = path.file_stem()?.to_string_lossy().into_owned();
    let mut name = String::new();
    let mut icon = String::new();
    let mut exec = String::new();
    let mut in_section = false;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == "[Desktop Entry]";
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            if name.is_empty() {
                name = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("Icon=") {
            if icon.is_empty() {
                icon = v.to_string();
            }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            if exec.is_empty() {
                exec = v.to_string();
            }
        } else if line == "NoDisplay=true" || line == "Hidden=true" {
            return None;
        }
    }

    if name.is_empty() {
        return None;
    }
    Some(DesktopEntry {
        id,
        name,
        icon,
        exec_basename: extract_exec_basename(&exec),
    })
}

fn parse_all_desktop_files() -> Vec<DesktopEntry> {
    let home = std::env::var("HOME").unwrap_or_default();
    let xdg_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    let mut dirs: Vec<PathBuf> = xdg_dirs
        .split(':')
        .map(|d| PathBuf::from(d).join("applications"))
        .collect();
    dirs.push(PathBuf::from(format!("{home}/.local/share/applications")));

    let mut entries = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    for dir in &dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for file in rd.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(e) = parse_desktop_file(&path) {
                if seen_ids.insert(e.id.clone()) {
                    entries.push(e);
                }
            }
        }
    }
    entries
}

fn flatpak_app_id_from_pid(pid_path: &Path) -> Option<String> {
    let cgroup = std::fs::read_to_string(pid_path.join("cgroup")).ok()?;
    for line in cgroup.lines() {
        if let Some(scope) = line.split('/').next_back() {
            if let Some(rest) = scope.strip_prefix("app-") {
                if let Some(stripped) = rest.strip_suffix(".scope") {
                    if let Some(dash) = stripped.rfind('-') {
                        let digits = &stripped[dash + 1..];
                        let raw_id = &stripped[..dash];
                        // Modern Flatpak prefixes the scope with "flatpak-"; strip it.
                        let app_id = raw_id.strip_prefix("flatpak-").unwrap_or(raw_id);
                        if !digits.is_empty()
                            && digits.chars().all(|c| c.is_ascii_digit())
                            && app_id.contains('.')
                        {
                            return Some(app_id.to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

fn steam_info_for_pid(pid_path: &Path) -> Option<(String, String)> {
    let environ = std::fs::read(pid_path.join("environ")).ok()?;
    let app_id_str = environ
        .split(|&b| b == 0u8)
        .filter_map(|kv| std::str::from_utf8(kv).ok())
        .find_map(|kv| kv.strip_prefix("SteamAppId="))?;
    let app_id: u64 = app_id_str.trim().parse().ok()?;
    if app_id == 0 {
        return None;
    }

    let home = std::env::var("HOME").ok()?;
    let steam_root = [
        format!("{home}/.local/share/Steam"),
        format!("{home}/.steam/steam"),
        format!("{home}/.steam/root"),
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|p| p.join("steamapps").exists())?;

    let manifest = std::fs::read_to_string(
        steam_root
            .join("steamapps")
            .join(format!("appmanifest_{app_id}.acf")),
    )
    .ok()?;

    let name = {
        let key = "\"name\"";
        manifest.lines().find_map(|line| {
            let t = line.trim();
            let rest = t.strip_prefix(key)?.trim();
            if rest.starts_with('"') && rest.ends_with('"') && rest.len() >= 2 {
                Some(rest[1..rest.len() - 1].to_string())
            } else {
                None
            }
        })?
    };

    Some((name, format!("steam_icon_{app_id}")))
}

/// `exec_basename -> icon`, parsed once from the installed `.desktop` catalog
/// and memoized for the process lifetime.
fn exec_icon_map() -> &'static HashMap<String, String> {
    use std::sync::OnceLock;
    static MAP: OnceLock<HashMap<String, String>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = HashMap::new();
        for e in parse_all_desktop_files() {
            if e.icon.is_empty() {
                continue;
            }
            if let Some(base) = &e.exec_basename {
                map.entry(base.clone()).or_insert_with(|| e.icon.clone());
            }
        }
        map
    })
}

/// Resolve icons for the given process names from the installed `.desktop`
/// catalog. Processes with no matching desktop entry are omitted.
pub(super) fn resolve_icons(process_names: &[String]) -> HashMap<String, String> {
    let map = exec_icon_map();
    process_names
        .iter()
        .filter_map(|name| map.get(name).map(|icon| (name.clone(), icon.clone())))
        .collect()
}

pub(super) fn build_apps() -> Vec<RunningApp> {
    let mut name_to_pid: BTreeMap<String, PathBuf> = Default::default();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let path = entry.path();
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.chars().all(|c| c.is_ascii_digit()))
                .unwrap_or(false)
            {
                continue;
            }
            if let Some(name) = process_name_for_pid(&path) {
                name_to_pid.entry(name).or_insert(path);
            }
        }
    }

    let desktop_entries = parse_all_desktop_files();
    let mut exec_map: HashMap<String, (String, String)> = HashMap::new();
    let mut id_map: HashMap<String, (String, String)> = HashMap::new();
    for e in &desktop_entries {
        let pair = (e.name.clone(), e.icon.clone());
        id_map.entry(e.id.clone()).or_insert_with(|| pair.clone());
        if let Some(base) = &e.exec_basename {
            exec_map.entry(base.clone()).or_insert(pair);
        }
    }

    name_to_pid
        .into_iter()
        .filter_map(|(proc_name, pid_path)| {
            if let Some((dn, icon)) = exec_map.get(&proc_name) {
                return Some(RunningApp {
                    process_name: proc_name,
                    display_name: dn.clone(),
                    icon_name: icon.clone(),
                });
            }
            // Skip Flatpak sandbox infrastructure — they share the cgroup but never
            // create windows, so picking them would produce rules that never fire.
            const FLATPAK_INFRA: &[&str] = &["bwrap", "xdg-dbus-proxy"];
            if !FLATPAK_INFRA.contains(&proc_name.as_str()) {
                if let Some(app_id) = flatpak_app_id_from_pid(&pid_path) {
                    if let Some((dn, icon)) = id_map.get(&app_id) {
                        return Some(RunningApp {
                            process_name: proc_name,
                            display_name: dn.clone(),
                            icon_name: icon.clone(),
                        });
                    }
                }
            }
            if let Some((dn, icon)) = steam_info_for_pid(&pid_path) {
                return Some(RunningApp {
                    process_name: proc_name,
                    display_name: dn,
                    icon_name: icon,
                });
            }
            None
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_exec_basename_simple_path() {
        assert_eq!(
            extract_exec_basename("/usr/bin/firefox"),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn extract_exec_basename_env_wrapped() {
        assert_eq!(
            extract_exec_basename("env HOME=/home/x firefox"),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn extract_exec_basename_full_path_env_wrapped() {
        assert_eq!(
            extract_exec_basename("/usr/bin/env /usr/bin/app --flag"),
            Some("app".to_string())
        );
    }

    #[test]
    fn extract_exec_basename_flatpak_returns_none() {
        assert_eq!(
            extract_exec_basename("/usr/bin/flatpak run org.gnome.Nautilus"),
            None
        );
    }

    #[test]
    fn extract_exec_basename_key_val_args_skipped() {
        assert_eq!(
            extract_exec_basename("env KEY=VAL ANOTHER=x /usr/bin/myapp"),
            Some("myapp".to_string())
        );
    }

    #[test]
    fn wine_exe_name_returns_stem_for_real_game() {
        let dir = tempfile::tempdir().unwrap();
        // cmdline is NUL-separated argv
        std::fs::write(
            dir.path().join("cmdline"),
            b"wine\0C:\\Games\\MyGame\\game.exe\0\0",
        )
        .unwrap();
        assert_eq!(wine_exe_name(dir.path()), Some("game".to_string()));
    }

    #[test]
    fn wine_exe_name_rejects_windows_system_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cmdline"),
            b"wine\0C:\\Windows\\system32\\services.exe\0\0",
        )
        .unwrap();
        assert_eq!(wine_exe_name(dir.path()), None);
    }

    #[test]
    fn wine_exe_name_returns_none_with_no_exe_arg() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cmdline"), b"wine\0some-loader\0\0").unwrap();
        assert_eq!(wine_exe_name(dir.path()), None);
    }

    #[test]
    fn parse_desktop_file_valid_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("firefox.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nName=Firefox\nIcon=firefox\nExec=/usr/bin/firefox %U\n",
        )
        .unwrap();
        let e = parse_desktop_file(&path).unwrap();
        assert_eq!(e.id, "firefox");
        assert_eq!(e.name, "Firefox");
        assert_eq!(e.icon, "firefox");
        assert_eq!(e.exec_basename, Some("firefox".to_string()));
    }

    #[test]
    fn parse_desktop_file_no_display_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hidden.desktop");
        std::fs::write(&path, "[Desktop Entry]\nName=Hidden\nNoDisplay=true\n").unwrap();
        assert!(parse_desktop_file(&path).is_none());
    }

    #[test]
    fn parse_desktop_file_hidden_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hidden.desktop");
        std::fs::write(&path, "[Desktop Entry]\nName=Hidden\nHidden=true\n").unwrap();
        assert!(parse_desktop_file(&path).is_none());
    }

    #[test]
    fn parse_desktop_file_missing_name_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("noname.desktop");
        std::fs::write(&path, "[Desktop Entry]\nIcon=app\nExec=/usr/bin/app\n").unwrap();
        assert!(parse_desktop_file(&path).is_none());
    }

    #[test]
    fn parse_desktop_file_ignores_non_desktop_entry_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.desktop");
        std::fs::write(
            &path,
            "[Other Section]\nName=ShouldBeIgnored\n[Desktop Entry]\nName=Real App\nIcon=app\n",
        )
        .unwrap();
        let e = parse_desktop_file(&path).unwrap();
        assert_eq!(e.name, "Real App");
    }

    #[test]
    fn flatpak_app_id_from_pid_valid_scope() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cgroup"),
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.gnome.Nautilus-12345.scope\n",
        )
        .unwrap();
        assert_eq!(
            flatpak_app_id_from_pid(dir.path()),
            Some("org.gnome.Nautilus".to_string())
        );
    }

    #[test]
    fn flatpak_app_id_from_pid_non_flatpak_cgroup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cgroup"),
            "0::/user.slice/user-1000.slice/session-1.scope\n",
        )
        .unwrap();
        assert_eq!(flatpak_app_id_from_pid(dir.path()), None);
    }

    #[test]
    fn flatpak_app_id_from_pid_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cgroup"), "").unwrap();
        assert_eq!(flatpak_app_id_from_pid(dir.path()), None);
    }

    #[test]
    fn flatpak_app_id_from_pid_rejects_id_without_dot() {
        let dir = tempfile::tempdir().unwrap();
        // "app-flatpak-nodot-12345.scope" — app_id "nodot" has no dot, must be rejected
        std::fs::write(
            dir.path().join("cgroup"),
            "0::/user.slice/app-flatpak-nodot-12345.scope\n",
        )
        .unwrap();
        assert_eq!(flatpak_app_id_from_pid(dir.path()), None);
    }

    #[test]
    fn process_name_for_pid_regular_name() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr/bin/bash", dir.path().join("exe")).unwrap();
        assert_eq!(process_name_for_pid(dir.path()), Some("bash".to_string()));
    }

    #[test]
    fn process_name_for_pid_unwraps_nix_wrapped_name() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr/bin/.firefox-wrapped", dir.path().join("exe")).unwrap();
        assert_eq!(
            process_name_for_pid(dir.path()),
            Some("firefox".to_string())
        );
    }

    #[test]
    fn process_name_for_pid_wine_binary_without_valid_cmdline_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr/bin/wine", dir.path().join("exe")).unwrap();
        // No cmdline file → wine_exe_name returns None
        assert_eq!(process_name_for_pid(dir.path()), None);
    }

    #[test]
    fn process_name_for_pid_wine_binary_with_valid_cmdline() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("/usr/bin/wine", dir.path().join("exe")).unwrap();
        std::fs::write(
            dir.path().join("cmdline"),
            b"wine\0C:\\Games\\Doom\\doom.exe\0\0",
        )
        .unwrap();
        assert_eq!(process_name_for_pid(dir.path()), Some("doom".to_string()));
    }
}
