//! Generic, editable keyboard-layout model.
//!
//! A keyboard layout is described as a vendor-neutral **base template**
//! ([`StandardLayout`]) plus an ordered list of per-device **edits**
//! ([`KeyEdit`]) that add, remove, or move individual keys. A device profile
//! references a [`KeyLayoutSpec`], which also carries a `cid_map` translating
//! the device's firmware/internal key IDs onto the resolved key set.
//!
//! The base template provides logical grid geometry (`col`/`row` in arbitrary
//! grid units, plus key `w`/`h`); a driver maps each resolved [`KeyCell`] to a
//! renderable LED position by translating the firmware ID through the
//! `cid_map`. Nothing in this module is vendor-specific.

use serde::{Deserialize, Serialize};

/// Device-neutral identity of a single physical key.
///
/// Covers every key found on a standard keyboard, plus [`KeyId::Custom`] for
/// keys a particular device adds that have no standard equivalent (media keys,
/// macro/G-keys, an `Fn` key, scroll-wheel tilt, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyId {
    // Function row
    Escape,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    // Number row
    Backtick,
    Digit1,
    Digit2,
    Digit3,
    Digit4,
    Digit5,
    Digit6,
    Digit7,
    Digit8,
    Digit9,
    Digit0,
    Minus,
    Equals,
    // Letters
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    K,
    L,
    M,
    N,
    O,
    P,
    Q,
    R,
    S,
    T,
    U,
    V,
    W,
    X,
    Y,
    Z,
    // Punctuation
    LeftBracket,
    RightBracket,
    Backslash,
    Semicolon,
    Quote,
    Comma,
    Period,
    Slash,
    /// The extra `<>` key found on ISO layouts (left of `Z`).
    IsoExtra,
    // Whitespace / editing
    Tab,
    CapsLock,
    Enter,
    Backspace,
    Space,
    // Modifiers
    LeftShift,
    RightShift,
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
    LeftSuper,
    RightSuper,
    // Navigation cluster
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    // Arrows
    Up,
    Down,
    Left,
    Right,
    // System keys
    PrintScreen,
    ScrollLock,
    Pause,
    Menu,
    // Numpad
    NumLock,
    NumDiv,
    NumMul,
    NumSub,
    NumAdd,
    NumEnter,
    Num0,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    NumDot,
    /// A non-standard key identified by a device-defined opaque number.
    Custom(u16),
}

/// One key's logical identity plus its grid geometry.
///
/// `col`/`row` place the key on an arbitrary logical grid (smaller `row` is
/// physically higher); `w`/`h` are the key's size in the same grid units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KeyCell {
    pub id: KeyId,
    pub col: f32,
    pub row: f32,
    pub w: f32,
    pub h: f32,
}

impl KeyCell {
    /// Convenience constructor for a default `1x1` key.
    pub const fn new(id: KeyId, col: f32, row: f32) -> Self {
        Self { id, col, row, w: 1.0, h: 1.0 }
    }
}

/// Named base keyboard templates.
///
/// - [`StandardLayout::Tkl`] — ANSI TKL, 86 keys. Enter in home row, Backslash in QWERTY row.
/// - [`StandardLayout::TklIso`] — ISO 105 TKL, 86 keys. Enter in QWERTY-row upper arm, Backslash in home row.
/// - [`StandardLayout::FullSize`] — ANSI 100%, 105 keys (TKL + `RightSuper` + `Menu` + numpad).
/// - [`StandardLayout::FullSizeIso`] — ISO 105 100%, 105 keys (`TklIso` + `RightSuper` + `Menu` + numpad).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandardLayout {
    FullSize,
    FullSizeIso,
    Tkl,
    TklIso,
}

impl StandardLayout {
    /// The ordered grid of keys for this template.
    pub fn cells(&self) -> Vec<KeyCell> {
        match self {
            StandardLayout::Tkl => tkl_cells(),
            StandardLayout::TklIso => iso_tkl_cells(),
            StandardLayout::FullSize => full_size_cells(),
            StandardLayout::FullSizeIso => iso_full_size_cells(),
        }
    }
}

/// A single per-device modification applied over a [`StandardLayout`] base.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum KeyEdit {
    /// Append a key not present on the base template.
    Add(KeyCell),
    /// Remove the key with this identity from the base template.
    Remove(KeyId),
    /// Move an existing key; `None` fields are left unchanged.
    Modify { id: KeyId, col: Option<f32>, row: Option<f32> },
}

/// A complete keyboard layout: a base template, device edits, and a map from
/// the device's firmware/internal key IDs onto resolved [`KeyId`]s.
#[derive(Debug, Clone)]
pub struct KeyLayoutSpec {
    pub base: StandardLayout,
    pub edits: &'static [KeyEdit],
    /// Maps a device's firmware/internal key ID to a [`KeyId`].
    pub cid_map: &'static [(u32, KeyId)],
}

impl KeyLayoutSpec {
    /// Apply `edits` over `base.cells()` and return the resolved key grid.
    ///
    /// `Add` appends, `Remove` drops every matching key, `Modify` patches the
    /// `col`/`row` of every matching key.
    pub fn resolve(&self) -> Vec<KeyCell> {
        let mut cells = self.base.cells();
        for edit in self.edits {
            match edit {
                KeyEdit::Add(cell) => cells.push(*cell),
                KeyEdit::Remove(id) => cells.retain(|c| c.id != *id),
                KeyEdit::Modify { id, col, row } => {
                    for cell in cells.iter_mut().filter(|c| c.id == *id) {
                        if let Some(c) = col {
                            cell.col = *c;
                        }
                        if let Some(r) = row {
                            cell.row = *r;
                        }
                    }
                }
            }
        }
        cells
    }
}

/// ANSI TKL key grid — 86 keys.
///
/// `col` ∈ [0, 17.5], `row` ∈ [0, 7.5] (smaller `row` = physically higher).
/// Enter is in the home row (ANSI single-row key); Backslash is in the QWERTY row.
/// IsoExtra is included at its standard position so ISO cid_maps can reference it
/// (ANSI keyboards simply omit it from their cid_map, so it gets filtered out).
fn tkl_cells() -> Vec<KeyCell> {
    use KeyId::*;
    [
        // Function row
        (Escape, 0.0, 0.0),
        (F1, 2.0, 0.0),
        (F2, 3.0, 0.0),
        (F3, 4.0, 0.0),
        (F4, 5.0, 0.0),
        (F5, 6.5, 0.0),
        (F6, 7.5, 0.0),
        (F7, 8.5, 0.0),
        (F8, 9.5, 0.0),
        (F9, 11.0, 0.0),
        (F10, 12.0, 0.0),
        (F11, 13.0, 0.0),
        (F12, 14.0, 0.0),
        (PrintScreen, 15.5, 0.0),
        (ScrollLock, 16.5, 0.0),
        (Pause, 17.5, 0.0),
        // Number row
        (Backtick, 0.0, 1.5),
        (Digit1, 1.0, 1.5),
        (Digit2, 2.0, 1.5),
        (Digit3, 3.0, 1.5),
        (Digit4, 4.0, 1.5),
        (Digit5, 5.0, 1.5),
        (Digit6, 6.0, 1.5),
        (Digit7, 7.0, 1.5),
        (Digit8, 8.0, 1.5),
        (Digit9, 9.0, 1.5),
        (Digit0, 10.0, 1.5),
        (Minus, 11.0, 1.5),
        (Equals, 12.0, 1.5),
        (Backspace, 13.5, 1.5),
        (Insert, 15.5, 1.5),
        (Home, 16.5, 1.5),
        (PageUp, 17.5, 1.5),
        // QWERTY row — Backslash at the ANSI position (right of `]`)
        (Tab, 0.75, 3.0),
        (Q, 1.75, 3.0),
        (W, 2.75, 3.0),
        (E, 3.75, 3.0),
        (R, 4.75, 3.0),
        (T, 5.75, 3.0),
        (Y, 6.75, 3.0),
        (U, 7.75, 3.0),
        (I, 8.75, 3.0),
        (O, 9.75, 3.0),
        (P, 10.75, 3.0),
        (LeftBracket, 11.75, 3.0),
        (RightBracket, 12.75, 3.0),
        (Backslash, 13.75, 3.0),
        (Delete, 15.5, 3.0),
        (End, 16.5, 3.0),
        (PageDown, 17.5, 3.0),
        // Home row — Enter at the ANSI position (wide key at end of row)
        (CapsLock, 0.875, 4.5),
        (A, 1.875, 4.5),
        (S, 2.875, 4.5),
        (D, 3.875, 4.5),
        (F, 4.875, 4.5),
        (G, 5.875, 4.5),
        (H, 6.875, 4.5),
        (J, 7.875, 4.5),
        (K, 8.875, 4.5),
        (L, 9.875, 4.5),
        (Semicolon, 10.875, 4.5),
        (Quote, 11.875, 4.5),
        (Enter, 13.875, 4.5),
        // ZXCV row
        (LeftShift, 1.125, 6.0),
        (IsoExtra, 2.25, 6.0),
        (Z, 3.25, 6.0),
        (X, 4.25, 6.0),
        (C, 5.25, 6.0),
        (V, 6.25, 6.0),
        (B, 7.25, 6.0),
        (N, 8.25, 6.0),
        (M, 9.25, 6.0),
        (Comma, 10.25, 6.0),
        (Period, 11.25, 6.0),
        (Slash, 12.25, 6.0),
        (RightShift, 13.625, 6.0),
        (Up, 16.5, 6.0),
        // Modifier row
        (LeftCtrl, 0.75, 7.5),
        (LeftSuper, 1.75, 7.5),
        (LeftAlt, 2.75, 7.5),
        (Space, 6.5, 7.5),
        (RightAlt, 10.25, 7.5),
        (RightCtrl, 13.25, 7.5),
        (Left, 15.5, 7.5),
        (Down, 16.5, 7.5),
        (Right, 17.5, 7.5),
    ]
    .into_iter()
    .map(|(id, col, row)| KeyCell::new(id, col, row))
    .collect()
}

/// ISO 105 TKL key grid — 86 keys.
///
/// Differs from [`tkl_cells`] in two ways:
/// - `Enter` is the ISO inverted-L shape; LED placed in the upper arm (QWERTY-row level).
/// - `Backslash` (`#/\` key) is in the home row, between `Quote` and the lower arm of `Enter`.
fn iso_tkl_cells() -> Vec<KeyCell> {
    use KeyId::*;
    [
        // Function row
        (Escape, 0.0, 0.0),
        (F1, 2.0, 0.0),
        (F2, 3.0, 0.0),
        (F3, 4.0, 0.0),
        (F4, 5.0, 0.0),
        (F5, 6.5, 0.0),
        (F6, 7.5, 0.0),
        (F7, 8.5, 0.0),
        (F8, 9.5, 0.0),
        (F9, 11.0, 0.0),
        (F10, 12.0, 0.0),
        (F11, 13.0, 0.0),
        (F12, 14.0, 0.0),
        (PrintScreen, 15.5, 0.0),
        (ScrollLock, 16.5, 0.0),
        (Pause, 17.5, 0.0),
        // Number row
        (Backtick, 0.0, 1.5),
        (Digit1, 1.0, 1.5),
        (Digit2, 2.0, 1.5),
        (Digit3, 3.0, 1.5),
        (Digit4, 4.0, 1.5),
        (Digit5, 5.0, 1.5),
        (Digit6, 6.0, 1.5),
        (Digit7, 7.0, 1.5),
        (Digit8, 8.0, 1.5),
        (Digit9, 9.0, 1.5),
        (Digit0, 10.0, 1.5),
        (Minus, 11.0, 1.5),
        (Equals, 12.0, 1.5),
        (Backspace, 13.5, 1.5),
        (Insert, 15.5, 1.5),
        (Home, 16.5, 1.5),
        (PageUp, 17.5, 1.5),
        // QWERTY row — ISO Enter upper arm replaces the ANSI `\` slot
        (Tab, 0.75, 3.0),
        (Q, 1.75, 3.0),
        (W, 2.75, 3.0),
        (E, 3.75, 3.0),
        (R, 4.75, 3.0),
        (T, 5.75, 3.0),
        (Y, 6.75, 3.0),
        (U, 7.75, 3.0),
        (I, 8.75, 3.0),
        (O, 9.75, 3.0),
        (P, 10.75, 3.0),
        (LeftBracket, 11.75, 3.0),
        (RightBracket, 12.75, 3.0),
        (Enter, 14.25, 3.0),
        (Delete, 15.5, 3.0),
        (End, 16.5, 3.0),
        (PageDown, 17.5, 3.0),
        // Home row — Backslash (`#/\` key) is between Quote and Enter lower arm
        (CapsLock, 0.875, 4.5),
        (A, 1.875, 4.5),
        (S, 2.875, 4.5),
        (D, 3.875, 4.5),
        (F, 4.875, 4.5),
        (G, 5.875, 4.5),
        (H, 6.875, 4.5),
        (J, 7.875, 4.5),
        (K, 8.875, 4.5),
        (L, 9.875, 4.5),
        (Semicolon, 10.875, 4.5),
        (Quote, 11.875, 4.5),
        (Backslash, 12.875, 4.5),
        // ZXCV row — IsoExtra between LeftShift and Z
        (LeftShift, 1.125, 6.0),
        (IsoExtra, 2.25, 6.0),
        (Z, 3.25, 6.0),
        (X, 4.25, 6.0),
        (C, 5.25, 6.0),
        (V, 6.25, 6.0),
        (B, 7.25, 6.0),
        (N, 8.25, 6.0),
        (M, 9.25, 6.0),
        (Comma, 10.25, 6.0),
        (Period, 11.25, 6.0),
        (Slash, 12.25, 6.0),
        (RightShift, 13.625, 6.0),
        (Up, 16.5, 6.0),
        // Modifier row
        (LeftCtrl, 0.75, 7.5),
        (LeftSuper, 1.75, 7.5),
        (LeftAlt, 2.75, 7.5),
        (Space, 6.5, 7.5),
        (RightAlt, 10.25, 7.5),
        (RightCtrl, 13.25, 7.5),
        (Left, 15.5, 7.5),
        (Down, 16.5, 7.5),
        (Right, 17.5, 7.5),
    ]
    .into_iter()
    .map(|(id, col, row)| KeyCell::new(id, col, row))
    .collect()
}

/// Keys present on 100% keyboards but absent from the TKL template: `RightSuper`,
/// `Menu`, and a full numpad. Shared by both ANSI and ISO full-size grids.
fn full_size_extra_cells() -> impl Iterator<Item = KeyCell> {
    use KeyId::*;
    [
        // Modifier row additions
        (RightSuper, 11.25, 7.5),
        (Menu,       12.25, 7.5),
        // Numpad — top row (same row level as number row)
        (NumLock,  19.5, 1.5),
        (NumDiv,   20.5, 1.5),
        (NumMul,   21.5, 1.5),
        (NumSub,   22.5, 1.5),
        // Numpad — second row
        (Num7,     19.5, 3.0),
        (Num8,     20.5, 3.0),
        (Num9,     21.5, 3.0),
        (NumAdd,   22.5, 3.75), // tall 1×2, centred between rows 3.0 and 4.5
        // Numpad — middle row
        (Num4,     19.5, 4.5),
        (Num5,     20.5, 4.5),
        (Num6,     21.5, 4.5),
        // Numpad — lower row
        (Num1,     19.5, 6.0),
        (Num2,     20.5, 6.0),
        (Num3,     21.5, 6.0),
        (NumEnter, 22.5, 6.75), // tall 1×2, centred between rows 6.0 and 7.5
        // Numpad — bottom row
        (Num0,     20.0, 7.5), // wide 2×1, centred
        (NumDot,   21.5, 7.5),
    ]
    .into_iter()
    .map(|(id, col, row)| KeyCell::new(id, col, row))
}

/// ANSI 100% key grid — 105 keys.
///
/// TKL base + `RightSuper` + `Menu` + 17 numpad keys.
fn full_size_cells() -> Vec<KeyCell> {
    let mut cells = tkl_cells();
    cells.extend(full_size_extra_cells());
    cells
}

/// ISO 105 100% key grid — 105 keys.
///
/// ISO TKL base + `RightSuper` + `Menu` + 17 numpad keys.
fn iso_full_size_cells() -> Vec<KeyCell> {
    let mut cells = iso_tkl_cells();
    cells.extend(full_size_extra_cells());
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec(edits: &'static [KeyEdit]) -> KeyLayoutSpec {
        KeyLayoutSpec { base: StandardLayout::Tkl, edits, cid_map: &[] }
    }

    #[test]
    fn tkl_base_has_expected_key_count() {
        // 16 (fn row) + 17 (num row) + 18 (qwerty + enter) + 12 (home row)
        // + 14 (zxcv + up) + 9 (modifier row) = 86 keys.
        assert_eq!(StandardLayout::Tkl.cells().len(), 86);
    }

    #[test]
    fn full_size_base_has_expected_key_count() {
        // 86 (TKL) + 2 (RightSuper, Menu) + 17 (numpad) = 105 keys.
        assert_eq!(StandardLayout::FullSize.cells().len(), 105);
    }

    #[test]
    fn full_size_base_has_no_duplicate_keys() {
        let cells = StandardLayout::FullSize.cells();
        let mut ids: Vec<KeyId> = cells.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_by_key(|id| format!("{id:?}"));
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate KeyId in FullSize base");
    }

    #[test]
    fn tkl_base_has_no_duplicate_keys() {
        let cells = StandardLayout::Tkl.cells();
        let mut ids: Vec<KeyId> = cells.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_by_key(|id| format!("{id:?}"));
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate KeyId in TKL base");
    }

    #[test]
    fn tkl_ansi_enter_is_in_home_row() {
        let cells = StandardLayout::Tkl.cells();
        let enter = cells.iter().find(|c| c.id == KeyId::Enter).expect("Enter missing");
        assert_eq!(enter.row, 4.5, "ANSI Enter must be in the home row (row 4.5)");
    }

    #[test]
    fn tkl_iso_enter_is_in_qwerty_row() {
        let cells = StandardLayout::TklIso.cells();
        let enter = cells.iter().find(|c| c.id == KeyId::Enter).expect("Enter missing");
        assert_eq!(enter.row, 3.0, "ISO Enter LED must be in the QWERTY row (upper arm)");
    }

    #[test]
    fn tkl_iso_backslash_is_in_home_row() {
        let cells = StandardLayout::TklIso.cells();
        let bs = cells.iter().find(|c| c.id == KeyId::Backslash).expect("Backslash missing");
        assert_eq!(bs.row, 4.5, "ISO Backslash must be in the home row");
    }

    #[test]
    fn tkl_iso_base_has_expected_key_count() {
        assert_eq!(StandardLayout::TklIso.cells().len(), 86);
    }

    #[test]
    fn tkl_iso_base_has_no_duplicate_keys() {
        let cells = StandardLayout::TklIso.cells();
        let mut ids: Vec<KeyId> = cells.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_by_key(|id| format!("{id:?}"));
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate KeyId in TklIso base");
    }

    #[test]
    fn full_size_iso_base_has_expected_key_count() {
        assert_eq!(StandardLayout::FullSizeIso.cells().len(), 105);
    }

    #[test]
    fn full_size_iso_base_has_no_duplicate_keys() {
        let cells = StandardLayout::FullSizeIso.cells();
        let mut ids: Vec<KeyId> = cells.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_by_key(|id| format!("{id:?}"));
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate KeyId in FullSizeIso base");
    }

    #[test]
    fn resolve_add_appends_key() {
        static EDITS: &[KeyEdit] =
            &[KeyEdit::Add(KeyCell::new(KeyId::Custom(99), 1.0, 2.0))];
        let cells = test_spec(EDITS).resolve();
        assert_eq!(cells.len(), 87);
        assert!(cells.iter().any(|c| c.id == KeyId::Custom(99)));
    }

    #[test]
    fn resolve_remove_drops_key() {
        static EDITS: &[KeyEdit] = &[KeyEdit::Remove(KeyId::Escape)];
        let cells = test_spec(EDITS).resolve();
        assert_eq!(cells.len(), 85);
        assert!(!cells.iter().any(|c| c.id == KeyId::Escape));
    }

    #[test]
    fn resolve_modify_patches_position() {
        static EDITS: &[KeyEdit] = &[KeyEdit::Modify {
            id: KeyId::Escape,
            col: Some(9.0),
            row: None,
        }];
        let cells = test_spec(EDITS).resolve();
        let esc = cells.iter().find(|c| c.id == KeyId::Escape).unwrap();
        assert_eq!(esc.col, 9.0);
        assert_eq!(esc.row, 0.0, "row left unchanged when None");
    }

    #[test]
    fn resolve_applies_edits_in_order() {
        static EDITS: &[KeyEdit] = &[
            KeyEdit::Add(KeyCell::new(KeyId::Custom(1), 0.0, 0.0)),
            KeyEdit::Modify { id: KeyId::Custom(1), col: Some(5.0), row: Some(6.0) },
            KeyEdit::Remove(KeyId::F1),
        ];
        let cells = test_spec(EDITS).resolve();
        assert!(!cells.iter().any(|c| c.id == KeyId::F1));
        let custom = cells.iter().find(|c| c.id == KeyId::Custom(1)).unwrap();
        assert_eq!((custom.col, custom.row), (5.0, 6.0));
    }
}
