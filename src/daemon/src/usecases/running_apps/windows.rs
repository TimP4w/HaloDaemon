use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, CloseHandle, HWND, LPARAM};
use windows::Win32::Graphics::Gdi::{
    BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HDC, HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, EnumWindows, GetIconInfo, GetWindowTextW, GetWindowThreadProcessId, HICON,
    ICONINFO, IsWindowVisible,
};

use halod_protocol::types::RunningApp;

struct AccItem {
    process_name: String,
    display_name: String,
    exe_path: PathBuf,
}

struct Acc(Vec<AccItem>);

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let acc = unsafe { &mut *(lparam.0 as *mut Acc) };
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut title_buf = vec![0u16; 512];
        let title_len = GetWindowTextW(hwnd, &mut title_buf);
        if title_len == 0 {
            return BOOL(1);
        }
        let title = OsString::from_wide(&title_buf[..title_len as usize])
            .to_string_lossy()
            .into_owned();

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return BOOL(1);
        }
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return BOOL(1);
        };
        let mut name_buf = vec![0u16; 1024];
        let mut size = name_buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(name_buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if ok.is_err() || size == 0 {
            return BOOL(1);
        }
        let path_os = OsString::from_wide(&name_buf[..size as usize]);
        let exe_path = PathBuf::from(&path_os);
        let proc_name = exe_path
            .file_name()
            .and_then(|f| f.to_str())
            .map(|s| crate::engines::focus_watcher::normalize_name(s))
            .unwrap_or_default();
        if proc_name.is_empty() {
            return BOOL(1);
        }
        acc.0.push(AccItem {
            process_name: proc_name.clone(),
            display_name: if title.is_empty() { proc_name } else { title },
            exe_path,
        });
    }
    BOOL(1)
}

pub(super) fn build_apps() -> Vec<RunningApp> {
    let mut acc = Acc(Vec::new());
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut acc as *mut Acc as isize),
        );
    }

    // Deduplicate by process_name; prefer shorter display_name (avoid long doc titles).
    let mut seen: BTreeMap<String, AccItem> = BTreeMap::new();
    for item in acc.0 {
        seen.entry(item.process_name.clone())
            .and_modify(|e| {
                if item.display_name.len() < e.display_name.len() {
                    e.display_name = item.display_name.clone();
                }
            })
            .or_insert(item);
    }

    seen.into_values()
        .map(|item| RunningApp {
            icon_name: icon_path_for_exe(&item.exe_path).unwrap_or_default(),
            process_name: item.process_name,
            display_name: item.display_name,
        })
        .collect()
}

fn icon_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("TEMP").or_else(|| std::env::var_os("TMP"))?;
    let dir = PathBuf::from(base).join("halod").join("icons");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

// FNV-1a — stable across process runs, unlike DefaultHasher (random-seeded).
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn icon_path_for_exe(exe_path: &Path) -> Option<String> {
    let cache_dir = icon_cache_dir()?;
    let key_bytes = exe_path.to_string_lossy().to_lowercase();
    let stem = exe_path.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
    let cache_file = cache_dir.join(format!("{stem}-{:016x}.png", fnv1a(key_bytes.as_bytes())));
    if cache_file.exists() {
        return cache_file.to_str().map(|s| s.to_string());
    }
    let png = extract_icon_png(exe_path)?;
    std::fs::write(&cache_file, png).ok()?;
    cache_file.to_str().map(|s| s.to_string())
}

fn extract_icon_png(exe_path: &Path) -> Option<Vec<u8>> {
    let mut wide: Vec<u16> = exe_path.as_os_str().encode_wide().collect();
    wide.push(0);

    unsafe {
        let mut large: HICON = HICON::default();
        let n = ExtractIconExW(
            PCWSTR(wide.as_ptr()),
            0,
            Some(&mut large as *mut HICON),
            None,
            1,
        );
        if n == 0 || large.is_invalid() {
            return None;
        }
        let result = encode_hicon_png(large);
        let _ = DestroyIcon(large);
        result
    }
}

unsafe fn encode_hicon_png(icon: HICON) -> Option<Vec<u8>> {
    let mut info = ICONINFO::default();
    GetIconInfo(icon, &mut info).ok()?;

    let cleanup = |info: &ICONINFO| {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }
    };

    if info.hbmColor.is_invalid() {
        cleanup(&info);
        return None;
    }

    let mut bm = BITMAP::default();
    let written = GetObjectW(
        HGDIOBJ(info.hbmColor.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut _),
    );
    if written == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
        cleanup(&info);
        return None;
    }
    let width = bm.bmWidth;
    let height = bm.bmHeight;

    let mut bi = BITMAPINFO::default();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = width;
    // Negative biHeight = top-down DIB; positive would give us a flipped image.
    bi.bmiHeader.biHeight = -height;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;

    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    let hdc: HDC = GetDC(HWND::default());
    let lines = GetDIBits(
        hdc,
        info.hbmColor,
        0,
        height as u32,
        Some(pixels.as_mut_ptr() as *mut _),
        &mut bi,
        DIB_RGB_COLORS,
    );
    ReleaseDC(HWND::default(), hdc);

    cleanup(&info);

    if lines == 0 {
        return None;
    }

    // GDI gives us BGRA; image crate needs RGBA.
    for chunk in pixels.chunks_exact_mut(4) {
        chunk.swap(0, 2);
    }

    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    image::ImageEncoder::write_image(
        encoder,
        &pixels,
        width as u32,
        height as u32,
        image::ExtendedColorType::Rgba8,
    )
    .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::fnv1a;

    #[test]
    fn fnv1a_empty_input_returns_offset_basis() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn fnv1a_single_byte_matches_known_vector() {
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn fnv1a_multi_byte_matches_known_vector() {
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn fnv1a_differs_for_distinct_inputs() {
        assert_ne!(
            fnv1a(b"c:\\program files\\foo\\app.exe"),
            fnv1a(b"c:\\program files\\bar\\app.exe"),
        );
    }
}
