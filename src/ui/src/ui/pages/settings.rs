use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gio;
use gtk4 as gtk;
use libadwaita as adw;

use crate::service;
use crate::state::AppState;
use crate::store::Store;
use crate::ui::widgets::debug_dialog::open_system_debug_dialog;
use halod_protocol::types::DiscoveryPhase;

const THIRD_PARTY_LICENSES: &str =
    include_str!(concat!(env!("OUT_DIR"), "/third_party_licenses.txt"));

#[derive(Clone)]
pub struct SettingsPage {
    pub root: gtk::Box,
    // Live-updated widgets
    conn_status_lbl: gtk::Label,
    daemon_toggle_btn: gtk::Button,
    daemon_restart_btn: gtk::Button,
    daemon_connected: Rc<Cell<bool>>,
    log_view: gtk::TextView,
    log_scroll: gtk::ScrolledWindow,
    // Track whether we are scanning so we only show spinner while active.
    scanning: Rc<Cell<bool>>,
    scan_btn: gtk::Button,
    scan_spinner: gtk::Spinner,
    // Track initial load: init fields once from the first broadcast, not on every tick.
    initialized: Rc<Cell<bool>>,
    // User-controlled inputs — stored so we can init them on first broadcast.
    // They are NEVER updated after initialization (CLAUDE.md rule).
    fan_curve_switch: gtk::Switch,
    fan_curve_spin: gtk::SpinButton,
    canvas_switch: gtk::Switch,
    canvas_spin: gtk::SpinButton,
    lcd_switch: gtk::Switch,
    lcd_spin: gtk::SpinButton,
    log_level_drop: gtk::DropDown,
    close_to_tray_switch: gtk::Switch,
    // Config folder: read-only display whose value is owned by the daemon.
    config_dir_row: adw::ActionRow,
    config_dir_path: Rc<RefCell<PathBuf>>,
}

impl SettingsPage {
    pub fn new(ctx: &Store) -> Self {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_start(32)
            .margin_end(32)
            .margin_top(28)
            .margin_bottom(32)
            .build();

        // --- Application section ---
        let (application_box, close_to_tray_switch) = build_application_section(ctx.clone());
        content.append(&application_box);

        // --- Engines section ---
        let (
            engines_box,
            fan_curve_switch,
            fan_curve_spin,
            canvas_switch,
            canvas_spin,
            lcd_switch,
            lcd_spin,
        ) = build_engines_section(ctx.clone());
        content.append(&engines_box);

        // --- Devices section ---
        let (
            devices_box,
            conn_status_lbl,
            scan_btn,
            scan_spinner,
            scanning,
            daemon_toggle_btn,
            daemon_restart_btn,
            daemon_connected,
        ) = build_devices_section(ctx.clone());
        content.append(&devices_box);

        // --- Logging section ---
        let (logging_box, log_level_drop, log_view, log_scroll) =
            build_logging_section(ctx.clone());
        content.append(&logging_box);

        // --- Advanced section ---
        let config_dir_path = Rc::new(RefCell::new(PathBuf::new()));
        let (advanced_box, config_dir_row) = build_advanced_section(config_dir_path.clone());
        content.append(&advanced_box);

        // --- Debug section ---
        content.append(&build_debug_section(ctx.clone()));

        // --- About section ---
        content.append(&build_about_section());

        scroll.set_child(Some(&content));
        root.append(&scroll);

        SettingsPage {
            root,
            conn_status_lbl,
            daemon_toggle_btn,
            daemon_restart_btn,
            daemon_connected,
            log_view,
            log_scroll,
            scanning,
            scan_btn,
            scan_spinner,
            initialized: Rc::new(Cell::new(false)),
            fan_curve_switch,
            fan_curve_spin,
            canvas_switch,
            canvas_spin,
            lcd_switch,
            lcd_spin,
            log_level_drop,
            close_to_tray_switch,
            config_dir_row,
            config_dir_path,
        }
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Called on every state broadcast. Only read-only displays are updated here;
    /// user-controlled inputs are initialized once on the first broadcast, then left alone.
    pub fn update_live(&self, state: &AppState, connected: bool) {
        // Connection status label + daemon control buttons.
        self.daemon_connected.set(connected);
        if connected {
            self.conn_status_lbl.set_text("● Connected");
            self.conn_status_lbl.remove_css_class("disconnected");
            self.conn_status_lbl.add_css_class("connected");
            self.daemon_toggle_btn.set_label("Stop");
            self.daemon_toggle_btn.remove_css_class("suggested-action");
            self.daemon_toggle_btn.add_css_class("destructive-action");
            self.daemon_restart_btn.set_sensitive(true);
        } else {
            self.conn_status_lbl.set_text("○ Disconnected");
            self.conn_status_lbl.remove_css_class("connected");
            self.conn_status_lbl.add_css_class("disconnected");
            self.daemon_toggle_btn.set_label("Start");
            self.daemon_toggle_btn
                .remove_css_class("destructive-action");
            self.daemon_toggle_btn.add_css_class("suggested-action");
            self.daemon_restart_btn.set_sensitive(false);
        }

        // Log viewer — always refreshed.
        let buf = self.log_view.buffer();
        let mut text = String::new();
        for entry in &state.log_entries {
            text.push_str(&format!(
                "[{}] {}: {}\n",
                entry.level, entry.target, entry.message
            ));
        }
        // Check if user is at the bottom before overwriting text (which resets scroll position).
        let adj = self.log_scroll.vadjustment();
        let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 1.0;
        buf.set_text(&text);
        // Only auto-scroll if user was already at the bottom.
        if at_bottom {
            self.log_view
                .scroll_to_iter(&mut buf.end_iter(), 0.0, false, 0.0, 0.0);
        }

        // Config folder — read-only path reported by the daemon. Always refreshed
        // so the displayed path follows the daemon (single source of truth).
        if !state.config_dir.is_empty() {
            let path = PathBuf::from(&state.config_dir);
            self.config_dir_row.set_subtitle(&state.config_dir);
            *self.config_dir_path.borrow_mut() = path;
        }

        // Initialize user-controlled inputs only once.
        if self.initialized.get() {
            return;
        }
        self.initialized.set(true);

        let g = &state.global_config;

        // Fan curve engine row.
        self.fan_curve_switch.set_active(g.engine_fan_curve_enabled);
        self.fan_curve_spin
            .set_value(g.engine_fan_curve_tick_ms as f64);
        self.fan_curve_spin
            .set_sensitive(g.engine_fan_curve_enabled);

        // Canvas engine row.
        self.canvas_switch.set_active(g.engine_canvas_enabled);
        self.canvas_spin.set_value(g.engine_canvas_fps as f64);
        self.canvas_spin.set_sensitive(g.engine_canvas_enabled);

        // LCD engine row.
        self.lcd_switch.set_active(g.engine_lcd_enabled);
        self.lcd_spin.set_value(g.engine_lcd_fps as f64);
        self.lcd_spin.set_sensitive(g.engine_lcd_enabled);

        // Log level dropdown.
        let levels = ["error", "warn", "info", "debug", "trace"];
        let idx = levels
            .iter()
            .position(|&l| l == g.log_level.to_lowercase())
            .unwrap_or(2);
        self.log_level_drop.set_selected(idx as u32);
        self.close_to_tray_switch
            .set_active(state.global_config.close_to_tray);
    }

    /// Mark uninitialized so inputs are re-read on the next broadcast (e.g. after reconnect).
    pub fn reset_init(&self) {
        self.initialized.set(false);
    }

    pub fn on_scan_complete(&self, state: &AppState) {
        if !self.scanning.get() {
            return;
        }
        // Discovery broadcasts a Discovering frame at the start of the scan,
        // so without this guard the spinner stops on the *first* broadcast
        // tick after the click rather than when the scan actually finishes.
        if !matches!(
            state.discovery.phase,
            DiscoveryPhase::Complete | DiscoveryPhase::Error
        ) {
            return;
        }
        self.scanning.set(false);
        self.scan_spinner.stop();
        self.scan_btn.set_sensitive(true);
        self.scan_btn.set_label("Scan now");
    }
}

fn section_heading(title: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(title)
        .halign(gtk::Align::Start)
        .css_classes(["home-section-label"])
        .build()
}

// ──────────────────────────────────────────────────────────────────────────────
// Application section
// ──────────────────────────────────────────────────────────────────────────────

fn build_application_section(store: Store) -> (gtk::Box, gtk::Switch) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&section_heading("Application"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let row = adw::ActionRow::builder()
        .title("Close to tray")
        .subtitle("Hide the window instead of quitting when the close button is pressed")
        .activatable(false)
        .build();

    let sw = gtk::Switch::builder()
        .active(true)
        .valign(gtk::Align::Center)
        .build();

    sw.connect_active_notify(move |s| {
        store.dispatch(crate::commands::Command::SetUiConfig {
            close_to_tray: s.is_active(),
        });
    });

    row.add_suffix(&sw);
    list.append(&row);
    section.append(&list);
    (section, sw)
}

// ──────────────────────────────────────────────────────────────────────────────
// Engines section
// ──────────────────────────────────────────────────────────────────────────────

struct EngineRowConfig {
    adj: gtk::Adjustment,
    unit: &'static str,
    width_chars: i32,
    /// JSON field name sent with the numeric value ("tick_ms" or "fps").
    value_field: &'static str,
}

fn build_engine_row(
    title: &str,
    subtitle: &str,
    store: Store,
    engine_key: &'static str,
    cfg: EngineRowConfig,
) -> (adw::ActionRow, gtk::Switch, gtk::SpinButton) {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(false)
        .build();

    let spin = gtk::SpinButton::builder()
        .adjustment(&cfg.adj)
        .valign(gtk::Align::Center)
        .digits(0)
        .width_chars(cfg.width_chars)
        .build();

    let unit_lbl = gtk::Label::builder()
        .label(cfg.unit)
        .valign(gtk::Align::Center)
        .css_classes(["dim-label"])
        .build();

    let sw = gtk::Switch::builder()
        .active(true)
        .valign(gtk::Align::Center)
        .build();

    let spin_c = spin.clone();
    let store_sw = store.clone();
    sw.connect_active_notify(move |s| {
        spin_c.set_sensitive(s.is_active());
        store_sw.dispatch(crate::commands::Command::SetEngineConfig {
            engine: engine_key.to_string(),
            config: serde_json::json!({ "enabled": s.is_active() }),
        });
    });

    spin.connect_value_changed(move |s| {
        let mut config = serde_json::json!({});
        config[cfg.value_field] = serde_json::json!(s.value() as u64);
        store.dispatch(crate::commands::Command::SetEngineConfig {
            engine: engine_key.to_string(),
            config,
        });
    });

    row.add_suffix(&unit_lbl);
    row.add_suffix(&spin);
    row.add_suffix(&sw);

    (row, sw, spin)
}

fn build_engines_section(
    store: Store,
) -> (
    gtk::Box,
    gtk::Switch,
    gtk::SpinButton,
    gtk::Switch,
    gtk::SpinButton,
    gtk::Switch,
    gtk::SpinButton,
) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&section_heading("Engines"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let (fc_row, fc_sw, fc_spin) = build_engine_row(
        "Fan Curve",
        "How often fan speeds are recalculated. Lower = more responsive, higher = less CPU load.",
        store.clone(),
        "fan_curve",
        EngineRowConfig {
            adj: gtk::Adjustment::new(2000.0, 500.0, 60_000.0, 100.0, 1000.0, 0.0),
            unit: "ms",
            width_chars: 6,
            value_field: "tick_ms",
        },
    );
    list.append(&fc_row);

    let (cv_row, cv_sw, cv_spin) = build_engine_row(
        "Canvas",
        "Frame rate for ambient RGB lighting effects rendered by the canvas engine.",
        store.clone(),
        "canvas",
        EngineRowConfig {
            adj: gtk::Adjustment::new(20.0, 1.0, 60.0, 1.0, 5.0, 0.0),
            unit: "fps",
            width_chars: 4,
            value_field: "fps",
        },
    );
    list.append(&cv_row);

    let (lcd_row, lcd_sw, lcd_spin) = build_engine_row(
        "LCD",
        "Frame rate for dynamic content driven to LCD screens by the LCD engine.",
        store.clone(),
        "lcd",
        EngineRowConfig {
            adj: gtk::Adjustment::new(20.0, 1.0, 60.0, 1.0, 5.0, 0.0),
            unit: "fps",
            width_chars: 4,
            value_field: "fps",
        },
    );
    list.append(&lcd_row);

    section.append(&list);
    (section, fc_sw, fc_spin, cv_sw, cv_spin, lcd_sw, lcd_spin)
}

// ──────────────────────────────────────────────────────────────────────────────
// Devices section
// ──────────────────────────────────────────────────────────────────────────────

fn build_devices_section(
    store: Store,
) -> (
    gtk::Box,
    gtk::Label,
    gtk::Button,
    gtk::Spinner,
    Rc<Cell<bool>>,
    gtk::Button,
    gtk::Button,
    Rc<Cell<bool>>,
) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&section_heading("Devices"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // Daemon status + service control row.
    let status_row = adw::ActionRow::builder()
        .title("Daemon")
        .subtitle("Connection to the HaloDaemon daemon")
        .activatable(false)
        .build();

    let conn_lbl = gtk::Label::builder()
        .label("○ Disconnected")
        .css_classes(["dim-label", "disconnected"])
        .valign(gtk::Align::Center)
        .build();

    // Single toggle button: "Start" when disconnected, "Stop" when connected.
    let toggle_btn = gtk::Button::builder()
        .label("Start")
        .css_classes(["suggested-action"])
        .valign(gtk::Align::Center)
        .build();

    // Icon-only restart button — enabled only when connected.
    let restart_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Restart daemon")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();

    let daemon_connected = Rc::new(Cell::new(false));
    let daemon_connected_click = daemon_connected.clone();
    let ipc_toggle = store.ipc().clone();
    let ipc_restart = store.ipc().clone();

    toggle_btn.connect_clicked(move |_| {
        if daemon_connected_click.get() {
            service::stop_service(&ipc_toggle);
        } else {
            service::start_service();
        }
    });
    restart_btn.connect_clicked(move |_| service::restart_service(&ipc_restart));

    let btn_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();
    btn_box.append(&toggle_btn);
    btn_box.append(&restart_btn);

    status_row.add_suffix(&conn_lbl);
    status_row.add_suffix(&btn_box);
    list.append(&status_row);

    // Rediscover row.
    let rediscover_row = adw::ActionRow::builder()
        .title("Rediscover Devices")
        .subtitle("Scan USB, HID, hwmon, SMBus")
        .activatable(false)
        .build();

    let spinner = gtk::Spinner::builder()
        .width_request(16)
        .height_request(16)
        .valign(gtk::Align::Center)
        .visible(false)
        .build();

    let scan_btn = gtk::Button::builder()
        .label("Scan now")
        .valign(gtk::Align::Center)
        .build();

    let scanning = Rc::new(Cell::new(false));
    let scanning_click = scanning.clone();
    let spinner_click = spinner.clone();
    scan_btn.connect_clicked(move |btn| {
        if scanning_click.get() {
            return;
        }
        scanning_click.set(true);
        spinner_click.set_visible(true);
        spinner_click.start();
        btn.set_sensitive(false);
        btn.set_label("Scanning…");
        store.dispatch(crate::commands::Command::Rediscover);
    });

    rediscover_row.add_suffix(&spinner);
    rediscover_row.add_suffix(&scan_btn);
    list.append(&rediscover_row);

    section.append(&list);
    (
        section,
        conn_lbl,
        scan_btn,
        spinner,
        scanning,
        toggle_btn,
        restart_btn,
        daemon_connected,
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Logging section
// ──────────────────────────────────────────────────────────────────────────────

fn build_logging_section(
    store: Store,
) -> (gtk::Box, gtk::DropDown, gtk::TextView, gtk::ScrolledWindow) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&section_heading("Logging"));

    // Tab strip.
    let tab_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["linked"])
        .build();

    let level_tab = gtk::ToggleButton::builder()
        .label("Level")
        .active(true)
        .build();
    let logs_tab = gtk::ToggleButton::builder()
        .label("Logs")
        .group(&level_tab)
        .build();

    tab_box.append(&level_tab);
    tab_box.append(&logs_tab);

    // Level page.
    let level_page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();

    let level_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let level_row = adw::ActionRow::builder()
        .title("Log Level")
        .subtitle("Verbosity of the daemon log")
        .activatable(false)
        .build();

    let level_model = gtk::StringList::new(&["Error", "Warn", "Info", "Debug", "Trace"]);
    let level_drop = gtk::DropDown::builder()
        .model(&level_model)
        .selected(2)
        .valign(gtk::Align::Center)
        .build();

    let level_keys = ["error", "warn", "info", "debug", "trace"];
    level_drop.connect_selected_notify(move |d| {
        let idx = d.selected() as usize;
        if let Some(key) = level_keys.get(idx) {
            store.dispatch(crate::commands::Command::SetLogLevel {
                level: key.to_string(),
            });
        }
    });

    level_row.add_suffix(&level_drop);
    level_list.append(&level_row);
    level_page.append(&level_list);

    // Logs page.
    let logs_page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .visible(false)
        .build();

    let log_view = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::Word)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();

    let log_scroll = gtk::ScrolledWindow::builder()
        .height_request(200)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .css_classes(["card"])
        .child(&log_view)
        .build();

    logs_page.append(&log_scroll);

    section.append(&tab_box);
    section.append(&level_page);
    section.append(&logs_page);

    let level_page2 = level_page.clone();
    let logs_page2 = logs_page.clone();
    level_tab.connect_toggled(move |btn| {
        if btn.is_active() {
            level_page.set_visible(true);
            logs_page.set_visible(false);
        }
    });
    logs_tab.connect_toggled(move |btn| {
        if btn.is_active() {
            level_page2.set_visible(false);
            logs_page2.set_visible(true);
        }
    });

    (section, level_drop, log_view, log_scroll)
}

// ──────────────────────────────────────────────────────────────────────────────
// Advanced section
// ──────────────────────────────────────────────────────────────────────────────

fn build_advanced_section(config_dir_path: Rc<RefCell<PathBuf>>) -> (gtk::Box, adw::ActionRow) {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&section_heading("Advanced"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    // Config folder row.
    let config_row = adw::ActionRow::builder()
        .title("Config Folder")
        .subtitle("")
        .activatable(false)
        .build();

    let open_btn = gtk::Button::builder()
        .label("Open ↗")
        .valign(gtk::Align::Center)
        .build();

    open_btn.connect_clicked(move |btn| {
        let path = config_dir_path.borrow().clone();
        if path.as_os_str().is_empty() {
            return;
        }
        let file = gio::File::for_path(&path);
        let launcher = gtk::FileLauncher::new(Some(&file));
        if let Some(window) = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok()) {
            launcher.launch(Some(&window), gio::Cancellable::NONE, |_| {});
        }
    });

    config_row.add_suffix(&open_btn);
    list.append(&config_row);

    section.append(&list);
    (section, config_row)
}

// ──────────────────────────────────────────────────────────────────────────────
// Debug section
// ──────────────────────────────────────────────────────────────────────────────

fn build_debug_section(ctx: Store) -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&section_heading("Debug"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let row = adw::ActionRow::builder()
        .title("System &amp; device diagnostics")
        .subtitle("Show every attached HID device, what was matched, elevation state, PawnIO/udev presence")
        .activatable(false)
        .build();

    let open_btn = gtk::Button::builder()
        .label("Open…")
        .valign(gtk::Align::Center)
        .build();

    open_btn.connect_clicked(move |btn| {
        let parent = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        open_system_debug_dialog(parent.as_ref(), &ctx);
    });

    row.add_suffix(&open_btn);
    list.append(&row);

    section.append(&list);
    section
}

// ──────────────────────────────────────────────────────────────────────────────
// About section
// ──────────────────────────────────────────────────────────────────────────────

fn build_about_section() -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&section_heading("About"));

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    let row = adw::ActionRow::builder()
        .title("HaloDaemon")
        .subtitle(concat!(
            "v",
            env!("CARGO_PKG_VERSION"),
            " - Peripheral controller daemon"
        ))
        .activatable(false)
        .build();

    let icon = gtk::Image::builder()
        .icon_name("application-x-executable-symbolic")
        .pixel_size(32)
        .valign(gtk::Align::Center)
        .build();
    row.add_prefix(&icon);

    let credits_btn = gtk::Button::builder()
        .label("Credits…")
        .valign(gtk::Align::Center)
        .build();

    credits_btn.connect_clicked(|btn| {
        let dialog = build_about_dialog();
        let parent = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        dialog.present(parent.as_ref());
    });

    row.add_suffix(&credits_btn);
    list.append(&row);

    // Third-party licenses row
    let licenses_row = adw::ActionRow::builder()
        .title("Third-Party Licenses")
        .subtitle("Open source components used by HaloDaemon")
        .activatable(false)
        .build();

    let licenses_btn = gtk::Button::builder()
        .label("View…")
        .valign(gtk::Align::Center)
        .build();

    licenses_btn.connect_clicked(|btn| {
        let parent = btn.root().and_then(|r| r.downcast::<gtk::Window>().ok());
        open_licenses_dialog(parent.as_ref());
    });

    licenses_row.add_suffix(&licenses_btn);
    list.append(&licenses_row);

    let repo_row = adw::ActionRow::builder()
        .title("Repository")
        .subtitle("github.com/TimP4w/HaloDaemon")
        .activatable(false)
        .build();

    let repo_btn = gtk::Button::builder()
        .label("Open…")
        .valign(gtk::Align::Center)
        .build();

    repo_btn.connect_clicked(|_| {
        let _ = gtk::gio::AppInfo::launch_default_for_uri(
            "https://github.com/TimP4w/HaloDaemon",
            gtk::gio::AppLaunchContext::NONE,
        );
    });

    repo_row.add_suffix(&repo_btn);
    list.append(&repo_row);

    section.append(&list);
    section
}

fn open_licenses_dialog(parent: Option<&gtk::Window>) {
    let window = adw::Window::builder()
        .modal(true)
        .title("Third-Party Licenses")
        .default_width(700)
        .default_height(600)
        .build();
    if let Some(p) = parent {
        window.set_transient_for(Some(p));
    }

    let header = adw::HeaderBar::new();
    let close_btn = gtk::Button::with_label("Close");
    header.pack_start(&close_btn);

    let text_view = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(16)
        .right_margin(16)
        .build();
    text_view.buffer().set_text(THIRD_PARTY_LICENSES);

    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&text_view)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scroll));

    window.set_content(Some(&toolbar));

    {
        let window = window.clone();
        close_btn.connect_clicked(move |_| window.close());
    }

    window.present();
}

fn build_about_dialog() -> adw::AboutDialog {
    let dialog = adw::AboutDialog::builder()
        .application_name("HaloDaemon")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("TimP4w")
        .license_type(gtk::License::Gpl30)
        .website("https://github.com/TimP4w/HaloDaemon")
        .build();

    dialog.add_credit_section(
        Some("Protocol References"),
        &[
            "Solaar\nhttps://github.com/pwr-Solaar/Solaar",
            "OpenRGB\nhttps://gitlab.com/CalcProgrammer1/OpenRGB",
            "liquidctl\nhttps://github.com/liquidctl/liquidctl",
            "Linux kernel nzxt-smart2\nhttps://github.com/torvalds/linux",
            "OpenRazer\nhttps://github.com/openrazer/openrazer",
            "LibreHardwareMonitor\nhttps://github.com/LibreHardwareMonitor/LibreHardwareMonitor",
            "linux-arctis-manager\nhttps://github.com/elegos/Linux-Arctis-Manager",
            "g560-led\nhttps://github.com/mijoe/g560-led",
            "sennheiser-gsx-control",
        ],
    );

    dialog
}
