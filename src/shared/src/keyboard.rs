use serde::{Deserialize, Serialize};

use crate::types::KeyboardLayout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyId {
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
    Tab,
    CapsLock,
    Enter,
    Backspace,
    Space,
    LeftShift,
    RightShift,
    LeftCtrl,
    RightCtrl,
    LeftAlt,
    RightAlt,
    LeftSuper,
    RightSuper,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Up,
    Down,
    Left,
    Right,
    PrintScreen,
    ScrollLock,
    Pause,
    Menu,
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
    Custom(u16),
}

/// Grid units; smaller `row` is physically higher.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KeyCell {
    pub id: KeyId,
    pub col: f32,
    pub row: f32,
    pub w: f32,
    pub h: f32,
}

impl KeyCell {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandardLayout {
    FullSize,
    FullSizeIso,
    Tkl,
    TklIso,
}

impl StandardLayout {
    pub fn cells(&self) -> Vec<KeyCell> {
        match self {
            StandardLayout::Tkl => tkl_cells(),
            StandardLayout::TklIso => iso_tkl_cells(),
            StandardLayout::FullSize => full_size_cells(),
            StandardLayout::FullSizeIso => iso_full_size_cells(),
        }
    }
}

/// Alpha-block shape, independent of the printed language legends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyVariant {
    Ansi,
    Iso,
}

/// `None` on an axis means Auto: resolved from the firmware-detected language.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardLayoutSelection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<KeyVariant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<KeyboardLayout>,
}

impl KeyboardLayoutSelection {
    /// Stored as absence in config.
    pub fn is_auto(&self) -> bool {
        self.variant.is_none() && self.language.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualKey {
    pub led_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remap_cid: Option<u16>,
    pub cell: KeyCell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardLayoutStatus {
    /// Empty when the device has no grid geometry (GUI falls back to LED dots).
    #[serde(default)]
    pub keys: Vec<VisualKey>,
    pub variant: KeyVariant,
    pub language: KeyboardLayout,
    /// `Unknown` when not detectable.
    pub detected_language: KeyboardLayout,
    pub selection: KeyboardLayoutSelection,
    pub iso_supported: bool,
    pub languages: Vec<KeyboardLayout>,
}

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

/// IsoExtra is included so ISO cid_maps can reference it.
fn tkl_cells() -> Vec<KeyCell> {
    let mut cells = tkl_common_cells();
    cells.push(cell(KeyId::Backslash, 13.5, 2.5, 1.5, 1.0));
    cells.push(cell(KeyId::Enter, 12.75, 3.5, 2.25, 1.0));
    cells
}

/// `col` is the key's left edge.
const fn cell(id: KeyId, col: f32, row: f32, w: f32, h: f32) -> KeyCell {
    KeyCell { id, col, row, w, h }
}

/// Everything except `Enter` and `Backslash`, which differ between ANSI and ISO.
fn tkl_common_cells() -> Vec<KeyCell> {
    use KeyId::*;
    [
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
        // the variant key at col 13.5 is added by the caller
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
        // the variant keys after Quote are per-caller
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
        // ANSI drops IsoExtra
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

fn iso_tkl_cells() -> Vec<KeyCell> {
    let mut cells = tkl_common_cells();
    // LED sits in the upper arm; the GUI derives the inverted-L lower arm.
    cells.push(cell(KeyId::Enter, 13.5, 2.5, 1.5, 1.0));
    cells.push(cell(KeyId::Backslash, 12.75, 3.5, 1.0, 1.0));
    cells
}

fn full_size_extra_cells() -> impl Iterator<Item = KeyCell> {
    use KeyId::*;
    [
        (RightSuper, 11.25, 5.5, 1.25, 1.0),
        (Menu, 12.5, 5.5, 1.25, 1.0),
        (NumLock, 19.5, 1.5, 1.0, 1.0),
        (NumDiv, 20.5, 1.5, 1.0, 1.0),
        (NumMul, 21.5, 1.5, 1.0, 1.0),
        (NumSub, 22.5, 1.5, 1.0, 1.0),
        (Num7, 19.5, 2.5, 1.0, 1.0),
        (Num8, 20.5, 2.5, 1.0, 1.0),
        (Num9, 21.5, 2.5, 1.0, 1.0),
        (NumAdd, 22.5, 3.0, 1.0, 2.0), // tall 1×2, spans the second + middle rows
        (Num4, 19.5, 3.5, 1.0, 1.0),
        (Num5, 20.5, 3.5, 1.0, 1.0),
        (Num6, 21.5, 3.5, 1.0, 1.0),
        (Num1, 19.5, 4.5, 1.0, 1.0),
        (Num2, 20.5, 4.5, 1.0, 1.0),
        (Num3, 21.5, 4.5, 1.0, 1.0),
        (NumEnter, 22.5, 5.0, 1.0, 2.0), // tall 1×2, spans the lower + bottom rows
        (Num0, 19.5, 5.5, 2.0, 1.0),
        (NumDot, 21.5, 5.5, 1.0, 1.0),
    ]
    .into_iter()
    .map(|(id, col, row, w, h)| cell(id, col, row, w, h))
}

fn full_size_cells() -> Vec<KeyCell> {
    let mut cells = tkl_cells();
    cells.extend(full_size_extra_cells());
    cells
}

fn iso_full_size_cells() -> Vec<KeyCell> {
    let mut cells = iso_tkl_cells();
    cells.extend(full_size_extra_cells());
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

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

    const ALL_LAYOUTS: [StandardLayout; 4] = [
        StandardLayout::Tkl,
        StandardLayout::TklIso,
        StandardLayout::FullSize,
        StandardLayout::FullSizeIso,
    ];

    /// `row` is the vertical centre.
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
