use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use halod_protocol::types::DpiStatus;

const PAD: f64 = 28.0;
const CANVAS_H: i32 = 96;
const TRACK_Y_FRAC: f64 = 0.58;
const CIRCLE_R: f64 = 8.0;
const HIT_DIST: f64 = 20.0;
const MAX_STEPS: usize = 5;

// Piecewise linear scale: ≤4000 DPI takes 72% of track width.
const BREAK_DPI: f64 = 4000.0;
const BREAK_FRAC: f64 = 0.72;

pub struct DpiProfileWidget {
    pub root: gtk::Box,
    current_label: gtk::Label,
    current_dpi: Rc<RefCell<u16>>,
    da: gtk::DrawingArea,
    /// Editable step values, shared with the rebuild closure.
    step_vals: Rc<RefCell<Vec<u16>>>,
    /// Rebuilds the step-entry rows from `step_vals`.
    rebuild_fn: Rc<RefCell<Option<Box<dyn Fn()>>>>,
    /// Steps last reported by the device — used to detect a device-side change
    /// (e.g. a profile switch) without clobbering an in-progress user edit.
    last_daemon_steps: RefCell<Vec<u16>>,
}

impl DpiProfileWidget {
    pub fn build(device_id: &str, profile: &DpiStatus, store: &Store) -> Self {
        let avail: Rc<Vec<u16>> = Rc::new(
            profile.available_dpis.iter().copied().filter(|&v| v > 0).collect(),
        );
        let min_dpi = avail.first().copied().unwrap_or(100) as f64;
        let max_dpi = avail.last().copied().unwrap_or(25600) as f64;

        let current_dpi: Rc<RefCell<u16>> = Rc::new(RefCell::new(profile.current_dpi));
        let step_vals: Rc<RefCell<Vec<u16>>> = Rc::new(RefCell::new(profile.steps.clone()));
        let dragging: Rc<RefCell<Option<usize>>> = Rc::new(RefCell::new(None));

        // ── Outer wrapper with top spacing ────────────────────────────────────
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .margin_top(8)
            .build();

        // ── Card ──────────────────────────────────────────────────────────────
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .css_classes(["card"])
            .build();

        // Inner padding box so the card contents have breathing room.
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_top(14)
            .margin_bottom(14)
            .margin_start(14)
            .margin_end(14)
            .build();
        card.append(&inner);
        root.append(&card);

        // Current DPI label
        let current_label = gtk::Label::builder()
            .label(format!("{} DPI", profile.current_dpi))
            .halign(gtk::Align::Center)
            .css_classes(["title-4"])
            .build();
        inner.append(&current_label);

        // ── Drawing area ──────────────────────────────────────────────────────
        let da = gtk::DrawingArea::builder()
            .hexpand(true)
            .height_request(CANVAS_H)
            .cursor(
                &gtk::gdk::Cursor::from_name("ew-resize", None)
                    .unwrap_or_else(|| gtk::gdk::Cursor::from_name("default", None).unwrap()),
            )
            .build();

        {
            let sv = step_vals.clone();
            let cd = current_dpi.clone();
            da.set_draw_func(move |_da, cr, pw, ph| {
                draw_circles(cr, pw as f64, ph as f64, &sv.borrow(), *cd.borrow(), min_dpi, max_dpi);
            });
        }
        inner.append(&da);

        // ── Step entries (SpinButton + × per slot, + Add button) ─────────────
        let rows_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        inner.append(&rows_box);

        let spins: Rc<RefCell<Vec<gtk::SpinButton>>> = Rc::new(RefCell::new(Vec::new()));
        let rebuild_fn: Rc<RefCell<Option<Box<dyn Fn()>>>> = Rc::new(RefCell::new(None));

        *rebuild_fn.borrow_mut() = Some(Box::new({
            let rows_box  = rows_box.clone();
            let step_vals = step_vals.clone();
            let avail     = avail.clone();
            let da        = da.clone();
            let spins     = spins.clone();
            let rebuild_fn = rebuild_fn.clone();
            let device_id = device_id.to_string();
            let store     = store.clone();

            move || {
                while let Some(child) = rows_box.first_child() {
                    rows_box.remove(&child);
                }
                spins.borrow_mut().clear();

                let steps  = step_vals.borrow().clone();
                let av_min = *avail.first().unwrap_or(&100) as f64;
                let av_max = *avail.last().unwrap_or(&25600) as f64;

                for (i, &dpi) in steps.iter().enumerate() {
                    let slot_box = gtk::Box::builder()
                        .orientation(gtk::Orientation::Horizontal)
                        .spacing(2)
                        .build();

                    let adj = gtk::Adjustment::new(dpi as f64, av_min, av_max, 50.0, 400.0, 0.0);
                    let spin = gtk::SpinButton::builder()
                        .adjustment(&adj)
                        .numeric(true)
                        .width_chars(6)
                        .build();

                    {
                        let sv  = step_vals.clone();
                        let av  = avail.clone();
                        let da  = da.clone();
                        spin.connect_value_changed(move |sp| {
                            let snapped = snap_to_step(sp.value(), &av);
                            if sv.borrow().get(i).copied() == Some(snapped) { return; }
                            if let Some(slot) = sv.borrow_mut().get_mut(i) {
                                *slot = snapped;
                            }
                            da.queue_draw();
                        });
                    }

                    let rm_btn = gtk::Button::builder()
                        .icon_name("list-remove-symbolic")
                        .css_classes(["flat", "circular"])
                        .tooltip_text("Remove step")
                        .build();
                    {
                        let sv  = step_vals.clone();
                        let da  = da.clone();
                        let rfn = rebuild_fn.clone();
                        rm_btn.connect_clicked(move |_| {
                            let mut v = sv.borrow_mut();
                            if v.len() > 1 && i < v.len() { v.remove(i); }
                            drop(v);
                            da.queue_draw();
                            if let Some(f) = rfn.borrow().as_ref() { f(); }
                        });
                    }

                    slot_box.append(&spin);
                    slot_box.append(&rm_btn);
                    rows_box.append(&slot_box);
                    spins.borrow_mut().push(spin);
                }

                // Add button
                let add_btn = gtk::Button::builder()
                    .icon_name("list-add-symbolic")
                    .css_classes(["flat", "circular"])
                    .tooltip_text("Add step")
                    .sensitive(steps.len() < MAX_STEPS)
                    .build();
                {
                    let sv  = step_vals.clone();
                    let av  = avail.clone();
                    let da  = da.clone();
                    let rfn = rebuild_fn.clone();
                    add_btn.connect_clicked(move |_| {
                        let mut v = sv.borrow_mut();
                        if v.len() < MAX_STEPS {
                            let cur_max = v.iter().copied().max().unwrap_or(1600);
                            let new_val = av.iter().copied().find(|&x| x > cur_max).unwrap_or(cur_max);
                            v.push(new_val);
                            v.sort();
                        }
                        drop(v);
                        da.queue_draw();
                        if let Some(f) = rfn.borrow().as_ref() { f(); }
                    });
                }
                rows_box.append(&add_btn);

                // Apply button
                let apply = gtk::Button::builder()
                    .label("Apply")
                    .css_classes(["suggested-action"])
                    .halign(gtk::Align::End)
                    .hexpand(true)
                    .build();
                {
                    let sv    = step_vals.clone();
                    let dev   = device_id.clone();
                    let store = store.clone();
                    apply.connect_clicked(move |_| {
                        let steps: Vec<u32> = sv.borrow().iter().map(|&v| v as u32).collect();
                        store.dispatch(crate::commands::Command::SetDpiSteps {
                            device_id: dev.clone(),
                            steps,
                        });
                    });
                }
                rows_box.append(&apply);
            }
        }));

        if let Some(f) = rebuild_fn.borrow().as_ref() { f(); }

        // ── Canvas gestures ───────────────────────────────────────────────────

        let click = gtk::GestureClick::new();
        {
            let sv   = step_vals.clone();
            let drag = dragging.clone();
            let da2  = da.clone();
            click.connect_pressed(move |_, _, mx, _| {
                let w   = da2.width() as f64;
                let hit = hit_circle(&sv.borrow(), mx, w, min_dpi, max_dpi);
                *drag.borrow_mut() = hit;
            });
        }
        {
            let drag = dragging.clone();
            click.connect_released(move |_, _, _, _| { *drag.borrow_mut() = None; });
        }
        da.add_controller(click);

        let motion = gtk::EventControllerMotion::new();
        {
            let sv    = step_vals.clone();
            let drag  = dragging.clone();
            let spins = spins.clone();
            let avail = avail.clone();
            let da2   = da.clone();
            motion.connect_motion(move |_, mx, _| {
                let Some(idx) = *drag.borrow() else { return };
                let w       = da2.width() as f64;
                let raw     = screen_to_dpi(mx, w, min_dpi, max_dpi);
                let snapped = snap_to_step(raw, &avail);
                if let Some(slot) = sv.borrow_mut().get_mut(idx) { *slot = snapped; }
                if let Some(sp) = spins.borrow().get(idx) { sp.set_value(snapped as f64); }
                da2.queue_draw();
            });
        }
        {
            let drag = dragging.clone();
            motion.connect_leave(move |_| { *drag.borrow_mut() = None; });
        }
        da.add_controller(motion);

        Self {
            root,
            current_label,
            current_dpi,
            da,
            step_vals: step_vals.clone(),
            rebuild_fn: rebuild_fn.clone(),
            last_daemon_steps: RefCell::new(profile.steps.clone()),
        }
    }

    pub fn update_current_dpi(&self, dpi: u16) {
        *self.current_dpi.borrow_mut() = dpi;
        self.current_label.set_label(&format!("{dpi} DPI"));
        self.da.queue_draw();
    }

    /// Refresh from a daemon broadcast. Updates the active-DPI highlight, and —
    /// only when the *device* reports different steps (e.g. the active profile
    /// changed) — rebuilds the step rows. Comparing against the last daemon
    /// steps (not the editable values) avoids clobbering an in-progress edit.
    pub fn update_live(&self, profile: &DpiStatus) {
        self.update_current_dpi(profile.current_dpi);
        if *self.last_daemon_steps.borrow() != profile.steps {
            *self.last_daemon_steps.borrow_mut() = profile.steps.clone();
            *self.step_vals.borrow_mut() = profile.steps.clone();
            if let Some(f) = self.rebuild_fn.borrow().as_ref() {
                f();
            }
            self.da.queue_draw();
        }
    }
}

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw_circles(
    cr: &cairo::Context,
    w: f64,
    h: f64,
    steps: &[u16],
    current_dpi: u16,
    min_dpi: f64,
    max_dpi: f64,
) {
    let track_y = h * TRACK_Y_FRAC;

    // Track line
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.set_line_width(2.0);
    cr.move_to(PAD, track_y);
    cr.line_to(w - PAD, track_y);
    cr.stroke().ok();

    // Break marker at 4000 DPI
    if max_dpi > BREAK_DPI {
        let bx = dpi_to_screen(BREAK_DPI, w, min_dpi, max_dpi);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.07);
        cr.set_line_width(1.0);
        cr.move_to(bx, 6.0);
        cr.line_to(bx, h);
        cr.stroke().ok();
    }

    // Axis tick labels
    let interval = nice_interval((max_dpi - min_dpi) / 6.0);
    let mut tick = (min_dpi / interval).ceil() * interval;
    while tick <= max_dpi + 0.5 {
        let tx = dpi_to_screen(tick, w, min_dpi, max_dpi);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.10);
        cr.set_line_width(1.0);
        cr.move_to(tx, track_y + 3.0);
        cr.line_to(tx, track_y + 7.0);
        cr.stroke().ok();
        let lbl = format_dpi(tick as u16);
        cr.set_font_size(8.0);
        cr.select_font_face("sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
        if let Ok(ext) = cr.text_extents(&lbl) {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.22);
            cr.move_to(tx - ext.width() / 2.0, h - 1.0);
            cr.show_text(&lbl).ok();
        }
        tick += interval;
    }

    // Circles
    for &dpi in steps {
        let x         = dpi_to_screen(dpi as f64, w, min_dpi, max_dpi);
        let is_active = dpi == current_dpi;
        let (r, g, b) = if is_active { (0.40, 0.78, 1.0) } else { (0.55, 0.55, 0.62) };

        // Glow for active step
        if is_active {
            let glow = cairo::RadialGradient::new(x, track_y, 0.0, x, track_y, CIRCLE_R * 2.8);
            glow.add_color_stop_rgba(0.0, r, g, b, 0.45);
            glow.add_color_stop_rgba(1.0, r, g, b, 0.0);
            let _ = cr.set_source(&glow);
            cr.arc(x, track_y, CIRCLE_R * 2.8, 0.0, 2.0 * PI);
            cr.fill().ok();
        }

        // Circle fill
        cr.set_source_rgba(r, g, b, if is_active { 1.0 } else { 0.8 });
        cr.arc(x, track_y, CIRCLE_R, 0.0, 2.0 * PI);
        cr.fill().ok();

        // White inner dot for inactive steps (gives depth)
        if !is_active {
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.25);
            cr.arc(x, track_y, CIRCLE_R * 0.38, 0.0, 2.0 * PI);
            cr.fill().ok();
        }

        // DPI label above circle
        let label = format_dpi(dpi);
        cr.set_font_size(9.5);
        cr.select_font_face(
            "sans",
            cairo::FontSlant::Normal,
            if is_active { cairo::FontWeight::Bold } else { cairo::FontWeight::Normal },
        );
        if let Ok(ext) = cr.text_extents(&label) {
            cr.set_source_rgba(1.0, 1.0, 1.0, if is_active { 1.0 } else { 0.65 });
            cr.move_to(x - ext.width() / 2.0, track_y - CIRCLE_R - 5.0);
            cr.show_text(&label).ok();
        }
    }
}

// ── Coordinate transforms (unchanged) ────────────────────────────────────────

fn dpi_to_screen(dpi: f64, w: f64, min: f64, max: f64) -> f64 {
    let track = w - PAD * 2.0;
    let t = if max <= BREAK_DPI {
        (dpi - min) / (max - min).max(1.0)
    } else if dpi <= BREAK_DPI {
        (dpi - min) / (BREAK_DPI - min).max(1.0) * BREAK_FRAC
    } else {
        BREAK_FRAC + (dpi - BREAK_DPI) / (max - BREAK_DPI).max(1.0) * (1.0 - BREAK_FRAC)
    };
    PAD + t.clamp(0.0, 1.0) * track
}

fn screen_to_dpi(sx: f64, w: f64, min: f64, max: f64) -> f64 {
    let t = (sx - PAD) / (w - PAD * 2.0).max(1.0);
    let t = t.clamp(0.0, 1.0);
    if max <= BREAK_DPI || t <= BREAK_FRAC {
        let lo_max = BREAK_DPI.min(max);
        min + (t / BREAK_FRAC.max(0.001)) * (lo_max - min)
    } else {
        BREAK_DPI + ((t - BREAK_FRAC) / (1.0 - BREAK_FRAC).max(0.001)) * (max - BREAK_DPI)
    }
}

fn snap_to_step(dpi: f64, avail: &[u16]) -> u16 {
    if avail.is_empty() { return dpi.round() as u16; }
    let target = dpi as u16;
    avail.iter().copied()
        .min_by_key(|&v| (v as i32 - target as i32).unsigned_abs())
        .unwrap_or(target)
}

fn hit_circle(steps: &[u16], mx: f64, w: f64, min: f64, max: f64) -> Option<usize> {
    steps.iter().enumerate()
        .map(|(i, &v)| (i, (dpi_to_screen(v as f64, w, min, max) - mx).abs()))
        .filter(|(_, d)| *d < HIT_DIST)
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
        .map(|(i, _)| i)
}

fn format_dpi(dpi: u16) -> String {
    if dpi >= 1000 {
        let k = dpi as f64 / 1000.0;
        if k == k.floor() { format!("{}K", k as u16) } else { format!("{k:.1}K") }
    } else {
        dpi.to_string()
    }
}

fn nice_interval(raw: f64) -> f64 {
    if raw <= 0.0 { return 1000.0; }
    let magnitude = 10f64.powf(raw.log10().floor());
    let n = raw / magnitude;
    let nice = if n < 1.5 { 1.0 } else if n < 3.5 { 2.0 } else if n < 7.5 { 5.0 } else { 10.0 };
    nice * magnitude
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── format_dpi ────────────────────────────────────────────────────────────

    #[test]
    fn format_dpi_below_1000_is_plain_number() {
        assert_eq!(format_dpi(800), "800");
        assert_eq!(format_dpi(400), "400");
    }

    #[test]
    fn format_dpi_exact_thousands_use_k_suffix() {
        assert_eq!(format_dpi(1000), "1K");
        assert_eq!(format_dpi(4000), "4K");
        assert_eq!(format_dpi(16000), "16K");
    }

    #[test]
    fn format_dpi_fractional_k_shows_one_decimal() {
        assert_eq!(format_dpi(1600), "1.6K");
        assert_eq!(format_dpi(2400), "2.4K");
    }

    // ── snap_to_step ─────────────────────────────────────────────────────────

    #[test]
    fn snap_to_step_empty_avail_rounds_input() {
        assert_eq!(snap_to_step(800.4, &[]), 800);
    }

    #[test]
    fn snap_to_step_picks_nearest_value() {
        let avail = [400_u16, 800, 1600, 3200, 6400];
        assert_eq!(snap_to_step(500.0, &avail), 400);
        assert_eq!(snap_to_step(1100.0, &avail), 800);
        assert_eq!(snap_to_step(2000.0, &avail), 1600);
        assert_eq!(snap_to_step(5000.0, &avail), 6400);
    }

    #[test]
    fn snap_to_step_exact_match() {
        let avail = [800_u16, 1600, 3200];
        assert_eq!(snap_to_step(1600.0, &avail), 1600);
    }

    // ── dpi_to_screen / screen_to_dpi roundtrip ───────────────────────────────

    #[test]
    fn dpi_screen_roundtrip_below_break() {
        let (w, min, max) = (400.0, 400.0, 8000.0);
        for dpi in [400.0_f64, 800.0, 2000.0, 4000.0] {
            let sx = dpi_to_screen(dpi, w, min, max);
            let back = screen_to_dpi(sx, w, min, max);
            assert!((back - dpi).abs() < 1.0, "dpi={dpi} → sx={sx} → back={back}");
        }
    }

    #[test]
    fn dpi_screen_roundtrip_above_break() {
        let (w, min, max) = (400.0, 400.0, 8000.0);
        for dpi in [5000.0_f64, 6400.0, 8000.0] {
            let sx = dpi_to_screen(dpi, w, min, max);
            let back = screen_to_dpi(sx, w, min, max);
            assert!((back - dpi).abs() < 1.0, "dpi={dpi} → sx={sx} → back={back}");
        }
    }

    #[test]
    fn dpi_to_screen_clamps_below_min() {
        let sx = dpi_to_screen(0.0, 400.0, 400.0, 8000.0);
        assert_eq!(sx, PAD);
    }

    // ── nice_interval ─────────────────────────────────────────────────────────

    #[test]
    fn nice_interval_zero_returns_fallback() {
        assert_eq!(nice_interval(0.0), 1000.0);
    }

    #[test]
    fn nice_interval_rounds_to_nice_number() {
        assert_eq!(nice_interval(1000.0), 1000.0); // n=1.0 → 1 * 1000
        assert_eq!(nice_interval(2000.0), 2000.0); // n=2.0 → 2 * 1000
        assert_eq!(nice_interval(500.0),  500.0);  // n=5.0 → 5 * 100
        assert_eq!(nice_interval(800.0),  1000.0); // n=8.0 → 10 * 100
    }

    // ── hit_circle ───────────────────────────────────────────────────────────

    #[test]
    fn hit_circle_returns_none_when_no_steps() {
        assert!(hit_circle(&[], 200.0, 400.0, 400.0, 8000.0).is_none());
    }

    #[test]
    fn hit_circle_returns_index_when_within_hit_dist() {
        let steps = [1600_u16];
        let w = 400.0_f64;
        let center = dpi_to_screen(1600.0, w, 400.0, 8000.0);
        assert_eq!(hit_circle(&steps, center, w, 400.0, 8000.0), Some(0));
    }

    #[test]
    fn hit_circle_returns_none_when_far_from_all() {
        let steps = [1600_u16];
        let w = 400.0_f64;
        let center = dpi_to_screen(1600.0, w, 400.0, 8000.0);
        assert!(hit_circle(&steps, center + HIT_DIST + 1.0, w, 400.0, 8000.0).is_none());
    }
}
