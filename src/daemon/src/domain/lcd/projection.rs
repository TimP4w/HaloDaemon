// SPDX-License-Identifier: GPL-3.0-or-later
//! Projects LCD runtime state into its retained bus representation.

use crate::application::state::AppState;
use crate::config::Config;
use halod_shared::types::EffectParamValue;
use std::collections::HashMap;

pub async fn project(
    app: &AppState,
    cfg: &Config,
    templates: HashMap<String, String>,
    params: HashMap<String, HashMap<String, EffectParamValue>>,
) -> halod_shared::types::LcdState {
    let mut lcd = app.lcd.snapshot(&app.registry, templates, params).await;
    lcd.config = cfg.lcd.clone();
    lcd
}
