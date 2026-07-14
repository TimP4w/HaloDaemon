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

use crate::types::KeyboardLayout;

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
        Self {
            id,
            col,
            row,
            w: 1.0,
            h: 1.0,
        }
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
    Modify {
        id: KeyId,
        col: Option<f32>,
        row: Option<f32>,
    },
}

/// A complete keyboard layout: a base template, device edits, and a map from
/// the device's firmware/internal key IDs onto resolved [`KeyId`]s.
#[derive(Debug, Clone, Copy)]
pub struct KeyLayoutSpec<'a> {
    pub base: StandardLayout,
    pub edits: &'a [KeyEdit],
    /// Maps a device's firmware/internal key ID to a [`KeyId`].
    pub cid_map: &'a [(u32, KeyId)],
}

impl<'a> KeyLayoutSpec<'a> {
    /// Apply `edits` over `base.cells()` and return the resolved key grid.
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

/// Physical keyboard variant: the shape of the alpha block, independent of the
/// language legends printed on the caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyVariant {
    Ansi,
    Iso,
}

/// A user's per-device layout selection. `None` on either axis means "Auto" —
/// resolve it from the firmware-detected language (and, for the variant, from
/// whether that language is an ISO language and the device supports ISO).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardLayoutSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<KeyVariant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<KeyboardLayout>,
}

impl KeyboardLayoutSelection {
    /// True when both axes are Auto — the state stored as *absence* in config.
    pub fn is_auto(&self) -> bool {
        self.variant.is_none() && self.language.is_none()
    }
}

/// One renderable key: its resolved grid geometry plus the device IDs that
/// address it — an RGB LED id (for the Lighting tab) and, optionally, a
/// KeyRemap control id (for the clickable Keys tab).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualKey {
    pub led_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remap_cid: Option<u16>,
    pub cell: KeyCell,
}

/// Everything both GUI tabs need to draw a keyboard and its layout selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardLayoutStatus {
    /// Resolved key grid. Empty ⇒ the device has no grid geometry, so the GUI
    /// keeps its bare LED-dot rendering but still shows the selector.
    #[serde(default)]
    pub keys: Vec<VisualKey>,
    /// Effective variant after resolving the selection.
    pub variant: KeyVariant,
    /// Effective language after resolving the selection.
    pub language: KeyboardLayout,
    /// Language reported by the firmware; `Unknown` when not detectable.
    pub detected_language: KeyboardLayout,
    /// The user's raw selection (Auto = `None` on an axis).
    pub selection: KeyboardLayoutSelection,
    /// Whether the device declares an ISO variant (gates the variant selector).
    pub iso_supported: bool,
    /// Languages the device offers.
    pub languages: Vec<KeyboardLayout>,
}

/// Whether a language layout is physically an ISO layout (implies the ISO
/// alpha block when the variant is left on Auto).
pub fn is_iso_language(l: KeyboardLayout) -> bool {
    matches!(
        l,
        KeyboardLayout::CH
            | KeyboardLayout::IT
            | KeyboardLayout::DE
            | KeyboardLayout::FR
            | KeyboardLayout::UK
    )
}

/// ANSI TKL key grid — 86 keys (smaller `row` = physically higher).
///
/// IsoExtra is included so ISO cid_maps can reference it.
fn tkl_cells() -> Vec<KeyCell> {
    let mut cells = tkl_common_cells();
    // QWERTY row — Backslash at the ANSI position (right of `]`, 1.5u wide).
    cells.push(cell(KeyId::Backslash, 13.5, 2.5, 1.5, 1.0));
    // Home row — Enter at the ANSI position (2.25u wide key at end of row).
    cells.push(cell(KeyId::Enter, 12.75, 3.5, 2.25, 1.0));
    cells
}

/// A key cell with explicit width/height. `col` is the key's **left edge**.
const fn cell(id: KeyId, col: f32, row: f32, w: f32, h: f32) -> KeyCell {
    KeyCell { id, col, row, w, h }
}

/// The keys common to both the ANSI and ISO TKL grids — every key except the
/// two that differ in shape/position between variants (`Enter`, `Backslash`).
fn tkl_common_cells() -> Vec<KeyCell> {
    use KeyId::*;
    [
        // Function row
        (Escape, 0.0, 0.0, 1.0, 1.0),
        (F1, 2.0, 0.0, 1.0, 1.0),
        (F2, 3.0, 0.0, 1.0, 1.0),
        (F3, 4.0, 0.0, 1.0, 1.0),
        (F4, 5.0, 0.0, 1.0, 1.0),
        (F5, 6.5, 0.0, 1.0, 1.0),
        (F6, 7.5, 0.0, 1.0, 1.0),
        (F7, 8.5, 0.0, 1.0, 1.0),
        (F8, 9.5, 0.0, 1.0, 1.0),
        (F9, 11.0, 0.0, 1.0, 1.0),
        (F10, 12.0, 0.0, 1.0, 1.0),
        (F11, 13.0, 0.0, 1.0, 1.0),
        (F12, 14.0, 0.0, 1.0, 1.0),
        (PrintScreen, 15.5, 0.0, 1.0, 1.0),
        (ScrollLock, 16.5, 0.0, 1.0, 1.0),
        (Pause, 17.5, 0.0, 1.0, 1.0),
        // Number row
        (Backtick, 0.0, 1.5, 1.0, 1.0),
        (Digit1, 1.0, 1.5, 1.0, 1.0),
        (Digit2, 2.0, 1.5, 1.0, 1.0),
        (Digit3, 3.0, 1.5, 1.0, 1.0),
        (Digit4, 4.0, 1.5, 1.0, 1.0),
        (Digit5, 5.0, 1.5, 1.0, 1.0),
        (Digit6, 6.0, 1.5, 1.0, 1.0),
        (Digit7, 7.0, 1.5, 1.0, 1.0),
        (Digit8, 8.0, 1.5, 1.0, 1.0),
        (Digit9, 9.0, 1.5, 1.0, 1.0),
        (Digit0, 10.0, 1.5, 1.0, 1.0),
        (Minus, 11.0, 1.5, 1.0, 1.0),
        (Equals, 12.0, 1.5, 1.0, 1.0),
        (Backspace, 13.0, 1.5, 2.0, 1.0),
        (Insert, 15.5, 1.5, 1.0, 1.0),
        (Home, 16.5, 1.5, 1.0, 1.0),
        (PageUp, 17.5, 1.5, 1.0, 1.0),
        // QWERTY row (Tab 1.5u; the variant key at col 13.5 is added by caller)
        (Tab, 0.0, 2.5, 1.5, 1.0),
        (Q, 1.5, 2.5, 1.0, 1.0),
        (W, 2.5, 2.5, 1.0, 1.0),
        (E, 3.5, 2.5, 1.0, 1.0),
        (R, 4.5, 2.5, 1.0, 1.0),
        (T, 5.5, 2.5, 1.0, 1.0),
        (Y, 6.5, 2.5, 1.0, 1.0),
        (U, 7.5, 2.5, 1.0, 1.0),
        (I, 8.5, 2.5, 1.0, 1.0),
        (O, 9.5, 2.5, 1.0, 1.0),
        (P, 10.5, 2.5, 1.0, 1.0),
        (LeftBracket, 11.5, 2.5, 1.0, 1.0),
        (RightBracket, 12.5, 2.5, 1.0, 1.0),
        (Delete, 15.5, 2.5, 1.0, 1.0),
        (End, 16.5, 2.5, 1.0, 1.0),
        (PageDown, 17.5, 2.5, 1.0, 1.0),
        // Home row (CapsLock 1.75u; the variant keys after Quote are per-caller)
        (CapsLock, 0.0, 3.5, 1.75, 1.0),
        (A, 1.75, 3.5, 1.0, 1.0),
        (S, 2.75, 3.5, 1.0, 1.0),
        (D, 3.75, 3.5, 1.0, 1.0),
        (F, 4.75, 3.5, 1.0, 1.0),
        (G, 5.75, 3.5, 1.0, 1.0),
        (H, 6.75, 3.5, 1.0, 1.0),
        (J, 7.75, 3.5, 1.0, 1.0),
        (K, 8.75, 3.5, 1.0, 1.0),
        (L, 9.75, 3.5, 1.0, 1.0),
        (Semicolon, 10.75, 3.5, 1.0, 1.0),
        (Quote, 11.75, 3.5, 1.0, 1.0),
        // ZXCV row (short 1.25u LeftShift + IsoExtra; ANSI drops IsoExtra)
        (LeftShift, 0.0, 4.5, 1.25, 1.0),
        (IsoExtra, 1.25, 4.5, 1.0, 1.0),
        (Z, 2.25, 4.5, 1.0, 1.0),
        (X, 3.25, 4.5, 1.0, 1.0),
        (C, 4.25, 4.5, 1.0, 1.0),
        (V, 5.25, 4.5, 1.0, 1.0),
        (B, 6.25, 4.5, 1.0, 1.0),
        (N, 7.25, 4.5, 1.0, 1.0),
        (M, 8.25, 4.5, 1.0, 1.0),
        (Comma, 9.25, 4.5, 1.0, 1.0),
        (Period, 10.25, 4.5, 1.0, 1.0),
        (Slash, 11.25, 4.5, 1.0, 1.0),
        (RightShift, 12.25, 4.5, 2.75, 1.0),
        (Up, 16.5, 4.5, 1.0, 1.0),
        // Modifier row
        (LeftCtrl, 0.0, 5.5, 1.25, 1.0),
        (LeftSuper, 1.25, 5.5, 1.25, 1.0),
        (LeftAlt, 2.5, 5.5, 1.25, 1.0),
        (Space, 3.75, 5.5, 6.25, 1.0),
        (RightAlt, 10.0, 5.5, 1.25, 1.0),
        (RightCtrl, 13.75, 5.5, 1.25, 1.0),
        (Left, 15.5, 5.5, 1.0, 1.0),
        (Down, 16.5, 5.5, 1.0, 1.0),
        (Right, 17.5, 5.5, 1.0, 1.0),
    ]
    .into_iter()
    .map(|(id, col, row, w, h)| cell(id, col, row, w, h))
    .collect()
}

/// ISO 105 TKL key grid — 86 keys.
///
/// Differs from [`tkl_cells`] in two ways:
/// - `Enter` is the ISO inverted-L shape; LED placed in the upper arm (QWERTY-row level).
/// - `Backslash` (`#/\` key) is in the home row, between `Quote` and the lower arm of `Enter`.
fn iso_tkl_cells() -> Vec<KeyCell> {
    let mut cells = tkl_common_cells();
    // QWERTY row — ISO Enter upper arm (1.5u) replaces the ANSI `\` slot. LED
    // sits in the upper arm; the GUI derives the inverted-L lower arm.
    cells.push(cell(KeyId::Enter, 13.5, 2.5, 1.5, 1.0));
    // Home row — Backslash (`#/\` key) between Quote and the Enter lower arm.
    cells.push(cell(KeyId::Backslash, 12.75, 3.5, 1.0, 1.0));
    cells
}

/// Keys present on 100% keyboards but absent from the TKL template: `RightSuper`,
/// `Menu`, and a full numpad. Shared by both ANSI and ISO full-size grids.
fn full_size_extra_cells() -> impl Iterator<Item = KeyCell> {
    use KeyId::*;
    [
        // Modifier row additions (fill the RightAlt→RightCtrl gap on 100%).
        (RightSuper, 11.25, 5.5, 1.25, 1.0),
        (Menu, 12.5, 5.5, 1.25, 1.0),
        // Numpad — top row (same row level as number row)
        (NumLock, 19.5, 1.5, 1.0, 1.0),
        (NumDiv, 20.5, 1.5, 1.0, 1.0),
        (NumMul, 21.5, 1.5, 1.0, 1.0),
        (NumSub, 22.5, 1.5, 1.0, 1.0),
        // Numpad — second row
        (Num7, 19.5, 2.5, 1.0, 1.0),
        (Num8, 20.5, 2.5, 1.0, 1.0),
        (Num9, 21.5, 2.5, 1.0, 1.0),
        (NumAdd, 22.5, 3.0, 1.0, 2.0), // tall 1×2, spans the second + middle rows
        // Numpad — middle row
        (Num4, 19.5, 3.5, 1.0, 1.0),
        (Num5, 20.5, 3.5, 1.0, 1.0),
        (Num6, 21.5, 3.5, 1.0, 1.0),
        // Numpad — lower row
        (Num1, 19.5, 4.5, 1.0, 1.0),
        (Num2, 20.5, 4.5, 1.0, 1.0),
        (Num3, 21.5, 4.5, 1.0, 1.0),
        (NumEnter, 22.5, 5.0, 1.0, 2.0), // tall 1×2, spans the lower + bottom rows
        // Numpad — bottom row
        (Num0, 19.5, 5.5, 2.0, 1.0), // wide 2×1
        (NumDot, 21.5, 5.5, 1.0, 1.0),
    ]
    .into_iter()
    .map(|(id, col, row, w, h)| cell(id, col, row, w, h))
}

/// ANSI 100% key grid — 105 keys.
///
fn full_size_cells() -> Vec<KeyCell> {
    let mut cells = tkl_cells();
    cells.extend(full_size_extra_cells());
    cells
}

/// ISO 105 100% key grid — 105 keys.
fn iso_full_size_cells() -> Vec<KeyCell> {
    let mut cells = iso_tkl_cells();
    cells.extend(full_size_extra_cells());
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_spec(edits: &'static [KeyEdit]) -> KeyLayoutSpec<'static> {
        KeyLayoutSpec {
            base: StandardLayout::Tkl,
            edits,
            cid_map: &[],
        }
    }

    #[test]
    fn tkl_base_has_expected_key_count() {
        assert_eq!(StandardLayout::Tkl.cells().len(), 86);
    }

    #[test]
    fn full_size_base_has_expected_key_count() {
        assert_eq!(StandardLayout::FullSize.cells().len(), 105);
    }

    fn assert_no_duplicate_keys(layout: StandardLayout, name: &str) {
        let cells = layout.cells();
        let mut ids: Vec<KeyId> = cells.iter().map(|c| c.id).collect();
        let total = ids.len();
        ids.sort_by_key(|id| format!("{id:?}"));
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate KeyId in {name} base");
    }

    #[test]
    fn full_size_base_has_no_duplicate_keys() {
        assert_no_duplicate_keys(StandardLayout::FullSize, "FullSize");
    }

    #[test]
    fn tkl_base_has_no_duplicate_keys() {
        assert_no_duplicate_keys(StandardLayout::Tkl, "TKL");
    }

    fn row_of(cells: &[KeyCell], id: KeyId) -> f32 {
        cells
            .iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("{id:?} missing"))
            .row
    }

    #[test]
    fn tkl_ansi_enter_is_in_home_row() {
        let cells = StandardLayout::Tkl.cells();
        assert_eq!(
            row_of(&cells, KeyId::Enter),
            row_of(&cells, KeyId::A),
            "ANSI Enter shares the home row with A"
        );
    }

    #[test]
    fn tkl_iso_enter_is_in_qwerty_row() {
        let cells = StandardLayout::TklIso.cells();
        assert_eq!(
            row_of(&cells, KeyId::Enter),
            row_of(&cells, KeyId::Q),
            "ISO Enter LED sits in the QWERTY row (upper arm)"
        );
    }

    #[test]
    fn tkl_iso_backslash_is_in_home_row() {
        let cells = StandardLayout::TklIso.cells();
        assert_eq!(
            row_of(&cells, KeyId::Backslash),
            row_of(&cells, KeyId::A),
            "ISO Backslash sits in the home row"
        );
    }

    #[test]
    fn tkl_iso_base_has_expected_key_count() {
        assert_eq!(StandardLayout::TklIso.cells().len(), 86);
    }

    #[test]
    fn tkl_iso_base_has_no_duplicate_keys() {
        assert_no_duplicate_keys(StandardLayout::TklIso, "TklIso");
    }

    #[test]
    fn full_size_iso_base_has_expected_key_count() {
        assert_eq!(StandardLayout::FullSizeIso.cells().len(), 105);
    }

    #[test]
    fn full_size_iso_base_has_no_duplicate_keys() {
        assert_no_duplicate_keys(StandardLayout::FullSizeIso, "FullSizeIso");
    }

    #[test]
    fn resolve_add_appends_key() {
        static EDITS: &[KeyEdit] = &[KeyEdit::Add(KeyCell::new(KeyId::Custom(99), 1.0, 2.0))];
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
    fn resolve_modify_absent_key_is_noop() {
        // A Modify targeting a key not present in the base silently does nothing.
        static EDITS: &[KeyEdit] = &[KeyEdit::Modify {
            id: KeyId::Custom(9999),
            col: Some(1.0),
            row: Some(1.0),
        }];
        let base_len = StandardLayout::Tkl.cells().len();
        let cells = test_spec(EDITS).resolve();
        assert_eq!(
            cells.len(),
            base_len,
            "absent-key Modify must not add a cell"
        );
        assert!(
            !cells.iter().any(|c| c.id == KeyId::Custom(9999)),
            "absent-key Modify must not inject the key"
        );
    }

    #[test]
    fn resolve_applies_edits_in_order() {
        static EDITS: &[KeyEdit] = &[
            KeyEdit::Add(KeyCell::new(KeyId::Custom(1), 0.0, 0.0)),
            KeyEdit::Modify {
                id: KeyId::Custom(1),
                col: Some(5.0),
                row: Some(6.0),
            },
            KeyEdit::Remove(KeyId::F1),
        ];
        let cells = test_spec(EDITS).resolve();
        assert!(!cells.iter().any(|c| c.id == KeyId::F1));
        let custom = cells.iter().find(|c| c.id == KeyId::Custom(1)).unwrap();
        assert_eq!((custom.col, custom.row), (5.0, 6.0));
    }

    const ALL_LAYOUTS: [StandardLayout; 4] = [
        StandardLayout::Tkl,
        StandardLayout::TklIso,
        StandardLayout::FullSize,
        StandardLayout::FullSizeIso,
    ];

    /// `col` is the left edge, `row` the vertical centre, `w`/`h` the extent.
    fn rect(c: &KeyCell) -> (f32, f32, f32, f32) {
        (c.col, c.col + c.w, c.row - c.h / 2.0, c.row + c.h / 2.0)
    }

    fn rects_overlap(a: &KeyCell, b: &KeyCell) -> bool {
        let (ax0, ax1, ay0, ay1) = rect(a);
        let (bx0, bx1, by0, by1) = rect(b);
        // Strict overlap: touching edges (shared boundary) do not count.
        ax0 < bx1 - 1e-4 && bx0 < ax1 - 1e-4 && ay0 < by1 - 1e-4 && by0 < ay1 - 1e-4
    }

    #[test]
    fn base_layouts_have_no_overlapping_key_rects() {
        for layout in ALL_LAYOUTS {
            let cells = layout.cells();
            for i in 0..cells.len() {
                for j in (i + 1)..cells.len() {
                    assert!(
                        !rects_overlap(&cells[i], &cells[j]),
                        "{layout:?}: {:?} overlaps {:?}",
                        cells[i].id,
                        cells[j].id
                    );
                }
            }
        }
    }

    #[test]
    fn base_layouts_have_positive_key_sizes() {
        for layout in ALL_LAYOUTS {
            for c in layout.cells() {
                assert!(
                    c.w > 0.0 && c.h > 0.0,
                    "{layout:?}: {:?} has zero size",
                    c.id
                );
            }
        }
    }

    #[test]
    fn led_centers_are_in_unit_square() {
        for layout in ALL_LAYOUTS {
            let div = match layout {
                StandardLayout::FullSize | StandardLayout::FullSizeIso => 23.0,
                StandardLayout::Tkl | StandardLayout::TklIso => 18.0,
            };
            for c in layout.cells() {
                let x = (c.col + c.w / 2.0) / div;
                let y = (c.row + 1.5) / 7.0;
                assert!((0.0..=1.0).contains(&x), "{layout:?}: {:?} x={x}", c.id);
                assert!((0.0..=1.0).contains(&y), "{layout:?}: {:?} y={y}", c.id);
            }
        }
    }

    #[test]
    fn is_iso_language_table() {
        use KeyboardLayout::*;
        for l in [CH, IT, DE, FR, UK] {
            assert!(is_iso_language(l), "{l:?} should be ISO");
        }
        for l in [US, Unknown] {
            assert!(!is_iso_language(l), "{l:?} should not be ISO");
        }
    }

    #[test]
    fn selection_is_auto_only_when_both_none() {
        assert!(KeyboardLayoutSelection::default().is_auto());
        assert!(!KeyboardLayoutSelection {
            variant: Some(KeyVariant::Iso),
            language: None,
        }
        .is_auto());
        assert!(!KeyboardLayoutSelection {
            variant: None,
            language: Some(KeyboardLayout::US),
        }
        .is_auto());
    }

    #[test]
    fn keyboard_layout_status_json_roundtrips() {
        let status = KeyboardLayoutStatus {
            keys: vec![VisualKey {
                led_id: 7,
                remap_cid: Some(0x50),
                cell: KeyCell::new(KeyId::A, 1.75, 4.5),
            }],
            variant: KeyVariant::Iso,
            language: KeyboardLayout::CH,
            detected_language: KeyboardLayout::Unknown,
            selection: KeyboardLayoutSelection {
                variant: Some(KeyVariant::Iso),
                language: None,
            },
            iso_supported: true,
            languages: vec![KeyboardLayout::US, KeyboardLayout::CH],
        };
        let json = serde_json::to_string(&status).unwrap();
        let back: KeyboardLayoutStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.keys.len(), 1);
        assert_eq!(back.keys[0].led_id, 7);
        assert_eq!(back.keys[0].remap_cid, Some(0x50));
        assert_eq!(back.keys[0].cell.id, KeyId::A);
        assert_eq!(back.variant, KeyVariant::Iso);
        assert_eq!(back.language, KeyboardLayout::CH);
        assert_eq!(back.detected_language, KeyboardLayout::Unknown);
        assert_eq!(back.selection.variant, Some(KeyVariant::Iso));
        assert_eq!(back.selection.language, None);
        assert!(back.iso_supported);
        assert_eq!(back.languages.len(), 2);
    }

    #[test]
    fn key_variant_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&KeyVariant::Ansi).unwrap(),
            "\"ansi\""
        );
        assert_eq!(serde_json::to_string(&KeyVariant::Iso).unwrap(), "\"iso\"");
    }
}
