// SPDX-License-Identifier: GPL-3.0-or-later
//! The `App` struct: every per-screen UI state bundle, the daemon-state
//! caches, and the fields shared by both window backends (eframe and the
//! Linux wayland_hide loop). This is the composition root — it is the one
//! place allowed to depend on both `domain` and `ui`. Rendering (`draw`)
//! lives in `ui::root`.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use halod_shared::types::AppState;

use crate::domain::{
    self,
    state::{Page, Rename, Variant},
};
use crate::runtime::ipc::{self, CommandTx, UiRx};
use crate::ui;

pub struct App {
    pub(crate) ui: UiRx,
    /// Last daemon `AppState`, re-cloned only when its watch channel changes.
    pub(crate) state_cache: Arc<AppState>,
    /// LCD library filenames, re-cloned only when the watch channel changes.
    pub(crate) lcd_images_cache: Arc<Vec<String>>,
    /// Decoded plugin display assets, keyed by `ipc::plugin_asset_cache_key`.
    pub(crate) plugin_assets_cache: Arc<HashMap<String, Vec<u8>>>,
    pub(crate) cmd: CommandTx,
    pub(crate) entered: bool,
    /// Plugin ids already toasted as quarantined (disabled for an on-disk
    /// change), so each is alerted once until it leaves that state.
    pub(crate) quarantine_toasted: std::collections::HashSet<String>,
    /// Recommendation keys already toasted this session, so a "new plugin you
    /// may activate" alert fires once per hardware match (persisted dedup lives
    /// in `seen_tours`; this bridges the daemon roundtrip within a session).
    pub(crate) recommendation_toasted: std::collections::HashSet<String>,
    /// The user completed first-run onboarding this session, so it isn't
    /// reopened before the `seen_tours` roundtrip lands.
    pub(crate) onboarding_completed: bool,
    /// First-run onboarding wizard state (step, selections, scan timer).
    pub(crate) onboarding_ui: ui::screens::onboarding::OnboardingUi,
    pub(crate) show_hidden: bool,
    pub(crate) variant: Variant,
    /// Home device-list filter text (matches name or vendor).
    pub(crate) search: String,
    pub(crate) rename: Option<Rename>,
    /// Pending confirmation to unlink a chained device from the Home view.
    pub(crate) confirm_remove: Option<ui::screens::home::ConfirmRemove>,
    /// Open duplicate-device resolve dialog: every conflict group with the
    /// owner the user has picked to keep. `None` when the dialog is closed.
    pub(crate) conflict_resolve: Option<Vec<ui::screens::home::ConflictGroup>>,
    pub(crate) sensor_history: HashMap<String, VecDeque<f32>>,
    /// Rolling write-rate throughput (bytes/sec) per device id.
    pub(crate) write_rate_history: HashMap<String, VecDeque<f32>>,
    pub(crate) last_sample: f64,
    pub(crate) page: Page,
    pub(crate) device_ui: ui::screens::device::DeviceUi,
    pub(crate) canvas_ui: ui::screens::canvas::CanvasUi,
    pub(crate) lighting_ui: ui::screens::lighting::LightingUi,
    pub(crate) effect_designer_ui: ui::screens::effect_designer::DesignerUi,
    pub(crate) settings_ui: ui::screens::settings::SettingsUi,
    pub(crate) plugins_ui: ui::screens::plugins::PluginsUi,
    pub(crate) integrations_ui: ui::screens::integrations::IntegrationsUi,
    pub(crate) profile_ui: ui::screens::profile::ProfileUi,
    pub(crate) depcheck_ui: ui::screens::depcheck::DepCheckUi,
    pub(crate) tour: domain::tour::TourState,
    pub(crate) tray: domain::tray::Tray,
    pub(crate) toasts: ui::components::toast::Toasts,
    /// Open notification Details modal. Outlives the toast that spawned it.
    /// A notification Details modal's translated title and full error text.
    pub(crate) issue_details_modal: Option<(String, String)>,
    /// Repository-integrity episodes already notified during this GUI session.
    pub(crate) integrity_alert_notified: std::collections::HashSet<String>,
    /// Set by the tray "Quit" so a close request bypasses "close to tray" and
    /// actually exits instead of hiding the window.
    pub(crate) force_quit: Arc<AtomicBool>,
    /// A just-loaded named LCD template, consumed once by the open editor.
    pub(crate) pending_lcd_template: Option<(String, halod_shared::lcd_custom::CustomTemplateDef)>,
    /// Latest LCD editor render, cached like `lcd_images_cache`: most frames
    /// carry no update on the watch channel (a delta reply is ~one per
    /// 200ms), so this must survive frames where `has_changed()` is false
    /// rather than going back to `None`.
    pub(crate) lcd_editor_render_cache: Option<ipc::DecodedEditorRender>,
    pub(crate) depcheck_grace: ui::screens::depcheck::GraceState,
    /// Latest repo update-check result; persists like `lcd_editor_render_cache`.
    pub(crate) repo_updates_cache: Vec<halod_shared::types::RepoUpdateStatus>,
    /// Latest per-plugin update-check result; persists like `repo_updates_cache`.
    pub(crate) plugin_updates_cache: Vec<halod_shared::types::PluginUpdateStatus>,
    /// Remote branch lists keyed by repo URL, for the Add-repository picker.
    pub(crate) repo_branches_cache: std::collections::HashMap<String, Vec<String>>,
    /// Host serial ports, for the `serial_port` config-field dropdown.
    pub(crate) serial_ports_cache: Vec<halod_shared::types::SerialPortInfo>,
    /// Set once we've requested the serial-port list, so we ask the daemon at
    /// most once per session instead of every frame a picker is visible.
    pub(crate) serial_ports_requested: bool,
}

impl App {
    /// Classify a window-close request. Each backend detects `close_requested`
    /// its own way (eframe reads `ctx.input`; the Linux loop scans viewport
    /// commands + WM events) and passes it in. The tray "Quit" sets `force_quit`
    /// (and already sent `Shutdown`), so it always quits; the × button / WM
    /// close honours the `close_to_tray` config.
    pub fn close_action(&self, close_requested: bool) -> domain::lifecycle::CloseAction {
        domain::lifecycle::classify_close(
            close_requested,
            self.force_quit.load(Ordering::SeqCst),
            self.state_cache.gui.close_to_tray,
        )
    }

    /// Assemble the app from the already-created IPC channel, tray, and shared
    /// flags. Used by both the eframe backend and the Linux custom loop.
    pub fn new(
        ui: UiRx,
        cmd: CommandTx,
        tray: domain::tray::Tray,
        force_quit: Arc<AtomicBool>,
    ) -> Self {
        App {
            ui,
            state_cache: Arc::new(AppState::default()),
            lcd_images_cache: Arc::new(Vec::new()),
            plugin_assets_cache: Arc::new(HashMap::new()),
            cmd,
            entered: false,
            quarantine_toasted: std::collections::HashSet::new(),
            recommendation_toasted: std::collections::HashSet::new(),
            onboarding_completed: false,
            onboarding_ui: ui::screens::onboarding::OnboardingUi::default(),
            show_hidden: false,
            variant: Variant::Grid,
            search: String::new(),
            rename: None,
            confirm_remove: None,
            conflict_resolve: None,
            sensor_history: HashMap::new(),
            write_rate_history: HashMap::new(),
            last_sample: 0.0,
            page: Page::Home,
            device_ui: ui::screens::device::DeviceUi::default(),
            canvas_ui: ui::screens::canvas::CanvasUi::default(),
            lighting_ui: ui::screens::lighting::LightingUi::default(),
            effect_designer_ui: ui::screens::effect_designer::DesignerUi::new_effect(),
            settings_ui: ui::screens::settings::SettingsUi::default(),
            plugins_ui: ui::screens::plugins::PluginsUi::default(),
            integrations_ui: ui::screens::integrations::IntegrationsUi::default(),
            profile_ui: ui::screens::profile::ProfileUi::default(),
            depcheck_ui: ui::screens::depcheck::DepCheckUi::default(),
            tour: domain::tour::TourState::default(),
            tray,
            toasts: ui::components::toast::Toasts::default(),
            issue_details_modal: None,
            integrity_alert_notified: std::collections::HashSet::new(),
            force_quit,
            pending_lcd_template: None,
            lcd_editor_render_cache: None,
            depcheck_grace: ui::screens::depcheck::GraceState::default(),
            repo_updates_cache: Vec::new(),
            repo_branches_cache: std::collections::HashMap::new(),
            plugin_updates_cache: Vec::new(),
            serial_ports_cache: Vec::new(),
            serial_ports_requested: false,
        }
    }

    // Only the Wayland custom-hide loop drives this out-of-frame tray sync.
    #[cfg(target_os = "linux")]
    pub(crate) fn sync_tray_background(&mut self, ctx: &egui::Context) {
        let state = self.ui.state.borrow();
        self.tray.sync(ctx, &state);
    }
}
