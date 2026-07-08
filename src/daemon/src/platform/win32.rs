/// Encode a string as a NUL-terminated UTF-16 buffer for the Win32 `*W` APIs.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Encode a `Path` as a NUL-terminated UTF-16 buffer using the native OS
/// encoding, avoiding the lossy UTF-8 round-trip that `to_string_lossy` incurs
/// on paths containing characters that are valid UTF-16 but not UTF-8.
pub(crate) fn wide_path(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
