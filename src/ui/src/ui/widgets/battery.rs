use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use halod_protocol::types::{Battery, BatteryStatus};

pub struct BatteryWidget {
    pub root: gtk::Box,
    labels: Vec<(gtk::Label, gtk::Label)>,
}

impl BatteryWidget {
    pub fn build(batteries: &[Battery]) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .homogeneous(true)
            .build();

        let mut labels = Vec::new();
        for battery in batteries {
            let (card, level_label, status_label) = battery_card(battery);
            root.append(&card);
            labels.push((level_label, status_label));
        }

        Self { root, labels }
    }

    pub fn update_live(&self, batteries: &[Battery]) {
        for (i, battery) in batteries.iter().enumerate() {
            if let Some((level_label, status_label)) = self.labels.get(i) {
                level_label.set_text(&format!("{}%", battery.level));
                status_label.set_text(status_text(battery));
            }
        }
    }
}

fn battery_card(battery: &Battery) -> (gtk::Box, gtk::Label, gtk::Label) {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["card"])
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_start(10)
        .margin_end(10)
        .margin_top(8)
        .margin_bottom(8)
        .build();

    let title_lbl = gtk::Label::builder()
        .label(battery.label.as_str())
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .build();

    let level_lbl = gtk::Label::builder()
        .label(&format!("{}%", battery.level))
        .halign(gtk::Align::Start)
        .css_classes(["title-4"])
        .build();

    let status_lbl = gtk::Label::builder()
        .label(status_text(battery))
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .build();

    inner.append(&title_lbl);
    inner.append(&level_lbl);
    inner.append(&status_lbl);
    card.append(&inner);

    (card, level_lbl, status_lbl)
}

fn status_text(battery: &Battery) -> &'static str {
    match battery.status {
        BatteryStatus::Charging => "Charging",
        BatteryStatus::Unknown => "Unknown",
        BatteryStatus::Discharging => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn battery(status: BatteryStatus) -> Battery {
        Battery { key: "bat".into(), label: "Battery".into(), level: 80, status }
    }

    #[test]
    fn status_text_returns_charging() {
        assert_eq!(status_text(&battery(BatteryStatus::Charging)), "Charging");
    }

    #[test]
    fn status_text_returns_empty_when_discharging() {
        assert_eq!(status_text(&battery(BatteryStatus::Discharging)), "");
    }

    #[test]
    fn status_text_returns_unknown() {
        assert_eq!(status_text(&battery(BatteryStatus::Unknown)), "Unknown");
    }
}
