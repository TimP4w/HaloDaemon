// SPDX-License-Identifier: GPL-3.0-or-later
//! `render_lcd_editor` — on-demand editor preview. Renders each widget of a
//! `CustomTemplateDef` to its own sprite bitmap against the device's canvas and
//! replies to the requesting client with an `lcd_editor_render` frame. The GUI
//! composites those sprites (position/scale/rotation) itself, so the editor
//! preview is pixel-identical to what the panel shows — the daemon is the single
//! source of truth for widget content.

use anyhow::Result;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::ipc::ClientHandle;
use crate::lcd::engine::custom;
use crate::registry::require_device_owned_id;
use crate::state::AppState;
use halod_shared::lcd_custom::{CustomTemplateDef, LcdEditorRender};

pub async fn render(
    device_id: String,
    def: CustomTemplateDef,
    known: HashMap<String, u64>,
    app: Arc<AppState>,
    client: ClientHandle,
) -> Result<()> {
    let device = require_device_owned_id(&device_id, &app).await?;
    let descriptor = device
        .as_lcd()
        .ok_or_else(|| anyhow::anyhow!("device does not support LCD: {device_id}"))?
        .lcd_descriptor();
    let (cw, ch) = (descriptor.width.max(1), descriptor.height.max(1));

    let sensors = app.snapshot_sensors().await;
    let images_dir = crate::config::lcd_images_dir();
    let render_device_id = device_id.clone();
    // `try_lock` inside the blocking closure: a render already in flight for
    // this device makes a queued second one pointless (the GUI re-requests
    // every ~200ms anyway), so drop the request rather than let
    // `spawn_blocking` tasks pile up behind a serialized lock.
    let result = tokio::task::spawn_blocking(move || {
        let Ok(mut session) = app.lcd.editor_session.try_lock() else {
            return None;
        };
        Some(custom::render_editor_sprites(
            &render_device_id,
            &def,
            cw,
            ch,
            &sensors,
            &images_dir,
            &known,
            &mut session,
        ))
    })
    .await?;
    let Some((sprites, signatures)) = result else {
        return Ok(());
    };

    let render = LcdEditorRender {
        device_id,
        canvas_w: cw,
        canvas_h: ch,
        sprites,
        signatures,
    };
    client.send_json(&json!({ "type": "lcd_editor_render", "data": render }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::lcd_custom::{WidgetDef, WidgetType};
    use std::collections::HashMap;

    fn text_widget(id: &str) -> WidgetDef {
        WidgetDef {
            id: id.to_string(),
            widget_type: WidgetType::Text,
            x: 0.5,
            y: 0.5,
            scale: 1.0,
            rotation: 0.0,
            color: None,
            font: None,
            params: HashMap::from([(
                "text".to_string(),
                halod_shared::types::EffectParamValue::Str("HI".to_string()),
            )]),
        }
    }

    #[test]
    fn renders_one_sprite_per_widget_with_nonzero_dims() {
        let def = CustomTemplateDef {
            widgets: vec![text_widget("a"), text_widget("b")],
            style: Default::default(),
        };
        let sensors = HashMap::new();
        let mut session = None;
        let (sprites, signatures) = custom::render_editor_sprites(
            "dev",
            &def,
            240,
            240,
            &sensors,
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        );
        assert_eq!(sprites.len(), 2);
        for s in &sprites {
            assert!(s.w > 0 && s.h > 0, "sprite {} has zero dims", s.id);
            assert!(!s.rgba_b64.is_empty());
        }
        assert_eq!(sprites[0].id, "a");
        assert_eq!(sprites[1].id, "b");
        assert_eq!(signatures.len(), 2);
    }

    #[test]
    fn known_signatures_skip_unchanged_widgets() {
        let def = CustomTemplateDef {
            widgets: vec![text_widget("a"), text_widget("b")],
            style: Default::default(),
        };
        let sensors = HashMap::new();
        let mut session = None;
        let (_, signatures) = custom::render_editor_sprites(
            "dev",
            &def,
            240,
            240,
            &sensors,
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        );
        let known: HashMap<String, u64> = signatures.into_iter().collect();
        let (sprites, signatures2) = custom::render_editor_sprites(
            "dev",
            &def,
            240,
            240,
            &sensors,
            std::path::Path::new("/tmp"),
            &known,
            &mut session,
        );
        assert!(sprites.is_empty(), "unchanged widgets shouldn't re-render");
        assert_eq!(signatures2.len(), 2);
    }

    #[test]
    fn stale_known_entry_rerenders_only_that_widget() {
        let def = CustomTemplateDef {
            widgets: vec![text_widget("a"), text_widget("b")],
            style: Default::default(),
        };
        let sensors = HashMap::new();
        let mut session = None;
        let (_, signatures) = custom::render_editor_sprites(
            "dev",
            &def,
            240,
            240,
            &sensors,
            std::path::Path::new("/tmp"),
            &HashMap::new(),
            &mut session,
        );
        let mut known: HashMap<String, u64> = signatures.into_iter().collect();
        known.insert("a".to_string(), 0);
        let (sprites, _) = custom::render_editor_sprites(
            "dev",
            &def,
            240,
            240,
            &sensors,
            std::path::Path::new("/tmp"),
            &known,
            &mut session,
        );
        assert_eq!(sprites.len(), 1);
        assert_eq!(sprites[0].id, "a");
    }
}
