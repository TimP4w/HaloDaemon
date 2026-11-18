// SPDX-License-Identifier: GPL-3.0-or-later
//! Win32 string encoding, `CommandLineToArgvW` quoting and UAC launching.

const ERROR_CANCELLED: i32 = 1223;

pub fn is_declined(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_CANCELLED)
}

/// Quotes one argument by the `CommandLineToArgvW` rules.
pub fn quote_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty() || arg.chars().any(|c| c == ' ' || c == '\t' || c == '"');
    if !needs_quoting {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes: usize = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2));
                out.push_str("\\\"");
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(windows)]
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
pub fn wide_path(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Starts `file` through a UAC prompt; a declined prompt satisfies [`is_declined`].
#[cfg(windows)]
pub fn run_elevated(
    file: &std::path::Path,
    parameters: &str,
    dir: &std::path::Path,
) -> std::io::Result<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb = wide("runas");
    let file = wide_path(file);
    let dir = wide_path(dir);
    let parameters = wide(parameters);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR(dir.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` and every buffer it points at outlive the call.
    unsafe { ShellExecuteExW(&mut info) }.map_err(win32_error)?;
    if info.hProcess.is_invalid() {
        return Err(std::io::Error::other("no process handle returned"));
    }
    // SAFETY: SEE_MASK_NOCLOSEPROCESS hands ownership of hProcess to the caller.
    Ok(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(info.hProcess.0) })
}

#[cfg(windows)]
fn win32_error(e: windows::core::Error) -> std::io::Error {
    let code = e.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        std::io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        std::io::Error::other(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_arg_no_quoting_needed() {
        assert_eq!(quote_arg("hello"), "hello");
    }

    #[test]
    fn empty_arg_wraps_in_quotes() {
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn arg_with_space_wraps_in_quotes() {
        assert_eq!(quote_arg("two words"), "\"two words\"");
    }

    #[test]
    fn arg_with_double_quote_escapes() {
        assert_eq!(quote_arg(r#"a"b"#), "\"a\\\"b\"");
    }

    #[test]
    fn arg_with_backslashes_before_quote() {
        assert_eq!(quote_arg(r"a\"), "a\\");
    }

    #[test]
    fn arg_with_multiple_backslashes_before_quote() {
        assert_eq!(quote_arg(r##"a\\\"b"##), r#""a\\\\\\\"b""#);
    }

    #[test]
    fn arg_with_tab_requires_quoting() {
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn cancelled_is_declined() {
        assert!(is_declined(&std::io::Error::from_raw_os_error(1223)));
        assert!(!is_declined(&std::io::Error::from_raw_os_error(5)));
    }

    #[cfg(windows)]
    #[test]
    fn win32_errors_keep_their_os_code() {
        use windows::core::{Error, HRESULT};
        let cancelled = win32_error(Error::from_hresult(HRESULT::from_win32(1223)));
        assert!(is_declined(&cancelled));
        assert_eq!(
            win32_error(Error::from_hresult(HRESULT::from_win32(5))).raw_os_error(),
            Some(5)
        );
        assert_eq!(
            win32_error(Error::from_hresult(HRESULT(0x8000_4005u32 as i32))).raw_os_error(),
            None
        );
    }
}
