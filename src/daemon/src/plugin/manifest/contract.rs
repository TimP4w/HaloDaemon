// SPDX-License-Identifier: GPL-3.0-or-later
//! Machine-readable Lua ABI catalog for [`super::PLUGIN_API`].
//!
//! This is the source of truth for callback spelling and the Lua table shapes
//! crossing the host/plugin boundary. A shape names the Rust wire type where
//! one exists; its serde definition is authoritative for individual fields.

use crate::plugin::PLUGIN_API;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackUse {
    /// The callback is required when the corresponding capability is used.
    Capability,
    /// The callback is optional and the host has a documented fallback.
    Optional,
    /// The callback is used only by the named lifecycle or transport path.
    Lifecycle,
    /// The callback name contains the manifest effect id in place of `{id}`.
    EffectPattern,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallbackContract {
    pub name: &'static str,
    pub usage: CallbackUse,
    /// Lua parameters in call order. Device callbacks begin with `dev`.
    pub args: &'static str,
    /// Lua result shape; `nil` means no result is consumed.
    pub returns: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableContract {
    pub name: &'static str,
    pub fields: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginApiContract {
    pub version: u32,
    pub callbacks: &'static [CallbackContract],
    pub tables: &'static [TableContract],
}

macro_rules! callback {
    ($name:literal, $usage:ident, $args:literal, $returns:literal) => {
        CallbackContract {
            name: $name,
            usage: CallbackUse::$usage,
            args: $args,
            returns: $returns,
        }
    };
}

/// Callback names and signatures accepted by plugin API v1.
pub const CALLBACKS_V1: &[CallbackContract] = &[
    callback!("initialize", Lifecycle, "dev", "bool | InitTable | nil"),
    callback!("close", Lifecycle, "dev", "nil"),
    callback!("close_child", Lifecycle, "dev", "nil"),
    callback!("pre_scan", Lifecycle, "dev", "nil"),
    callback!("apply", Capability, "dev, RgbState", "nil"),
    callback!(
        "write_frame",
        Capability,
        "dev, zone_id, colors, led_ids",
        "nil"
    ),
    callback!("write_frame_batch", Optional, "dev, Frame[]", "nil"),
    callback!("read_status", Optional, "dev", "PollOutcome | nil"),
    callback!("event", Optional, "dev, Event", "PollOutcome | nil"),
    callback!(
        "event_source",
        Optional,
        "Event",
        "child_index | false | nil"
    ),
    callback!(
        "enumerate_controllers",
        Optional,
        "dev",
        "DetectedController[]"
    ),
    callback!("detect_accessories", Optional, "dev", "DetectedAccessory[]"),
    callback!(
        "write_ext_frame",
        Capability,
        "dev, channel_id, colors",
        "nil"
    ),
    callback!("get_duty", Capability, "dev", "integer"),
    callback!("set_duty", Capability, "dev, duty", "nil"),
    callback!("get_rpm", Optional, "dev", "integer | nil"),
    callback!("get_sensors", Capability, "dev", "Sensor[]"),
    callback!("fan_rpm", Capability, "dev, channel", "integer"),
    callback!("fan_duty", Capability, "dev, channel", "integer"),
    callback!("fan_controllable", Capability, "dev, channel", "bool"),
    callback!("set_fan_duty", Capability, "dev, channel, duty", "nil"),
    callback!(
        "lcd_stream_frame",
        Capability,
        "dev, bytes, width, height, rotation, raw, brightness",
        "nil"
    ),
    callback!("set_image", Capability, "dev, bytes, rotation", "nil"),
    callback!(
        "lcd_set_brightness",
        Capability,
        "dev, brightness, rotation",
        "nil"
    ),
    callback!(
        "lcd_set_rotation",
        Capability,
        "dev, brightness, degrees",
        "nil"
    ),
    callback!("lcd_reset", Capability, "dev", "nil"),
    callback!("set_dpi", Capability, "dev, dpi", "nil"),
    callback!("set_dpi_steps", Capability, "dev, steps", "nil"),
    callback!("set_choice", Capability, "dev, key, selected_index", "nil"),
    callback!("set_range", Capability, "dev, key, value", "nil"),
    callback!("get_booleans", Optional, "dev", "Boolean[]"),
    callback!("set_boolean", Capability, "dev, key, value", "nil"),
    callback!("trigger_action", Capability, "dev, key", "nil"),
    callback!("get_batteries", Optional, "dev", "Battery[]"),
    callback!(
        "connection_status",
        Optional,
        "dev",
        "ConnectionStatus | nil"
    ),
    callback!("get_equalizer", Capability, "dev", "Equalizer"),
    callback!("set_eq_preset", Capability, "dev, preset_index", "nil"),
    callback!("set_eq_bands", Capability, "dev, values", "nil"),
    callback!("start_pairing", Capability, "dev, timeout_seconds", "nil"),
    callback!("stop_pairing", Capability, "dev", "nil"),
    callback!("unpair", Capability, "dev, slot", "nil"),
    callback!("pairing_status", Capability, "dev", "PairingStatus"),
    callback!("switch_profile", Capability, "dev, slot", "nil"),
    callback!("restore_profile", Capability, "dev, slot", "nil"),
    callback!(
        "set_profile_enabled",
        Capability,
        "dev, slot, enabled",
        "nil"
    ),
    callback!(
        "onboard_profiles_status",
        Capability,
        "dev",
        "OnboardProfiles"
    ),
    callback!(
        "set_button_mapping",
        Capability,
        "dev, ButtonMapping",
        "nil"
    ),
    callback!("reset_button_mapping", Capability, "dev, cid", "nil"),
    callback!("reset_all_button_mappings", Capability, "dev", "nil"),
    callback!("key_remap_host_mode", Optional, "dev", "bool"),
    callback!(
        "render_effect_{id}",
        EffectPattern,
        "buffer, EffectCtx",
        "nil"
    ),
    callback!(
        "led_effect_{id}",
        EffectPattern,
        "leds, EffectCtx",
        "Color[]"
    ),
    callback!(
        "render_widget_{id}",
        EffectPattern,
        "buffer, width, height, time, dt, params, WidgetRenderCtx",
        "nil"
    ),
    callback!(
        "preview_widget_{id}",
        EffectPattern,
        "buffer, width, height, params, WidgetRenderCtx",
        "nil"
    ),
];

/// Host-created and callback-returned table shapes in plugin API v1.
pub const TABLES_V1: &[TableContract] = &[
    TableContract {
        name: "plugin",
        fields: "callback-name -> function; returned by main.lua",
    },
    TableContract {
        name: "dev",
        fields: "transport; match; status?; zones?; audio?",
    },
    TableContract {
        name: "dev.match",
        fields: "transport; bus?; addr?; vid?; pid?; index?; key?; name?; declared extra fields",
    },
    TableContract {
        name: "Event",
        fields: "transport='hid'; endpoint; report",
    },
    TableContract {
        name: "Frame",
        fields: "zone_id; colors; led_ids",
    },
    TableContract {
        name: "PollOutcome",
        fields: "child_index?; state_changed?; children_changed?; button_events={pressed?, released?}",
    },
    TableContract {
        name: "InitTable",
        fields: "ok?; model?; capabilities?; zones?; native_effects?; lcd?; chain?; accessories?; controls?; dpi?; fan?; key_remap?; keyboard?; ranges?; choices?",
    },
    TableContract {
        name: "DetectedController",
        fields: "index; name; device_type?; id?; key?; serial?; location?; extra?; zones?",
    },
    TableContract {
        name: "DetectedControllerZone",
        fields: "id?; name?; led_count",
    },
    TableContract {
        name: "DetectedAccessory",
        fields: "channel; accessory",
    },
    TableContract {
        name: "EffectCtx",
        fields: "time; dt; params; audio; frame; seed; zone { id, topology, led_count, device_id }; data(key); random(stream?); noise1d(x); noise2d(x, y); lerp_color(a, b, amount); gradient(stops, amount); srgb_to_linear(value); linear_to_srgb(value)",
    },
    TableContract {
        name: "WidgetRenderCtx",
        fields: "is_preview(); color(); data(key); local_time(); audio(); push_clip(x, y, width, height); pop_clip(); push_opacity(opacity); pop_opacity(); push_rotation(degrees, center_x, center_y); pop_rotation(); draw_media_art(buffer, x, y, width, height); measure_text(text, size); measure_text_box(text, width, style); ellipsize_text(text, size, max_width); draw_text(buffer, text, x, y, size, color?); draw_text_box(buffer, text, x, y, width, height, style, color?); fill_rect(buffer, x, y, width, height, color?); fill_rounded_rect(buffer, x, y, width, height, radius, color?); draw_line(buffer, x1, y1, x2, y2, color?, stroke_width?); draw_circle(buffer, cx, cy, radius, filled, color?, stroke_width?); draw_arc(buffer, cx, cy, radius, thickness, start_degrees, sweep_degrees, cap_radius, color?); draw_triangle(buffer, x1, y1, x2, y2, x3, y3, filled, color?, stroke_width?); draw_polyline(buffer, points, color?, stroke_width?); draw_polygon(buffer, points, filled, color?, stroke_width?); draw_image(buffer, name, x, y, width, height, fit, shape); draw_asset(buffer, name, x, y, width, height, fit)",
    },
];

pub const PLUGIN_API_CONTRACT: PluginApiContract = PluginApiContract {
    version: PLUGIN_API,
    callbacks: CALLBACKS_V1,
    tables: TABLES_V1,
};

pub fn active() -> &'static PluginApiContract {
    &PLUGIN_API_CONTRACT
}

/// Find a fixed callback name in the active contract. Effect callbacks use
/// manifest-derived names and are represented by their `{id}` patterns.
pub fn callback(name: &str) -> Option<&'static CallbackContract> {
    active().callbacks.iter().find(|entry| entry.name == name)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn active_catalog_is_versioned_and_has_unique_names() {
        assert_eq!(PLUGIN_API_CONTRACT.version, PLUGIN_API);
        let mut names = HashSet::new();
        for callback in PLUGIN_API_CONTRACT.callbacks {
            assert!(names.insert(callback.name), "duplicate {}", callback.name);
            assert!(!callback.args.is_empty());
            assert!(!callback.returns.is_empty());
        }
    }
}
