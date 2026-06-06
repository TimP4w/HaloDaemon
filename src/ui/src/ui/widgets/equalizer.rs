use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::store::Store;
use halod_protocol::types::Equalizer;

pub struct EqualizerWidget {
    pub root: gtk::Box,
}

impl EqualizerWidget {
    pub fn build(device_id: &str, eq: &Equalizer, store: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(16)
            .build();

        // ── Preset selector ──────────────────────────────────────────────
        let preset_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .build();

        let preset_label = gtk::Label::builder()
            .label("Preset")
            .halign(gtk::Align::Start)
            .hexpand(true)
            .css_classes(["heading"])
            .build();

        let preset_model = gtk::StringList::new(
            &eq.presets.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(),
        );
        let preset_drop = gtk::DropDown::builder()
            .model(&preset_model)
            .selected(eq.selected_preset as u32)
            .build();

        preset_row.append(&preset_label);
        preset_row.append(&preset_drop);
        root.append(&preset_row);

        // ── Band sliders ─────────────────────────────────────────────────
        let bands_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .css_classes(["card"])
            .build();

        let bands_inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_start(16)
            .margin_end(16)
            .margin_top(16)
            .margin_bottom(16)
            .build();

        let bands_heading = gtk::Label::builder()
            .label("Equalizer Bands")
            .halign(gtk::Align::Start)
            .css_classes(["heading"])
            .build();
        bands_inner.append(&bands_heading);

        let bands_grid = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .homogeneous(true)
            .build();

        let mut band_scales: Vec<gtk::Scale> = Vec::new();
        for band in &eq.bands {
            let band_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .halign(gtk::Align::Center)
                .build();

            let adj = gtk::Adjustment::new(
                band.value as f64,
                band.min as f64,
                band.max as f64,
                band.step as f64,
                1.0,
                0.0,
            );
            let scale = gtk::Scale::builder()
                .adjustment(&adj)
                .orientation(gtk::Orientation::Vertical)
                .inverted(true)
                .draw_value(true)
                .value_pos(gtk::PositionType::Bottom)
                .height_request(160)
                .build();
            scale.set_digits(1);

            let lbl = gtk::Label::builder()
                .label(band.label.as_str())
                .css_classes(["caption", "dim-label"])
                .build();

            band_box.append(&scale);
            band_box.append(&lbl);
            bands_grid.append(&band_box);
            band_scales.push(scale);
        }

        bands_inner.append(&bands_grid);
        bands_card.append(&bands_inner);
        root.append(&bands_card);

        // ── Wire up preset changes ───────────────────────────────────────
        {
            let store = store.clone();
            let id = device_id.to_string();
            preset_drop.connect_selected_notify(move |drop| {
                store.dispatch(crate::commands::Command::SetEqPreset {
                    device_id: id.clone(),
                    preset_index: drop.selected() as usize,
                });
            });
        }

        // ── Wire up band changes ─────────────────────────────────────────
        for scale in &band_scales {
            let store = store.clone();
            let id = device_id.to_string();
            let all_scales = band_scales.clone();
            scale.connect_value_changed(move |_| {
                let values: Vec<f32> = all_scales.iter().map(|s| s.value() as f32).collect();
                store.dispatch(crate::commands::Command::SetEqBands {
                    device_id: id.clone(),
                    values,
                });
            });
        }

        Self { root }
    }
}

use crate::ui::capability_registry::CapabilityPanel;
use crate::state::AppState;
impl CapabilityPanel for EqualizerWidget {
    fn root_widget(&self) -> gtk::Widget { self.root.clone().upcast() }
    fn tab_label(&self) -> &'static str  { "Equalizer" }
    fn tab_icon(&self)  -> &'static str  { "multimedia-equalizer-symbolic" }
    fn tab_name(&self)  -> &'static str  { "equalizer" }
    fn update_live(&self, _state: &AppState) {}
}
