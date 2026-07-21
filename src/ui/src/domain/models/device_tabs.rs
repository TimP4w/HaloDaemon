// SPDX-License-Identifier: GPL-3.0-or-later
//! Capability → device-page tab derivation. Pure data: which tabs a device
//! shows and in what order, derived entirely from its reported capabilities.

use halod_shared::types::{DeviceCapability, DeviceType, WireDevice};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum TabKind {
    Devices,
    Lighting,
    Chains,
    Cooling,
    Lcd,
    Equalizer,
    Keys,
    Performance,
    Controls,
    Onboard,
    Pairing,
    Info,
}

pub struct Tab {
    pub kind: TabKind,
}

fn has(dev: &WireDevice, pred: impl Fn(&DeviceCapability) -> bool) -> bool {
    dev.capabilities.iter().any(pred)
}

/// Resolve tab index by remembered TabKind, falling back to clamped index.
pub fn realign_tab(tabs: &[Tab], remembered: Option<TabKind>, current: usize) -> usize {
    if let Some(kind) = remembered {
        if let Some(i) = tabs.iter().position(|t| t.kind == kind) {
            return i;
        }
    }
    if current >= tabs.len() {
        0
    } else {
        current
    }
}

/// The ordered tab set for a device, derived from its capabilities + type.
pub fn tabs_for(dev: &WireDevice) -> Vec<Tab> {
    let mut tabs = Vec::new();
    let mut push = |kind| tabs.push(Tab { kind });

    if has(dev, |c| matches!(c, DeviceCapability::Children(_))) {
        push(TabKind::Devices);
    }
    // A device whose every channel is chainable has nothing to paint directly:
    // its content is composed from chain links in the Chains tab.
    if !super::device::plain_channels(dev).is_empty() {
        push(TabKind::Lighting);
    }
    if has(
        dev,
        |c| matches!(c, DeviceCapability::Lighting(r) if r.descriptor.channels.iter().any(super::device::is_chainable)),
    ) {
        push(TabKind::Chains);
    }
    if has(dev, |c| matches!(c, DeviceCapability::Cooling(_))) {
        push(TabKind::Cooling);
    }
    if has(dev, |c| matches!(c, DeviceCapability::Lcd(_))) {
        push(TabKind::Lcd);
    }
    if has(dev, |c| matches!(c, DeviceCapability::Equalizer(_))) {
        push(TabKind::Equalizer);
    }
    if has(dev, |c| matches!(c, DeviceCapability::KeyRemap(_))) {
        push(TabKind::Keys);
    }
    if has(dev, |c| matches!(c, DeviceCapability::Dpi(_))) {
        push(TabKind::Performance);
    }
    if has(dev, |c| {
        matches!(
            c,
            DeviceCapability::Choice(_)
                | DeviceCapability::Range(_)
                | DeviceCapability::Boolean(_)
                | DeviceCapability::Action(_)
        )
    }) {
        push(TabKind::Controls);
    }
    if has(dev, |c| matches!(c, DeviceCapability::OnboardProfiles(_))) {
        push(TabKind::Onboard);
    }
    if has(dev, |c| matches!(c, DeviceCapability::Pairing(_))) {
        push(TabKind::Pairing);
    }
    push(TabKind::Info);
    tabs
}

/// User-facing translated label for a tab, matched on the `TabKind` enum. The
/// key-remap tab reads "Buttons" on a mouse and "Keys" otherwise.
pub fn tab_label(kind: TabKind, dev: &WireDevice) -> std::borrow::Cow<'static, str> {
    match kind {
        TabKind::Devices => t!("devtabs.tab_devices"),
        TabKind::Lighting => t!("devtabs.tab_lighting"),
        TabKind::Chains => t!("devtabs.tab_chains"),
        TabKind::Cooling => t!("devtabs.tab_cooling"),
        TabKind::Lcd => t!("devtabs.tab_lcd"),
        TabKind::Equalizer => t!("devtabs.tab_equalizer"),
        TabKind::Keys if matches!(dev.device_type, DeviceType::Mouse) => t!("devtabs.tab_buttons"),
        TabKind::Keys => t!("devtabs.tab_keys"),
        TabKind::Performance => t!("devtabs.tab_performance"),
        TabKind::Controls => t!("devtabs.tab_controls"),
        TabKind::Onboard => t!("devtabs.tab_onboard"),
        TabKind::Pairing => t!("devtabs.tab_pairing"),
        TabKind::Info => t!("devtabs.tab_info"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{
        LightingChannel, LightingDescriptor, LightingDivision, LightingStatus, ZoneTopology,
    };

    fn division_channel() -> LightingChannel {
        LightingChannel {
            id: "ch0".into(),
            name: "Header 1".into(),
            topology: ZoneTopology::Linear,
            leds: vec![],
            color_order: Default::default(),
            division: LightingDivision::Divisible {
                max_leds: 300,
                segments: vec![],
            },
            visibility: Default::default(),
        }
    }

    fn dev(ty: DeviceType, caps: Vec<DeviceCapability>) -> WireDevice {
        WireDevice {
            device_type: ty,
            capabilities: caps,
            ..Default::default()
        }
    }

    fn rgb_cap() -> DeviceCapability {
        lighting_cap(vec![plain_channel()])
    }

    fn lighting_cap(channels: Vec<halod_shared::types::LightingChannel>) -> DeviceCapability {
        DeviceCapability::Lighting(LightingStatus {
            descriptor: LightingDescriptor {
                channels,
                native_effects: vec![],
            },
            state: None,
            channel_transforms: Default::default(),
        })
    }

    fn plain_channel() -> halod_shared::types::LightingChannel {
        halod_shared::types::LightingChannel {
            id: "ring".into(),
            name: "Ring".into(),
            topology: halod_shared::types::ZoneTopology::Linear,
            leds: vec![],
            color_order: Default::default(),
            division: halod_shared::types::LightingDivision::Indivisible,
            visibility: Default::default(),
        }
    }

    fn kinds(d: &WireDevice) -> Vec<TabKind> {
        tabs_for(d).iter().map(|t| t.kind).collect()
    }

    #[test]
    fn info_tab_is_always_present() {
        assert_eq!(kinds(&dev(DeviceType::Other, vec![])), vec![TabKind::Info]);
    }

    #[test]
    fn rgb_capability_adds_lighting_first() {
        assert_eq!(
            kinds(&dev(DeviceType::Keyboard, vec![rgb_cap()])),
            vec![TabKind::Lighting, TabKind::Info]
        );
    }

    #[test]
    fn hub_lists_devices_before_lighting() {
        let d = dev(
            DeviceType::Hub,
            vec![DeviceCapability::Children(vec![]), rgb_cap()],
        );
        assert_eq!(
            kinds(&d),
            vec![TabKind::Devices, TabKind::Lighting, TabKind::Info]
        );
    }

    #[test]
    fn keyremap_tab_kind_is_the_same_but_label_depends_on_device_type() {
        let remap = DeviceCapability::KeyRemap(halod_shared::types::KeyRemapStatus {
            buttons: vec![],
            mappings: vec![],
            requires_host_mode: false,
            host_mode_active: false,
        });
        let mouse = dev(DeviceType::Mouse, vec![remap.clone()]);
        let keyboard = dev(DeviceType::Keyboard, vec![remap]);
        // Same tab kind for both...
        assert!(kinds(&mouse).contains(&TabKind::Keys));
        assert!(kinds(&keyboard).contains(&TabKind::Keys));
        // ...but the translated label differs (default `en` locale).
        assert_eq!(tab_label(TabKind::Keys, &mouse), "Buttons");
        assert_eq!(tab_label(TabKind::Keys, &keyboard), "Keys");
    }

    #[test]
    fn battery_has_no_tab_and_shows_in_header() {
        // Battery is a header chip, never a tab.
        let batt = DeviceCapability::Battery(vec![]);
        assert_eq!(
            kinds(&dev(DeviceType::Headset, vec![batt.clone()])),
            vec![TabKind::Info]
        );
        assert_eq!(
            kinds(&dev(DeviceType::Mouse, vec![batt])),
            vec![TabKind::Info]
        );
    }

    #[test]
    fn selected_tab_follows_kind_when_tab_set_shifts() {
        // Hub before adding any chain link: no children → no "Devices" tab.
        let before = tabs_for(&dev(
            DeviceType::Hub,
            vec![DeviceCapability::Lighting(LightingStatus {
                descriptor: LightingDescriptor {
                    channels: vec![division_channel()],
                    native_effects: vec![],
                },
                state: None,
                channel_transforms: Default::default(),
            })],
        ));
        let chains_idx = before
            .iter()
            .position(|t| t.kind == TabKind::Chains)
            .unwrap();

        // After adding a link a "Devices" tab appears at the front, shifting
        // every index by one. The selection must still resolve to Chains.
        let after = tabs_for(&dev(
            DeviceType::Hub,
            vec![
                DeviceCapability::Children(vec![]),
                DeviceCapability::Lighting(LightingStatus {
                    descriptor: LightingDescriptor {
                        channels: vec![division_channel()],
                        native_effects: vec![],
                    },
                    state: None,
                    channel_transforms: Default::default(),
                }),
            ],
        ));
        let new_chains_idx = realign_tab(&after, Some(TabKind::Chains), chains_idx);
        assert_eq!(after[new_chains_idx].kind, TabKind::Chains);
        assert_ne!(
            new_chains_idx, chains_idx,
            "index should have shifted, proving kind-tracking (not index) kept the tab"
        );

        // When the remembered kind disappears, fall back to the clamped index.
        let no_chains = tabs_for(&dev(DeviceType::Keyboard, vec![rgb_cap()]));
        let idx = realign_tab(&no_chains, Some(TabKind::Chains), 5);
        assert_eq!(idx, 0);
    }

    #[test]
    fn requested_cooling_kind_selects_the_cooling_tab() {
        // A device with both Lighting (first) and Cooling tabs.
        let d = dev(
            DeviceType::Other,
            vec![rgb_cap(), DeviceCapability::Cooling(Default::default())],
        );
        let tabs = tabs_for(&d);
        // Fresh state defaults to index 0 (Lighting); remembering Cooling wins.
        let idx = realign_tab(&tabs, Some(TabKind::Cooling), 0);
        assert_eq!(tabs[idx].kind, TabKind::Cooling);
        assert_ne!(idx, 0);
    }

    #[test]
    fn segments_tab_appears_only_when_divisible_channels_present() {
        let rgb_no_chains = rgb_cap();
        assert!(!kinds(&dev(DeviceType::Hub, vec![rgb_no_chains])).contains(&TabKind::Chains));

        let d = dev(
            DeviceType::Hub,
            vec![lighting_cap(vec![plain_channel(), division_channel()])],
        );
        let tabs = kinds(&d);
        assert!(
            tabs.contains(&TabKind::Chains),
            "Chains tab missing: {tabs:?}"
        );
        // Chains comes after Lighting
        let li = tabs.iter().position(|&k| k == TabKind::Lighting).unwrap();
        let ci = tabs.iter().position(|&k| k == TabKind::Chains).unwrap();
        assert!(ci > li, "Chains should be after Lighting");
    }

    #[test]
    fn a_purely_chainable_device_gets_chains_but_no_empty_lighting_tab() {
        let d = dev(
            DeviceType::Hub,
            vec![lighting_cap(vec![division_channel()])],
        );
        let tabs = kinds(&d);
        assert!(tabs.contains(&TabKind::Chains));
        assert!(!tabs.contains(&TabKind::Lighting), "{tabs:?}");
    }
}
