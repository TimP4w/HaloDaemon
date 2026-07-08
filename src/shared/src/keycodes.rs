//! Keyboard key table shared by the GUI (recording/labels) and the daemon
//! (injection pin tests). `name` matches `egui::Key::name()` so the GUI can
//! look entries up directly from input events without an egui dependency
//! here; `linux` is the evdev key code, `windows` the virtual-key code —
//! the same platform-native `u32` convention as `MacroAtom`/`KeyChord`.

pub struct KeyDef {
    pub name: &'static str,
    pub linux: u32,
    pub windows: u32,
}

#[rustfmt::skip]
pub static KEYS: &[KeyDef] = &[
    KeyDef { name: "Down", linux: 108, windows: 0x28 },
    KeyDef { name: "Left", linux: 105, windows: 0x25 },
    KeyDef { name: "Right", linux: 106, windows: 0x27 },
    KeyDef { name: "Up", linux: 103, windows: 0x26 },
    KeyDef { name: "Escape", linux: 1, windows: 0x1B },
    KeyDef { name: "Tab", linux: 15, windows: 0x09 },
    KeyDef { name: "Backspace", linux: 14, windows: 0x08 },
    KeyDef { name: "Enter", linux: 28, windows: 0x0D },
    KeyDef { name: "Space", linux: 57, windows: 0x20 },
    KeyDef { name: "Insert", linux: 110, windows: 0x2D },
    KeyDef { name: "Delete", linux: 111, windows: 0x2E },
    KeyDef { name: "Home", linux: 102, windows: 0x24 },
    KeyDef { name: "End", linux: 107, windows: 0x23 },
    KeyDef { name: "PageUp", linux: 104, windows: 0x21 },
    KeyDef { name: "PageDown", linux: 109, windows: 0x22 },
    KeyDef { name: "Comma", linux: 51, windows: 0xBC },
    KeyDef { name: "Minus", linux: 12, windows: 0xBD },
    KeyDef { name: "Period", linux: 52, windows: 0xBE },
    KeyDef { name: "Equals", linux: 13, windows: 0xBB },
    KeyDef { name: "Semicolon", linux: 39, windows: 0xBA },
    KeyDef { name: "Quote", linux: 40, windows: 0xDE },
    KeyDef { name: "OpenBracket", linux: 26, windows: 0xDB },
    KeyDef { name: "CloseBracket", linux: 27, windows: 0xDD },
    KeyDef { name: "Backtick", linux: 41, windows: 0xC0 },
    KeyDef { name: "Backslash", linux: 43, windows: 0xDC },
    KeyDef { name: "Slash", linux: 53, windows: 0xBF },
    KeyDef { name: "IntlBackslash", linux: 86, windows: 0xE2 },
    KeyDef { name: "0", linux: 11, windows: 0x30 },
    KeyDef { name: "1", linux: 2, windows: 0x31 },
    KeyDef { name: "2", linux: 3, windows: 0x32 },
    KeyDef { name: "3", linux: 4, windows: 0x33 },
    KeyDef { name: "4", linux: 5, windows: 0x34 },
    KeyDef { name: "5", linux: 6, windows: 0x35 },
    KeyDef { name: "6", linux: 7, windows: 0x36 },
    KeyDef { name: "7", linux: 8, windows: 0x37 },
    KeyDef { name: "8", linux: 9, windows: 0x38 },
    KeyDef { name: "9", linux: 10, windows: 0x39 },
    KeyDef { name: "A", linux: 30, windows: 0x41 },
    KeyDef { name: "B", linux: 48, windows: 0x42 },
    KeyDef { name: "C", linux: 46, windows: 0x43 },
    KeyDef { name: "D", linux: 32, windows: 0x44 },
    KeyDef { name: "E", linux: 18, windows: 0x45 },
    KeyDef { name: "F", linux: 33, windows: 0x46 },
    KeyDef { name: "G", linux: 34, windows: 0x47 },
    KeyDef { name: "H", linux: 35, windows: 0x48 },
    KeyDef { name: "I", linux: 23, windows: 0x49 },
    KeyDef { name: "J", linux: 36, windows: 0x4A },
    KeyDef { name: "K", linux: 37, windows: 0x4B },
    KeyDef { name: "L", linux: 38, windows: 0x4C },
    KeyDef { name: "M", linux: 50, windows: 0x4D },
    KeyDef { name: "N", linux: 49, windows: 0x4E },
    KeyDef { name: "O", linux: 24, windows: 0x4F },
    KeyDef { name: "P", linux: 25, windows: 0x50 },
    KeyDef { name: "Q", linux: 16, windows: 0x51 },
    KeyDef { name: "R", linux: 19, windows: 0x52 },
    KeyDef { name: "S", linux: 31, windows: 0x53 },
    KeyDef { name: "T", linux: 20, windows: 0x54 },
    KeyDef { name: "U", linux: 22, windows: 0x55 },
    KeyDef { name: "V", linux: 47, windows: 0x56 },
    KeyDef { name: "W", linux: 17, windows: 0x57 },
    KeyDef { name: "X", linux: 45, windows: 0x58 },
    KeyDef { name: "Y", linux: 21, windows: 0x59 },
    KeyDef { name: "Z", linux: 44, windows: 0x5A },
    KeyDef { name: "F1", linux: 59, windows: 0x70 },
    KeyDef { name: "F2", linux: 60, windows: 0x71 },
    KeyDef { name: "F3", linux: 61, windows: 0x72 },
    KeyDef { name: "F4", linux: 62, windows: 0x73 },
    KeyDef { name: "F5", linux: 63, windows: 0x74 },
    KeyDef { name: "F6", linux: 64, windows: 0x75 },
    KeyDef { name: "F7", linux: 65, windows: 0x76 },
    KeyDef { name: "F8", linux: 66, windows: 0x77 },
    KeyDef { name: "F9", linux: 67, windows: 0x78 },
    KeyDef { name: "F10", linux: 68, windows: 0x79 },
    KeyDef { name: "F11", linux: 87, windows: 0x7A },
    KeyDef { name: "F12", linux: 88, windows: 0x7B },
    KeyDef { name: "F13", linux: 183, windows: 0x7C },
    KeyDef { name: "F14", linux: 184, windows: 0x7D },
    KeyDef { name: "F15", linux: 185, windows: 0x7E },
    KeyDef { name: "F16", linux: 186, windows: 0x7F },
    KeyDef { name: "F17", linux: 187, windows: 0x80 },
    KeyDef { name: "F18", linux: 188, windows: 0x81 },
    KeyDef { name: "F19", linux: 189, windows: 0x82 },
    KeyDef { name: "F20", linux: 190, windows: 0x83 },
    KeyDef { name: "F21", linux: 191, windows: 0x84 },
    KeyDef { name: "F22", linux: 192, windows: 0x85 },
    KeyDef { name: "F23", linux: 193, windows: 0x86 },
    KeyDef { name: "F24", linux: 194, windows: 0x87 },
    KeyDef { name: "BrowserBack", linux: 158, windows: 0xA6 },
    KeyDef { name: "ShiftLeft", linux: 42, windows: 0xA0 },
    KeyDef { name: "ShiftRight", linux: 54, windows: 0xA1 },
    KeyDef { name: "ControlLeft", linux: 29, windows: 0xA2 },
    KeyDef { name: "ControlRight", linux: 97, windows: 0xA3 },
    KeyDef { name: "AltLeft", linux: 56, windows: 0xA4 },
    KeyDef { name: "AltRight", linux: 100, windows: 0xA5 },
    KeyDef { name: "SuperLeft", linux: 125, windows: 0x5B },
    KeyDef { name: "SuperRight", linux: 126, windows: 0x5C },
];

pub fn by_name(name: &str) -> Option<&'static KeyDef> {
    KEYS.iter().find(|k| k.name == name)
}

fn platform_code(def: &KeyDef) -> u32 {
    #[cfg(target_os = "windows")]
    return def.windows;
    #[cfg(not(target_os = "windows"))]
    def.linux
}

/// Key code for `name` on the running platform.
pub fn native_code(name: &str) -> Option<u32> {
    by_name(name).map(platform_code)
}

/// Display name for a platform-native key code; hex fallback for codes
/// outside the table (e.g. hand-authored configs).
pub fn label(code: u32) -> String {
    KEYS.iter()
        .find(|k| platform_code(k) == code)
        .map(|k| k.name.to_string())
        .unwrap_or_else(|| format!("0x{code:X}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn names_and_codes_unique() {
        let names: HashSet<_> = KEYS.iter().map(|k| k.name).collect();
        let linux: HashSet<_> = KEYS.iter().map(|k| k.linux).collect();
        let windows: HashSet<_> = KEYS.iter().map(|k| k.windows).collect();
        assert_eq!(names.len(), KEYS.len());
        assert_eq!(linux.len(), KEYS.len());
        assert_eq!(windows.len(), KEYS.len());
    }

    #[test]
    fn linux_codes_within_uinput_range() {
        // The daemon's virtual keyboard registers evdev keys 1..=767.
        assert!(KEYS.iter().all(|k| (1..=767).contains(&k.linux)));
    }

    #[test]
    fn windows_codes_nonzero() {
        assert!(KEYS.iter().all(|k| k.windows != 0));
    }

    #[test]
    fn label_round_trips_every_entry() {
        for k in KEYS {
            assert_eq!(label(platform_code(k)), k.name);
            assert_eq!(native_code(k.name), Some(platform_code(k)));
        }
    }

    #[test]
    fn unknown_code_falls_back_to_hex() {
        assert_eq!(label(0xFFFF), "0xFFFF");
    }
}
