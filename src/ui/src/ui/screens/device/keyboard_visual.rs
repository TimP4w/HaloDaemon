// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyboard-shaped RGB/keys widget: real key-cap rectangles with legends,
//! replacing bare LED dots. All geometry/label logic is in free functions over
//! plain data so it is unit-testable without a live egui frame; `draw_keyboard`
//! is the only egui-facing entry point.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::keyboard::{KeyId, KeyVariant, KeyboardLayoutStatus, VisualKey};
use halod_shared::types::KeyboardLayout;

use crate::ui::theme;

/// Allocate an interactive, full-width panel with a rounded dark background,
/// returning its response and the padded inner rect to lay keys into. Shared by
/// the Lighting-tab paint canvas and the Keys-tab overview.
pub fn panel(ui: &mut egui::Ui, height: f32, sense: Sense) -> (egui::Response, Rect) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), sense);
    let p = ui.painter();
    p.rect_filled(rect, theme::RADIUS_MD, theme::hex(0x0a0d13));
    p.rect_stroke(
        rect,
        theme::RADIUS_MD,
        Stroke::new(1.0, theme::BORDER_INNER),
        egui::StrokeKind::Middle,
    );
    (resp, rect.shrink(14.0))
}

/// Grid bounding box `(min_x, max_x, min_y, max_y)` over every key cell. `col`
/// is a left edge, `row` a vertical centre, so a cell spans `[row-h/2, row+h/2]`.
fn grid_bounds(keys: &[VisualKey]) -> (f32, f32, f32, f32) {
    let mut minx = f32::MAX;
    let mut maxx = f32::MIN;
    let mut miny = f32::MAX;
    let mut maxy = f32::MIN;
    for k in keys {
        let c = &k.cell;
        minx = minx.min(c.col);
        maxx = maxx.max(c.col + c.w);
        miny = miny.min(c.row - c.h / 2.0);
        maxy = maxy.max(c.row + c.h / 2.0);
    }
    (minx, maxx, miny, maxy)
}

/// Screen rect for every key, laid out inside `avail` at a uniform scale
/// (aspect preserved), centred, each cap inset by `gap`.
pub fn key_rects(keys: &[VisualKey], avail: Rect, gap: f32) -> Vec<Rect> {
    if keys.is_empty() {
        return Vec::new();
    }
    let (minx, maxx, miny, maxy) = grid_bounds(keys);
    let gw = (maxx - minx).max(1e-3);
    let gh = (maxy - miny).max(1e-3);
    let scale = (avail.width() / gw).min(avail.height() / gh);
    let (used_w, used_h) = (gw * scale, gh * scale);
    let ox = avail.left() + (avail.width() - used_w) / 2.0;
    let oy = avail.top() + (avail.height() - used_h) / 2.0;
    keys.iter()
        .map(|k| {
            let c = &k.cell;
            let x0 = ox + (c.col - minx) * scale;
            let y0 = oy + ((c.row - c.h / 2.0) - miny) * scale;
            Rect::from_min_size(Pos2::new(x0, y0), Vec2::new(c.w * scale, c.h * scale))
                .shrink(gap * 0.5)
        })
        .collect()
}

/// One main-block row pitch, in the same grid units the base layouts use (the
/// alpha rows are 1.0 apart). The ISO Enter lower arm drops this far to reach
/// the home row.
const ROW_PITCH: f32 = 1.0;

/// The grid scale (pixels per grid unit) [`key_rects`] uses for `avail`. Passed
/// to [`iso_enter_lower`]/[`hit_key`]/[`draw_keyboard`] so the Enter's lower arm
/// reaches exactly the home-row bottom rather than the inset key edge.
pub fn unit_for(keys: &[VisualKey], avail: Rect) -> f32 {
    if keys.is_empty() {
        return 0.0;
    }
    let (minx, maxx, miny, maxy) = grid_bounds(keys);
    let gw = (maxx - minx).max(1e-3);
    let gh = (maxy - miny).max(1e-3);
    (avail.width() / gw).min(avail.height() / gh)
}

/// The lower arm of an ISO inverted-L Enter, given its upper-arm rect and the
/// grid scale; `None` for any other key (including the wide 2.25u ANSI Enter).
/// The arm is 1.25u wide, right-aligned, and drops one row pitch below the upper
/// arm so it sits flush with the home row — the two arms share a right edge and
/// are vertically contiguous.
pub fn iso_enter_lower(k: &VisualKey, rect: Rect, unit: f32) -> Option<Rect> {
    if k.cell.id != KeyId::Enter || k.cell.w >= 2.0 {
        return None;
    }
    Some(Rect::from_min_max(
        Pos2::new(rect.right() - 1.25 * unit, rect.bottom()),
        Pos2::new(rect.right(), rect.bottom() + ROW_PITCH * unit),
    ))
}

/// The 6 vertices of the closed inverted-L outline, from the upper-arm rect and
/// its `lower` arm — a single boundary so the Enter draws as one cap, not two.
fn iso_enter_outline(upper: Rect, lower: Rect) -> Vec<Pos2> {
    vec![
        upper.left_top(),
        upper.right_top(),
        lower.right_bottom(),
        lower.left_bottom(),
        Pos2::new(lower.left(), upper.bottom()),
        upper.left_bottom(),
    ]
}

/// The key hit by `pos`, if any. Both ISO Enter arms resolve to the one key.
pub fn hit_key(keys: &[VisualKey], rects: &[Rect], pos: Pos2, unit: f32) -> Option<usize> {
    keys.iter().zip(rects).position(|(k, r)| {
        r.contains(pos) || iso_enter_lower(k, *r, unit).is_some_and(|lower| lower.contains(pos))
    })
}

// ── Legends ────────────────────────────────────────────────────────────────

/// Human-readable language name for the layout selector.
pub fn language_name(l: KeyboardLayout) -> &'static str {
    match l {
        KeyboardLayout::US => "US",
        KeyboardLayout::CH => "Swiss",
        KeyboardLayout::IT => "Italian",
        KeyboardLayout::DE => "German",
        KeyboardLayout::FR => "French",
        KeyboardLayout::UK => "UK",
        KeyboardLayout::Unknown => "\u{2014}",
    }
}

fn variant_name(v: KeyVariant) -> &'static str {
    match v {
        KeyVariant::Ansi => "ANSI",
        KeyVariant::Iso => "ISO",
    }
}

/// Exhaustive US legend for a standard key. `Custom(_)` renders as a blank cap.
fn us_label(id: KeyId) -> &'static str {
    use KeyId::*;
    match id {
        Escape => "Esc",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        Backtick => "`",
        Digit1 => "1",
        Digit2 => "2",
        Digit3 => "3",
        Digit4 => "4",
        Digit5 => "5",
        Digit6 => "6",
        Digit7 => "7",
        Digit8 => "8",
        Digit9 => "9",
        Digit0 => "0",
        Minus => "-",
        Equals => "=",
        A => "A",
        B => "B",
        C => "C",
        D => "D",
        E => "E",
        F => "F",
        G => "G",
        H => "H",
        I => "I",
        J => "J",
        K => "K",
        L => "L",
        M => "M",
        N => "N",
        O => "O",
        P => "P",
        Q => "Q",
        R => "R",
        S => "S",
        T => "T",
        U => "U",
        V => "V",
        W => "W",
        X => "X",
        Y => "Y",
        Z => "Z",
        LeftBracket => "[",
        RightBracket => "]",
        Backslash => "\\",
        Semicolon => ";",
        Quote => "'",
        Comma => ",",
        Period => ".",
        Slash => "/",
        IsoExtra => "<",
        Tab => "Tab",
        CapsLock => "Caps",
        Enter => "Enter",
        Backspace => "Bksp",
        Space => "",
        LeftShift => "Shift",
        RightShift => "Shift",
        LeftCtrl => "Ctrl",
        RightCtrl => "Ctrl",
        LeftAlt => "Alt",
        RightAlt => "Alt",
        LeftSuper => "Super",
        RightSuper => "Super",
        Insert => "Ins",
        Delete => "Del",
        Home => "Home",
        End => "End",
        PageUp => "PgUp",
        PageDown => "PgDn",
        Up => "\u{2191}",
        Down => "\u{2193}",
        Left => "\u{2190}",
        Right => "\u{2192}",
        PrintScreen => "PrSc",
        ScrollLock => "ScrLk",
        Pause => "Pause",
        Menu => "Menu",
        NumLock => "Num",
        NumDiv => "/",
        NumMul => "*",
        NumSub => "-",
        NumAdd => "+",
        NumEnter => "Ent",
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        NumDot => ".",
        Custom(_) => "",
    }
}

/// Sparse per-language legend overrides. Only keys whose cap differs from US
/// are listed; everything else falls through to [`us_label`].
fn lang_override(lang: KeyboardLayout, id: KeyId) -> Option<&'static str> {
    use KeyId::*;
    match lang {
        // QWERTZ: Y and Z swap; a few punctuation keys carry umlauts.
        KeyboardLayout::CH | KeyboardLayout::DE => match id {
            Y => Some("Z"),
            Z => Some("Y"),
            Semicolon => Some(if matches!(lang, KeyboardLayout::DE) {
                "\u{00d6}" // Ö
            } else {
                "\u{00e9}" // é (Swiss)
            }),
            Quote => Some("\u{00e4}"),       // ä
            LeftBracket => Some("\u{00fc}"), // ü
            _ => None,
        },
        // AZERTY: A/Q and W/Z swap; M moves next to L.
        KeyboardLayout::FR => match id {
            Q => Some("A"),
            A => Some("Q"),
            W => Some("Z"),
            Z => Some("W"),
            Semicolon => Some("M"),
            M => Some(","),
            _ => None,
        },
        // Italian ISO: umlaut/accent punctuation differences.
        KeyboardLayout::IT => match id {
            Semicolon => Some("\u{00f2}"),   // ò
            Quote => Some("\u{00e0}"),       // à
            LeftBracket => Some("\u{00e8}"), // è
            _ => None,
        },
        // UK ISO differs only on a couple of symbol keys.
        KeyboardLayout::UK => match id {
            Backslash => Some("#"),
            IsoExtra => Some("\\"),
            _ => None,
        },
        KeyboardLayout::US | KeyboardLayout::Unknown => None,
    }
}

/// The legend text for a key under a given language.
pub fn key_label(lang: KeyboardLayout, id: KeyId) -> &'static str {
    lang_override(lang, id).unwrap_or_else(|| us_label(id))
}

// ── Layout selector option builder ──────────────────────────────────────────

/// The pure data a layout-selector renders: two combo boxes (variant hidden
/// when the device declares no ISO variant).
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutOptions {
    pub show_variant: bool,
    pub variant_labels: Vec<String>,
    pub variant_selected: usize,
    pub language_labels: Vec<String>,
    pub language_selected: usize,
}

/// Build the selector labels/selection from a status. Index 0 on each axis is
/// "Auto (detected: …)".
pub fn layout_options(s: &KeyboardLayoutStatus) -> LayoutOptions {
    let auto = |detected: String| {
        if detected == "\u{2014}" {
            "Auto".to_string()
        } else {
            format!("Auto (detected: {detected})")
        }
    };

    let mut language_labels = vec![auto(language_name(s.detected_language).to_string())];
    language_labels.extend(s.languages.iter().map(|l| language_name(*l).to_string()));
    let language_selected = match s.selection.language {
        None => 0,
        Some(l) => s
            .languages
            .iter()
            .position(|x| *x == l)
            .map(|i| i + 1)
            .unwrap_or(0),
    };

    let variant_labels = vec![
        format!("Auto (detected: {})", variant_name(s.variant)),
        "ANSI".to_string(),
        "ISO".to_string(),
    ];
    let variant_selected = match s.selection.variant {
        None => 0,
        Some(KeyVariant::Ansi) => 1,
        Some(KeyVariant::Iso) => 2,
    };

    LayoutOptions {
        show_variant: s.iso_supported,
        variant_labels,
        variant_selected,
        language_labels,
        language_selected,
    }
}

/// Map a variant combo index back to a selection value (`None` = Auto).
pub fn variant_from_index(i: usize) -> Option<KeyVariant> {
    match i {
        1 => Some(KeyVariant::Ansi),
        2 => Some(KeyVariant::Iso),
        _ => None,
    }
}

/// Map a language combo index back to a selection value (`None` = Auto).
pub fn language_from_index(s: &KeyboardLayoutStatus, i: usize) -> Option<KeyboardLayout> {
    i.checked_sub(1).and_then(|j| s.languages.get(j).copied())
}

// ── Painting ────────────────────────────────────────────────────────────────

/// Whether a cap fill is dark enough to want a light legend, by perceived
/// luminance (Rec. 601). A crude per-channel test mis-classes mid-dark blues.
fn is_dark(c: Color32) -> bool {
    let lum = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    lum < 128.0
}

/// Paint the keyboard. `colors` maps a key's `led_id` to its fill; `selected`
/// highlights one key; `language` picks the legends. Both ISO Enter arms are
/// drawn as one key.
pub fn draw_keyboard(
    ui: &egui::Ui,
    keys: &[VisualKey],
    rects: &[Rect],
    colors: &dyn Fn(u32) -> Color32,
    selected: Option<usize>,
    language: KeyboardLayout,
    unit: f32,
) {
    let painter = ui.painter();
    let rounding = 3.0;
    for (i, (k, r)) in keys.iter().zip(rects).enumerate() {
        let fill = colors(k.led_id);
        let stroke = if selected == Some(i) {
            Stroke::new(2.0, theme::CYAN)
        } else {
            Stroke::new(1.0, theme::hex(0x2a3446))
        };
        match iso_enter_lower(k, *r, unit) {
            // One connected inverted-L cap: fill both arms sharp-cornered so they
            // merge, then stroke a single outline around the whole shape.
            Some(lower) => {
                painter.rect_filled(*r, 0.0, fill);
                painter.rect_filled(lower, 0.0, fill);
                painter.add(egui::Shape::closed_line(
                    iso_enter_outline(*r, lower),
                    stroke,
                ));
            }
            None => {
                painter.rect_filled(*r, rounding, fill);
                painter.rect_stroke(*r, rounding, stroke, egui::StrokeKind::Middle);
            }
        }
        let label = key_label(language, k.cell.id);
        if !label.is_empty() {
            let text_col = if is_dark(fill) {
                theme::TEXT_MUT
            } else {
                theme::hex(0x0a0d13)
            };
            let font = theme::body((r.height() * 0.5).clamp(7.0, 12.0));
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                label,
                font,
                text_col,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::keyboard::{KeyCell, StandardLayout};

    fn keys_for(layout: StandardLayout) -> Vec<VisualKey> {
        layout
            .cells()
            .into_iter()
            .enumerate()
            .map(|(i, cell)| VisualKey {
                led_id: i as u32,
                remap_cid: None,
                cell,
            })
            .collect()
    }

    const ALL: [StandardLayout; 4] = [
        StandardLayout::Tkl,
        StandardLayout::TklIso,
        StandardLayout::FullSize,
        StandardLayout::FullSizeIso,
    ];

    fn rects_overlap(a: Rect, b: Rect) -> bool {
        let i = a.intersect(b);
        i.width() > 0.5 && i.height() > 0.5
    }

    #[test]
    fn key_rects_stay_within_avail() {
        let avail = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 300.0));
        for layout in ALL {
            let keys = keys_for(layout);
            for r in key_rects(&keys, avail, 2.0) {
                assert!(
                    avail.contains_rect(r.expand(0.01)),
                    "{layout:?} rect escaped avail"
                );
            }
        }
    }

    #[test]
    fn key_rects_preserve_cell_aspect_without_gap() {
        let avail = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(800.0, 300.0));
        for layout in ALL {
            let keys = keys_for(layout);
            let rects = key_rects(&keys, avail, 0.0);
            for (k, r) in keys.iter().zip(&rects) {
                // Uniform scale ⇒ screen aspect matches the cell aspect.
                let cell_ratio = k.cell.w / k.cell.h;
                let rect_ratio = r.width() / r.height();
                assert!(
                    (cell_ratio - rect_ratio).abs() < 0.02,
                    "{layout:?} aspect not preserved for {:?}",
                    k.cell.id
                );
            }
        }
    }

    #[test]
    fn key_rects_do_not_overlap() {
        let avail = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 320.0));
        for layout in ALL {
            let keys = keys_for(layout);
            let rects = key_rects(&keys, avail, 1.5);
            for i in 0..rects.len() {
                for j in (i + 1)..rects.len() {
                    assert!(
                        !rects_overlap(rects[i], rects[j]),
                        "{layout:?}: {:?} overlaps {:?}",
                        keys[i].cell.id,
                        keys[j].cell.id
                    );
                }
            }
        }
    }

    fn iso_enter_key() -> (VisualKey, Rect) {
        let cell = KeyCell {
            id: KeyId::Enter,
            col: 0.0,
            row: 0.0,
            w: 1.5,
            h: 1.0,
        };
        let k = VisualKey {
            led_id: 0,
            remap_cid: None,
            cell,
        };
        // 20px/unit ⇒ a 30×20 upper arm.
        (
            k,
            Rect::from_min_size(Pos2::new(100.0, 50.0), Vec2::new(30.0, 20.0)),
        )
    }

    /// The upper-arm rect is 30px wide for a 1.5u cell ⇒ 20px per grid unit.
    const ENTER_UNIT: f32 = 20.0;

    #[test]
    fn iso_enter_lower_shares_right_edge_and_is_contiguous() {
        let (k, upper) = iso_enter_key();
        let lower = iso_enter_lower(&k, upper, ENTER_UNIT).unwrap();
        assert!(
            (upper.right() - lower.right()).abs() < 1e-4,
            "share right edge"
        );
        assert!(
            (upper.bottom() - lower.top()).abs() < 1e-4,
            "vertically contiguous"
        );
        assert!(lower.width() < upper.width(), "lower arm is narrower");
    }

    #[test]
    fn iso_enter_outline_is_a_single_six_point_l() {
        let (k, upper) = iso_enter_key();
        let lower = iso_enter_lower(&k, upper, ENTER_UNIT).unwrap();
        let outline = iso_enter_outline(upper, lower);
        assert_eq!(outline.len(), 6, "one boundary, not two rectangles");
        // The right edge is straight (upper and lower share it).
        assert!((outline[1].x - outline[2].x).abs() < 1e-4);
        // The concave notch sits at the lower arm's left, level with the upper
        // arm's bottom.
        assert!((outline[4].x - lower.left()).abs() < 1e-4);
        assert!((outline[4].y - upper.bottom()).abs() < 1e-4);
    }

    #[test]
    fn iso_enter_lower_only_for_narrow_enter() {
        let (k, r) = iso_enter_key();
        assert!(iso_enter_lower(&k, r, ENTER_UNIT).is_some());
        // A wide ANSI Enter (and any other key) has no lower arm.
        let ansi = VisualKey {
            cell: KeyCell { w: 2.25, ..k.cell },
            ..k
        };
        assert!(iso_enter_lower(&ansi, r, ENTER_UNIT).is_none());
    }

    #[test]
    fn hit_key_treats_iso_enter_arms_as_one() {
        let keys = keys_for(StandardLayout::TklIso);
        let avail = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 320.0));
        let rects = key_rects(&keys, avail, 1.0);
        let unit = unit_for(&keys, avail);
        let enter_idx = keys.iter().position(|k| k.cell.id == KeyId::Enter).unwrap();
        let lower = iso_enter_lower(&keys[enter_idx], rects[enter_idx], unit).unwrap();
        // Both arms resolve to the Enter key.
        assert_eq!(
            hit_key(&keys, &rects, lower.center(), unit),
            Some(enter_idx)
        );
        assert_eq!(
            hit_key(&keys, &rects, rects[enter_idx].center(), unit),
            Some(enter_idx)
        );
    }

    #[test]
    fn us_label_is_exhaustive_for_standard_keys() {
        for cell in StandardLayout::FullSize.cells() {
            if matches!(cell.id, KeyId::Custom(_) | KeyId::Space) {
                continue;
            }
            assert!(
                !key_label(KeyboardLayout::US, cell.id).is_empty(),
                "no US legend for {:?}",
                cell.id
            );
        }
        assert_eq!(key_label(KeyboardLayout::US, KeyId::Custom(7)), "");
    }

    #[test]
    fn qwertz_swaps_y_and_z() {
        for lang in [KeyboardLayout::DE, KeyboardLayout::CH] {
            assert_eq!(key_label(lang, KeyId::Y), "Z");
            assert_eq!(key_label(lang, KeyId::Z), "Y");
        }
        // US keeps them.
        assert_eq!(key_label(KeyboardLayout::US, KeyId::Y), "Y");
    }

    #[test]
    fn azerty_swaps_a_and_q() {
        assert_eq!(key_label(KeyboardLayout::FR, KeyId::Q), "A");
        assert_eq!(key_label(KeyboardLayout::FR, KeyId::A), "Q");
    }

    #[test]
    fn layout_options_labels_and_selection() {
        let status = KeyboardLayoutStatus {
            keys: vec![],
            variant: KeyVariant::Iso,
            language: KeyboardLayout::CH,
            detected_language: KeyboardLayout::CH,
            selection: halod_shared::keyboard::KeyboardLayoutSelection {
                variant: None,
                language: Some(KeyboardLayout::IT),
            },
            iso_supported: true,
            languages: vec![KeyboardLayout::US, KeyboardLayout::CH, KeyboardLayout::IT],
        };
        let opts = layout_options(&status);
        assert!(opts.show_variant);
        assert_eq!(opts.language_labels[0], "Auto (detected: Swiss)");
        assert_eq!(opts.language_labels[1], "US");
        // IT is at languages[2] → combo index 3.
        assert_eq!(opts.language_selected, 3);
        assert_eq!(opts.variant_selected, 0, "variant is Auto");
        assert_eq!(opts.variant_labels[0], "Auto (detected: ISO)");
        // Round-trip the selection indices back to values.
        assert_eq!(variant_from_index(opts.variant_selected), None);
        assert_eq!(
            language_from_index(&status, opts.language_selected),
            Some(KeyboardLayout::IT)
        );
    }

    #[test]
    fn layout_options_unknown_detection_shows_plain_auto() {
        let status = KeyboardLayoutStatus {
            keys: vec![],
            variant: KeyVariant::Ansi,
            language: KeyboardLayout::US,
            detected_language: KeyboardLayout::Unknown,
            selection: Default::default(),
            iso_supported: false,
            languages: vec![KeyboardLayout::US, KeyboardLayout::CH],
        };
        let opts = layout_options(&status);
        assert_eq!(opts.language_labels[0], "Auto");
        assert!(
            !opts.show_variant,
            "no variant selector without ISO support"
        );
    }

    #[test]
    fn is_dark_uses_luminance_not_per_channel() {
        // The Keys-tab mapped-cap blue (0x28324a) reads as dark → light legend,
        // even though its red channel (0x28 = 40) defeats a naive `< 40` test.
        assert!(is_dark(theme::hex(0x28324a)));
        assert!(is_dark(theme::hex(0x141a24)));
        // Bright fills want a dark legend.
        assert!(!is_dark(Color32::from_rgb(255, 255, 0)));
        assert!(!is_dark(Color32::from_rgb(90, 209, 232)));
    }

    #[test]
    fn empty_keys_produce_no_rects() {
        let avail = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 100.0));
        assert!(key_rects(&[], avail, 1.0).is_empty());
    }
}
