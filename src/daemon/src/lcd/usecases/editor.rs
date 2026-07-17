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
    crate::lcd::usecases::templates::validate_template(&def)?;
    crate::lcd::usecases::templates::validate_template_catalog(&def, &app.registry)?;
    let device = require_device_owned_id(&device_id, &app).await?;
    let lcd = device
        .as_lcd()
        .ok_or_else(|| anyhow::anyhow!("device does not support LCD: {device_id}"))?;
    let descriptor = lcd.lcd_descriptor();
    lcd.lcd_state().set_editor_preview();
    lcd.lcd_state()
        .set_health(halod_shared::types::LcdHealth::Starting);
    let (cw, ch) = (descriptor.width.max(1), descriptor.height.max(1));

    if app
        .lcd
        .editor_rendering
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        )
        .is_err()
    {
        return Ok(());
    }
    let sensors = app.snapshot_sensors().await;
    let images_dir = crate::config::lcd_images_dir();
    let render_device_id = device_id.clone();
    let mut session = app.lcd.editor_session().take();
    custom::prepare_editor_session(&render_device_id, &def, &images_dir, &mut session)
        .gather_plugin_sprites(cw, ch, descriptor.shape.clone(), 0.0, &sensors, &app)
        .await;
    let render_def = def.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = custom::render_editor_sprites(
            &render_device_id,
            &render_def,
            cw,
            ch,
            descriptor.shape,
            &sensors,
            &images_dir,
            &known,
            &mut session,
        );
        (session, result)
    })
    .await;
    app.lcd
        .editor_rendering
        .store(false, std::sync::atomic::Ordering::Release);
    let result = match result {
        Ok((session, result)) => {
            *app.lcd.editor_session() = session;
            result
        }
        Err(error) => {
            lcd.lcd_state()
                .set_health(halod_shared::types::LcdHealth::Failed(error.to_string()));
            crate::ipc::broadcast_state(&app).await;
            return Err(error.into());
        }
    };
    lcd.lcd_state()
        .set_health(halod_shared::types::LcdHealth::Stable);
    crate::ipc::broadcast_state(&app).await;
    let (sprites, signatures) = result;

    let widgets = def
        .widgets
        .iter()
        .map(|widget| {
            let missing = app.registry.widget_descriptor(&widget.widget).is_none();
            let signature = signatures
                .iter()
                .find(|(id, _)| id == &widget.id)
                .map(|(_, signature)| *signature);
            halod_shared::lcd_custom::WidgetRenderState {
                id: widget.id.clone(),
                status: if missing {
                    halod_shared::lcd_custom::WidgetRenderStatus::Missing
                } else {
                    halod_shared::lcd_custom::WidgetRenderStatus::Ready
                },
                signature: (!missing).then_some(signature).flatten(),
            }
        })
        .collect();

    let render = LcdEditorRender {
        device_id,
        canvas_w: cw,
        canvas_h: ch,
        sprites,
        signatures,
        widgets,
    };
    client.send_json(&json!({ "type": "lcd_editor_render", "data": render }));
    Ok(())
}
