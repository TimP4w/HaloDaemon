#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use super::pages::{
    AppRulesPage, CanvasPage, CoolingPage, DevicePage, HomePage, LightingPage, SettingsPage,
};
use super::sidebar::Sidebar;
use super::widgets::ProfileSwitcher;
use crate::service;
use crate::store::{NavTarget, Store};
use crate::ui::capability_registry::CapabilityRegistry;

#[derive(Clone)]
pub struct MainWindow {
    pub window: adw::ApplicationWindow,
    pub toast_overlay: adw::ToastOverlay,
    pub sidebar: Sidebar,
    pub home_page: HomePage,
    pub canvas_page: CanvasPage,
    pub cooling_page: CoolingPage,
    pub lighting_page: LightingPage,
    pub settings_page: SettingsPage,
    pub app_rules_page: AppRulesPage,
    pub device_page: DevicePage,
    pub store: Store,
    profile_switcher: ProfileSwitcher,
    content_stack: gtk::Stack,
    daemon_overlay: gtk::Box,
}

impl MainWindow {
    pub fn new(app: &adw::Application, store: Store, registry: Rc<CapabilityRegistry>) -> Self {
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("HaloDaemon")
            .icon_name("halod")
            .default_width(1100)
            .default_height(700)
            .build();

        let toast_overlay = adw::ToastOverlay::new();

        let content_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .build();

        let home_page = HomePage::new(&store);
        let device_page = DevicePage::new(&store, registry);
        let canvas_page = CanvasPage::new(&store);
        let cooling_page = CoolingPage::new(&store);
        let lighting_page = LightingPage::new(&store);
        let settings_page = SettingsPage::new(&store);
        let app_rules_page = AppRulesPage::new(&store);
        let profile_switcher = ProfileSwitcher::new(&store);

        content_stack.add_named(&home_page.root, Some("home"));
        content_stack.add_named(&device_page.root, Some("device"));
        content_stack.add_named(&canvas_page.root, Some("canvas"));
        content_stack.add_named(&cooling_page.root, Some("cooling"));
        content_stack.add_named(&lighting_page.root, Some("lighting"));
        content_stack.add_named(settings_page.widget(), Some("settings"));
        content_stack.add_named(&app_rules_page.root, Some("app_rules"));
        content_stack.set_visible_child_name("home");

        let header = adw::HeaderBar::builder()
            .show_start_title_buttons(false)
            .show_end_title_buttons(true)
            .build();

        // Search bar lives in the header, collapsed by default and revealed
        // with Ctrl+F while on the home page. It wraps the home page's search
        // entry, which drives the live device filter.
        let search_bar = gtk::SearchBar::builder().show_close_button(true).build();
        search_bar.set_child(Some(&home_page.search_entry));
        search_bar.connect_entry(&home_page.search_entry);

        // Clearing the entry when the bar collapses resets the filter.
        let search_entry_clear = home_page.search_entry.clone();
        search_bar.connect_search_mode_enabled_notify(move |sb| {
            if !sb.is_search_mode() {
                search_entry_clear.set_text("");
            }
        });

        // Escape in the entry collapses the bar (which clears it via the notify above).
        // Guard against re-entry: `stop-search` also fires while the entry is being unmapped
        // during the SearchBar's hide animation. Calling `set_search_mode(false)` again at
        // that point stalls the revealer transition and the bar appears to linger ~1s.
        let search_bar_esc = search_bar.clone();
        home_page.search_entry.connect_stop_search(move |_| {
            if search_bar_esc.is_search_mode() {
                search_bar_esc.set_search_mode(false);
            }
        });

        let device_title_lbl = gtk::Label::builder()
            .css_classes(["device-page-title"])
            .build();

        let back_btn = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .css_classes(["flat"])
            .visible(false)
            .build();
        header.pack_start(&back_btn);
        header.pack_end(&profile_switcher.root);

        // Home is the initial page, so start with the search bar mounted.
        header.set_title_widget(Some(&search_bar));

        let header_back = header.clone();
        let stack_back = content_stack.clone();
        let back_btn_back = back_btn.clone();
        let search_bar_back = search_bar.clone();
        back_btn.connect_clicked(move |_| {
            header_back.set_title_widget(Some(&search_bar_back));
            back_btn_back.set_visible(false);
            stack_back.set_visible_child_name("home");
        });

        let sidebar = Sidebar::new(&store);

        // Keep sidebar active state in sync with the stack's visible child.
        // Also swap the header title widget: canvas controls when on canvas, nothing otherwise.
        // Whenever we leave the device page, hide the back button — otherwise it sticks around
        // when the user clicks Home/Canvas/Settings in the sidebar.
        let sidebar_sync = sidebar.clone();
        let header_sync = header.clone();
        let canvas_header = canvas_page.header_box.clone();
        let search_bar_sync = search_bar.clone();
        let back_btn_sync = back_btn.clone();
        content_stack.connect_visible_child_notify(move |stack| {
            if let Some(name) = stack.visible_child_name() {
                sidebar_sync.set_active_page(name.as_str());
                if name == "canvas" {
                    header_sync.set_title_widget(Some(&canvas_header));
                } else if name == "home" {
                    header_sync.set_title_widget(Some(&search_bar_sync));
                }
                if name != "device" {
                    back_btn_sync.set_visible(false);
                }
            }
        });

        // Wire navigation — resolves NavTarget to actual stack navigation.
        let dp = device_page.clone();
        let cs = content_stack.clone();
        let header_n = header.clone();
        let title_lbl = device_title_lbl.clone();
        let back_btn_n = back_btn.clone();
        let store_nav = store.clone();
        store.set_nav(move |target| match target {
            NavTarget::Device(id) => {
                let s = store_nav.state();
                if let Some(dev) = s.devices.iter().find(|d| d.id == id) {
                    let sensors = s.all_sensors();
                    dp.show_device(dev, &s.devices, &s.fan_curves, &s.preset_curves, &sensors);
                    title_lbl.set_text(&dev.name);
                    header_n.set_title_widget(Some(&title_lbl));
                    back_btn_n.set_visible(true);
                    cs.set_visible_child_name("device");
                }
            }
            NavTarget::Home => {
                cs.set_visible_child_name("home");
            }
            NavTarget::Canvas => {
                cs.set_visible_child_name("canvas");
            }
            NavTarget::Cooling => {
                cs.set_visible_child_name("cooling");
            }
            NavTarget::Lighting => {
                cs.set_visible_child_name("lighting");
            }
            NavTarget::AppRules => {
                cs.set_visible_child_name("app_rules");
            }
            NavTarget::Settings => {
                cs.set_visible_child_name("settings");
            }
        });

        // Wire toast. adw::Toast titles render Pango markup, so the severity
        // tint is applied inline rather than via a CSS class (Toast is not a
        // Widget and has no add_css_class).
        let toast_overlay_t = toast_overlay.clone();
        store.set_toast(move |n| {
            use halod_protocol::types::NotificationSeverity;
            let body = if n.title.is_empty() {
                glib::markup_escape_text(&n.message).to_string()
            } else if n.message.is_empty() {
                glib::markup_escape_text(&n.title).to_string()
            } else {
                format!(
                    "{}: {}",
                    glib::markup_escape_text(&n.title),
                    glib::markup_escape_text(&n.message)
                )
            };
            let (title, timeout, priority) = match n.severity {
                // Sticky + High priority gives errors a Dismiss button.
                NotificationSeverity::Error => (
                    format!("<span color=\"#ff7a72\"><b>Error</b></span>  {body}"),
                    0u32,
                    adw::ToastPriority::High,
                ),
                NotificationSeverity::Warning => (
                    format!("<span color=\"#f0b350\"><b>Warning</b></span>  {body}"),
                    8u32,
                    adw::ToastPriority::Normal,
                ),
                NotificationSeverity::Info => (body, 5u32, adw::ToastPriority::Normal),
            };
            let toast = adw::Toast::builder()
                .title(title)
                .timeout(timeout)
                .priority(priority)
                .build();
            toast_overlay_t.add_toast(toast);
        });

        let main_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();

        main_box.append(&sidebar.root);

        // Blocking overlay shown when the daemon is unreachable. The status page
        // explains *why* the UI is unresponsive (daemon is offline, trying to
        // reconnect) rather than just dimming the screen silently.
        let daemon_spinner = gtk::Spinner::builder()
            .width_request(28)
            .height_request(28)
            .build();
        daemon_spinner.start();

        let daemon_status = adw::StatusPage::builder()
            .icon_name("network-offline-symbolic")
            .title("Daemon offline")
            .description("Trying to reconnect to the HaloDaemon daemon…")
            .child(&daemon_spinner)
            .build();

        let start_daemon_btn = gtk::Button::builder()
            .label("Start daemon")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .margin_top(16)
            .build();
        start_daemon_btn.connect_clicked(|_| service::start_service());

        let daemon_overlay = gtk::Box::builder()
            .css_classes(["daemon-overlay"])
            .halign(gtk::Align::Fill)
            .valign(gtk::Align::Fill)
            .orientation(gtk::Orientation::Vertical)
            .build();
        daemon_overlay.append(&daemon_status);
        daemon_overlay.append(&start_daemon_btn);

        let content_area = gtk::Overlay::new();
        content_area.set_child(Some(&content_stack));
        content_area.add_overlay(&daemon_overlay);

        let content_toolbar = adw::ToolbarView::builder()
            .content(&content_area)
            .hexpand(true)
            .build();
        content_toolbar.add_top_bar(&header);

        main_box.append(&content_toolbar);

        toast_overlay.set_child(Some(&main_box));
        window.set_content(Some(&toast_overlay));

        // ── State subscribers ──────────────────────────────────────────────────────

        let hp = home_page.clone();
        store.subscribe(|state| state.version, move |state| hp.update(state));

        let cp = canvas_page.clone();
        let store_c = store.clone();
        let last_profile_cp: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        store.subscribe(
            |state| state.version,
            move |state| {
                let profile_changed = *last_profile_cp.borrow() != state.active_profile;
                if profile_changed {
                    *last_profile_cp.borrow_mut() = state.active_profile.clone();
                    cp.on_profile_switch(state, &store_c);
                } else {
                    cp.update(state, &store_c);
                }
            },
        );

        let clp = cooling_page.clone();
        let store_clp = store.clone();
        let last_profile_clp: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        store.subscribe(
            |state| state.version,
            move |state| {
                let profile_changed = *last_profile_clp.borrow() != state.active_profile;
                if profile_changed {
                    *last_profile_clp.borrow_mut() = state.active_profile.clone();
                    clp.on_profile_switch(state, &store_clp);
                } else {
                    clp.update_live(state, &store_clp);
                }
            },
        );

        let sp = settings_page.clone();
        let store_sp = store.clone();
        store.subscribe(
            |state| state.version,
            move |state| {
                sp.update_live(state, store_sp.is_connected());
                sp.on_scan_complete(state);
            },
        );

        let sb = sidebar.clone();
        let store_sb = store.clone();
        store.subscribe(
            |state| state.version,
            move |state| {
                sb.update(state, store_sb.is_connected());
            },
        );

        let ps = profile_switcher.clone();
        let last_key: Rc<RefCell<Option<(String, Vec<String>)>>> = Rc::new(RefCell::new(None));
        store.subscribe(
            |state| crate::store::sel_hash(&(&state.active_profile, &state.profiles)),
            move |state| {
                let key = (state.active_profile.clone(), state.profiles.clone());
                if last_key.borrow().as_ref() != Some(&key) {
                    ps.update(&state.active_profile, &state.profiles);
                    *last_key.borrow_mut() = Some(key);
                }
            },
        );

        let arp = app_rules_page.clone();
        store.subscribe(
            |state| {
                crate::store::sel_hash(&(
                    &state.app_rules,
                    &state.profiles,
                    state.focus_watcher_supported,
                ))
            },
            move |state| {
                arp.update(
                    &state.app_rules,
                    &state.profiles,
                    state.focus_watcher_supported,
                );
            },
        );

        let dp = device_page.clone();
        let init_state = store.state();
        let last_profile_dp: Rc<RefCell<String>> =
            Rc::new(RefCell::new(init_state.active_profile.clone()));
        let last_fan_hash: Rc<Cell<u64>> = Rc::new(Cell::new(crate::store::sel_hash(
            &init_state
                .fan_curves
                .iter()
                .map(|c| &c.points)
                .collect::<Vec<_>>(),
        )));
        drop(init_state);
        store.subscribe(
            |state| state.version,
            move |state| {
                if let Some(id) = dp.current_id() {
                    if let Some(dev) = state.devices.iter().find(|d| d.id == id) {
                        let profile_changed = *last_profile_dp.borrow() != state.active_profile;
                        let new_fan_hash = crate::store::sel_hash(
                            &state
                                .fan_curves
                                .iter()
                                .map(|c| &c.points)
                                .collect::<Vec<_>>(),
                        );
                        let curves_changed = last_fan_hash.get() != new_fan_hash;
                        *last_profile_dp.borrow_mut() = state.active_profile.clone();
                        last_fan_hash.set(new_fan_hash);
                        if dp.structure_changed(dev) || profile_changed || curves_changed {
                            let sensors = state.all_sensors();
                            dp.show_device(
                                dev,
                                &state.devices,
                                &state.fan_curves,
                                &state.preset_curves,
                                &sensors,
                            );
                        } else {
                            dp.refresh(
                                dev,
                                &state.fan_curves,
                                &state.all_sensors(),
                                &state.lcd_engine,
                            );
                        }
                    }
                }
            },
        );

        // ── Connection subscribers ─────────────────────────────────────────────────

        let overlay = daemon_overlay.clone();
        store.on_connection(move |connected| overlay.set_visible(!connected));

        let cp_reset = canvas_page.clone();
        store.on_connection(move |connected| {
            if !connected {
                cp_reset.reset_frame();
            }
        });

        let clp_reset = cooling_page.clone();
        store.on_connection(move |_| clp_reset.reset_init());

        let sp_reset = settings_page.clone();
        store.on_connection(move |_| sp_reset.reset_init());

        let ps_conn = profile_switcher.clone();
        store.on_connection(move |connected| ps_conn.root.set_sensitive(connected));

        let sb_conn = sidebar.clone();
        let store_sb_conn = store.clone();
        store.on_connection(move |connected| {
            let state = store_sb_conn.state();
            sb_conn.update(&state, connected);
        });

        let store_home = store.clone();
        store.on_connection(move |connected| {
            if connected {
                store_home.navigate(NavTarget::Home);
            }
        });

        // ── Ctrl+F toggles the header search bar (home page only) ──────────────────

        let key_ctl = gtk::EventControllerKey::new();
        key_ctl.set_propagation_phase(gtk::PropagationPhase::Capture);
        let search_bar_key = search_bar.clone();
        let stack_key = content_stack.clone();
        let search_entry_key = home_page.search_entry.clone();
        key_ctl.connect_key_pressed(move |_, key, _, modifier| {
            if key == gtk::gdk::Key::f
                && modifier.contains(gtk::gdk::ModifierType::CONTROL_MASK)
                && stack_key.visible_child_name().as_deref() == Some("home")
            {
                let reveal = !search_bar_key.is_search_mode();
                search_bar_key.set_search_mode(reveal);
                if reveal {
                    search_entry_key.grab_focus();
                }
                return gtk::glib::Propagation::Stop;
            }
            gtk::glib::Propagation::Proceed
        });
        window.add_controller(key_ctl);

        let store_close = store.clone();
        window.connect_close_request(move |w| {
            if store_close.state().global_config.close_to_tray {
                w.set_visible(false);
                gtk::glib::Propagation::Stop
            } else {
                gtk::glib::Propagation::Proceed
            }
        });

        MainWindow {
            window,
            toast_overlay,
            sidebar,
            home_page,
            canvas_page,
            cooling_page,
            lighting_page,
            settings_page,
            app_rules_page,
            device_page,
            store,
            profile_switcher,
            content_stack,
            daemon_overlay,
        }
    }

    pub fn present(&self) {
        self.window.present();
    }
}
