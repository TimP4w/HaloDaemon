// SPDX-License-Identifier: GPL-3.0-or-later
//! Device keyboard-layout commands.
use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use halod_shared::keyboard::KeyboardLayoutSelection;

/// Apply a keyboard's layout selection in place: update the live slot, persist
/// it, let the device re-apply any layout-dependent hardware state, then
/// broadcast. The device stays registered — its `KeyboardLayoutStatus` (and the
/// GUI legends/grid) recompute from the slot on the next serialize. Avoids the
/// full re-discovery a device drop would trigger.
pub async fn set_keyboard_layout(
    id: String,
    selection: KeyboardLayoutSelection,
    app: Arc<AppState>,
) -> Result<()> {
    let device = {
        let devices = app.device_registry.read().await;
        devices.iter().find(|d| d.id() == id).cloned()
    };
    let device = device.ok_or_else(|| anyhow!("device not found: {id}"))?;
    let slot = device
        .keyboard_layout_slot()
        .ok_or_else(|| anyhow!("device {id} does not support keyboard layout selection"))?;

    slot.set_selection(selection);

    {
        let mut cfg = app.config.write().await;
        // Auto/Auto is stored as absence (mirrors sensor_visibility).
        if selection.is_auto() {
            cfg.keyboard_layouts.remove(&id);
        } else {
            cfg.keyboard_layouts.insert(id.clone(), selection);
        }
        drop(cfg);
        app.request_config_save();
    }

    if let Some(kb) = device.as_keyboard_layout() {
        if let Err(e) = kb.apply_layout().await {
            log::warn!("[{id}] apply_layout failed: {e:#}");
        }
    }

    app.record_change(crate::application::bus::coordinator::Change::Device(id))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::test_support::MockDevice;
    use halod_shared::keyboard::KeyVariant;
    use halod_shared::types::KeyboardLayout;

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    #[tokio::test]
    async fn persists_and_applies_selection_without_dropping_device() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("kbd").with_keyboard_layout());
        app.device_registry.write().await.push(device.clone());

        let sel = KeyboardLayoutSelection {
            variant: Some(KeyVariant::Iso),
            language: Some(KeyboardLayout::CH),
        };
        set_keyboard_layout("kbd".into(), sel, app.clone())
            .await
            .unwrap();

        // Persisted…
        assert_eq!(
            app.config
                .read()
                .await
                .keyboard_layouts
                .get("kbd")
                .unwrap()
                .language,
            Some(KeyboardLayout::CH)
        );
        // …applied to the live slot in place…
        assert_eq!(
            device
                .keyboard_layout
                .as_ref()
                .unwrap()
                .selection()
                .language,
            Some(KeyboardLayout::CH)
        );
        // …and the device is still registered (no re-discovery drop).
        assert_eq!(app.device_registry.read().await.len(), 1);
    }

    #[tokio::test]
    async fn auto_selection_removes_entry() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("kbd").with_keyboard_layout());
        app.device_registry.write().await.push(device);
        app.config.write().await.keyboard_layouts.insert(
            "kbd".into(),
            KeyboardLayoutSelection {
                variant: Some(KeyVariant::Iso),
                language: None,
            },
        );

        set_keyboard_layout(
            "kbd".into(),
            KeyboardLayoutSelection::default(),
            app.clone(),
        )
        .await
        .unwrap();

        assert!(!app.config.read().await.keyboard_layouts.contains_key("kbd"));
    }

    #[tokio::test]
    async fn rejects_device_without_slot() {
        let app = make_app();
        app.device_registry
            .write()
            .await
            .push(Arc::new(MockDevice::new("plain")));

        let err = set_keyboard_layout(
            "plain".into(),
            KeyboardLayoutSelection {
                variant: Some(KeyVariant::Ansi),
                language: None,
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("does not support"));
    }
}
