/// Cross-platform input injection for key remapper actions.
///
/// Linux: evdev uinput — works on X11, Wayland, and headless. Requires /dev/uinput
/// write access (the HaloDaemon udev rules grant this via TAG+="uaccess").
///
/// Windows: enigo, which drives SendInput via the Win32 API.
use std::sync::Arc;

use anyhow::Result;

use crate::state::AppState;
use halod_protocol::types::{ButtonAction, CycleDir, DpiMode};

// ── Shared device-level helpers ───────────────────────────────────────────────

async fn dpi_cycle(direction: &CycleDir, device_id: &str, app: Arc<AppState>) {
    let device = match app.find_device_by_id(device_id).await {
        Some(d) => d,
        None => return,
    };
    let Some(sw) = device.as_dpi() else { return };
    let status = sw.dpi_status().await;
    // DPI cycling is a host-mode feature; in onboard mode the device's own
    // profiles govern DPI, so a remapped DpiCycle button is a no-op there.
    if status.steps.is_empty() || status.mode != DpiMode::Host {
        return;
    }
    let next = match direction {
        CycleDir::Up => (status.current_index + 1) % status.steps.len(),
        CycleDir::Down => {
            if status.current_index == 0 {
                status.steps.len() - 1
            } else {
                status.current_index - 1
            }
        }
    };
    if let Err(e) = sw.set_dpi_index(next).await {
        log::warn!("ActionExecutor: dpi_cycle: {e}");
    } else {
        crate::ipc::broadcast_state(app).await;
    }
}

async fn profile_cycle(direction: &CycleDir, device_id: &str, app: Arc<AppState>) {
    let device = match app.find_device_by_id(device_id).await {
        Some(d) => d,
        None => return,
    };
    let Some(op) = device.as_onboard_profiles() else {
        return;
    };
    let wire = device.serialize().await;
    let info = wire.capabilities.iter().find_map(|c| {
        if let halod_protocol::types::DeviceCapability::OnboardProfiles(p) = c {
            let enabled: Vec<u8> = p
                .slots
                .iter()
                .filter(|s| s.enabled)
                .map(|s| s.index)
                .collect();
            Some((p.active_slot, enabled))
        } else {
            None
        }
    });
    let Some((current, enabled)) = info else {
        return;
    };
    if enabled.is_empty() {
        return;
    }
    let pos = enabled.iter().position(|&s| s == current).unwrap_or(0);
    let next = match direction {
        CycleDir::Up => (pos + 1) % enabled.len(),
        CycleDir::Down => {
            if pos == 0 {
                enabled.len() - 1
            } else {
                pos - 1
            }
        }
    };
    if let Err(e) = op.switch_profile(enabled[next]).await {
        log::warn!("ActionExecutor: profile_cycle: {e}");
    } else {
        crate::ipc::broadcast_state(app).await;
    }
}

fn spawn_process(cmd: &str, args: &[String]) {
    if let Err(e) = tokio::process::Command::new(cmd).args(args).spawn() {
        log::warn!("ActionExecutor: spawn {cmd:?}: {e}");
    }
}

// ── Platform backends ─────────────────────────────────────────────────────────
//
// Each platform module exposes:
//   struct Backend
//   impl Backend { fn new() -> Result<Self> }
//   impl Backend { async fn mouse_button / scroll / key_chord / media_key }
//   fn run_macro(steps: Vec<MacroStep>)   — spawns its own task

#[cfg(target_os = "linux")]
mod platform {
    use anyhow::Result;
    use evdev::{
        uinput::VirtualDeviceBuilder, AttributeSet, EventType, InputEvent, Key as EvKey,
        RelativeAxisType,
    };
    use halod_protocol::types::{MacroAtom, MacroStep, MediaAction, ModKey, MouseBtn, ScrollAxis};
    use tokio::sync::Mutex;

    pub struct Backend {
        kbd: Mutex<evdev::uinput::VirtualDevice>,
        ptr: Mutex<evdev::uinput::VirtualDevice>,
    }

    impl Backend {
        pub fn new() -> Result<Self> {
            let mut kbd_keys = AttributeSet::<EvKey>::new();
            // All key codes 1–767 so KeyChord can use arbitrary Linux input codes.
            for code in 1u16..=767 {
                kbd_keys.insert(EvKey::new(code));
            }
            let kbd = VirtualDeviceBuilder::new()?
                .name("HaloDaemon Virtual Keyboard")
                .with_keys(&kbd_keys)?
                .build()?;

            let mut btn_keys = AttributeSet::<EvKey>::new();
            for k in [
                EvKey::BTN_LEFT,
                EvKey::BTN_RIGHT,
                EvKey::BTN_MIDDLE,
                EvKey::BTN_SIDE,
                EvKey::BTN_EXTRA,
            ] {
                btn_keys.insert(k);
            }
            let mut axes = AttributeSet::<RelativeAxisType>::new();
            axes.insert(RelativeAxisType::REL_WHEEL);
            axes.insert(RelativeAxisType::REL_HWHEEL);
            let ptr = VirtualDeviceBuilder::new()?
                .name("HaloDaemon Virtual Pointer")
                .with_keys(&btn_keys)?
                .with_relative_axes(&axes)?
                .build()?;

            Ok(Self {
                kbd: Mutex::new(kbd),
                ptr: Mutex::new(ptr),
            })
        }

        pub async fn mouse_button(&self, btn: &MouseBtn, pressed: bool) {
            let val = i32::from(pressed);
            if let Err(e) =
                self.ptr
                    .lock()
                    .await
                    .emit(&[InputEvent::new(EventType::KEY, mouse_btn(btn), val)])
            {
                log::warn!("ActionExecutor: mouse button: {e}");
            }
        }

        pub async fn scroll(&self, axis: &ScrollAxis, clicks: i32) {
            let code = match axis {
                ScrollAxis::Vertical => RelativeAxisType::REL_WHEEL.0,
                ScrollAxis::Horizontal => RelativeAxisType::REL_HWHEEL.0,
            };
            if let Err(e) =
                self.ptr
                    .lock()
                    .await
                    .emit(&[InputEvent::new(EventType::RELATIVE, code, clicks)])
            {
                log::warn!("ActionExecutor: scroll: {e}");
            }
        }

        pub async fn key_chord(&self, key: u32, modifiers: &[ModKey], pressed: bool) {
            let mut kbd = self.kbd.lock().await;
            if pressed {
                for m in modifiers {
                    let _ = kbd.emit(&[InputEvent::new(EventType::KEY, mod_key(m), 1)]);
                }
                if let Err(e) = kbd.emit(&[InputEvent::new(EventType::KEY, key as u16, 1)]) {
                    log::warn!("ActionExecutor: key chord press: {e}");
                }
            } else {
                let _ = kbd.emit(&[InputEvent::new(EventType::KEY, key as u16, 0)]);
                for m in modifiers.iter().rev() {
                    let _ = kbd.emit(&[InputEvent::new(EventType::KEY, mod_key(m), 0)]);
                }
            }
        }

        pub async fn media_key(&self, key: &MediaAction) {
            let code = media_key(key);
            let mut kbd = self.kbd.lock().await;
            let _ = kbd.emit(&[InputEvent::new(EventType::KEY, code, 1)]);
            let _ = kbd.emit(&[InputEvent::new(EventType::KEY, code, 0)]);
        }
    }

    pub fn run_macro(steps: Vec<MacroStep>) {
        match Backend::new() {
            Ok(exec) => {
                tokio::spawn(async move {
                    for step in &steps {
                        match &step.kind {
                            MacroAtom::KeyDown { key } => {
                                let _ = exec.kbd.lock().await.emit(&[InputEvent::new(
                                    EventType::KEY,
                                    *key as u16,
                                    1,
                                )]);
                            }
                            MacroAtom::KeyUp { key } => {
                                let _ = exec.kbd.lock().await.emit(&[InputEvent::new(
                                    EventType::KEY,
                                    *key as u16,
                                    0,
                                )]);
                            }
                            MacroAtom::MouseDown { btn } => {
                                let _ = exec.ptr.lock().await.emit(&[InputEvent::new(
                                    EventType::KEY,
                                    mouse_btn(btn),
                                    1,
                                )]);
                            }
                            MacroAtom::MouseUp { btn } => {
                                let _ = exec.ptr.lock().await.emit(&[InputEvent::new(
                                    EventType::KEY,
                                    mouse_btn(btn),
                                    0,
                                )]);
                            }
                            MacroAtom::Delay => {}
                        }
                        if step.delay_after_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                step.delay_after_ms as u64,
                            ))
                            .await;
                        }
                    }
                });
            }
            Err(e) => log::warn!("ActionExecutor: macro uinput init: {e}"),
        }
    }

    fn mod_key(m: &ModKey) -> u16 {
        match m {
            ModKey::Ctrl => EvKey::KEY_LEFTCTRL.code(),
            ModKey::Shift => EvKey::KEY_LEFTSHIFT.code(),
            ModKey::Alt => EvKey::KEY_LEFTALT.code(),
            ModKey::Super => EvKey::KEY_LEFTMETA.code(),
        }
    }

    fn mouse_btn(btn: &MouseBtn) -> u16 {
        match btn {
            MouseBtn::Left => EvKey::BTN_LEFT.code(),
            MouseBtn::Right => EvKey::BTN_RIGHT.code(),
            MouseBtn::Middle => EvKey::BTN_MIDDLE.code(),
            MouseBtn::Back => EvKey::BTN_SIDE.code(),
            MouseBtn::Forward => EvKey::BTN_EXTRA.code(),
        }
    }

    fn media_key(key: &MediaAction) -> u16 {
        match key {
            MediaAction::VolumeUp => EvKey::KEY_VOLUMEUP.code(),
            MediaAction::VolumeDown => EvKey::KEY_VOLUMEDOWN.code(),
            MediaAction::Mute => EvKey::KEY_MUTE.code(),
            MediaAction::Play => EvKey::KEY_PLAYPAUSE.code(),
            MediaAction::Next => EvKey::KEY_NEXTSONG.code(),
            MediaAction::Prev => EvKey::KEY_PREVIOUSSONG.code(),
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use anyhow::Result;
    use enigo::{Button, Direction, Enigo, Key, Keyboard, Mouse, Settings};
    use halod_protocol::types::{MacroAtom, MacroStep, MediaAction, ModKey, MouseBtn, ScrollAxis};
    use tokio::sync::Mutex;

    pub struct Backend {
        enigo: Mutex<Enigo>,
    }

    impl Backend {
        pub fn new() -> Result<Self> {
            Ok(Self {
                enigo: Mutex::new(Enigo::new(&Settings::default())?),
            })
        }

        pub async fn mouse_button(&self, btn: &MouseBtn, pressed: bool) {
            let dir = if pressed {
                Direction::Press
            } else {
                Direction::Release
            };
            if let Err(e) = self.enigo.lock().await.button(map_btn(btn), dir) {
                log::warn!("ActionExecutor: mouse button: {e}");
            }
        }

        pub async fn scroll(&self, axis: &ScrollAxis, clicks: i32) {
            let ax = match axis {
                ScrollAxis::Vertical => enigo::Axis::Vertical,
                ScrollAxis::Horizontal => enigo::Axis::Horizontal,
            };
            if let Err(e) = self.enigo.lock().await.scroll(clicks, ax) {
                log::warn!("ActionExecutor: scroll: {e}");
            }
        }

        pub async fn key_chord(&self, key: u32, modifiers: &[ModKey], pressed: bool) {
            let mut eng = self.enigo.lock().await;
            if pressed {
                // Press the modifiers then the key. If any press fails, roll
                // back every key already pressed — a partial chord must never
                // leave a modifier stranded in the held state.
                let mut held: Vec<Key> = Vec::new();
                let mut failed: Option<enigo::InputError> = None;
                for k in modifiers
                    .iter()
                    .map(map_mod)
                    .chain(std::iter::once(Key::Other(key)))
                {
                    match eng.key(k, Direction::Press) {
                        Ok(()) => held.push(k),
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    }
                }
                if let Some(e) = failed {
                    log::warn!("ActionExecutor: key chord press failed ({e}); rolling back");
                    for k in held.into_iter().rev() {
                        if let Err(e) = eng.key(k, Direction::Release) {
                            log::warn!("ActionExecutor: key chord rollback release: {e}");
                        }
                    }
                }
            } else {
                // Release best-effort: release the key and every modifier even
                // if one fails, so a single error can't strand the rest held.
                for k in std::iter::once(Key::Other(key)).chain(modifiers.iter().rev().map(map_mod))
                {
                    if let Err(e) = eng.key(k, Direction::Release) {
                        log::warn!("ActionExecutor: key chord release: {e}");
                    }
                }
            }
        }

        pub async fn media_key(&self, key: &MediaAction) {
            let k = match key {
                MediaAction::VolumeUp => Key::VolumeUp,
                MediaAction::VolumeDown => Key::VolumeDown,
                MediaAction::Mute => Key::VolumeMute,
                MediaAction::Play => Key::MediaPlayPause,
                MediaAction::Next => Key::MediaNextTrack,
                MediaAction::Prev => Key::MediaPrevTrack,
            };
            if let Err(e) = self.enigo.lock().await.key(k, Direction::Click) {
                log::warn!("ActionExecutor: media key: {e}");
            }
        }
    }

    pub fn run_macro(steps: Vec<MacroStep>) {
        match Enigo::new(&Settings::default()) {
            Ok(mut eng) => {
                tokio::spawn(async move {
                    for step in &steps {
                        match &step.kind {
                            MacroAtom::KeyDown { key } => {
                                let _ = eng.key(Key::Other(*key), Direction::Press);
                            }
                            MacroAtom::KeyUp { key } => {
                                let _ = eng.key(Key::Other(*key), Direction::Release);
                            }
                            MacroAtom::MouseDown { btn } => {
                                let _ = eng.button(map_btn(btn), Direction::Press);
                            }
                            MacroAtom::MouseUp { btn } => {
                                let _ = eng.button(map_btn(btn), Direction::Release);
                            }
                            MacroAtom::Delay => {}
                        }
                        if step.delay_after_ms > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                step.delay_after_ms as u64,
                            ))
                            .await;
                        }
                    }
                });
            }
            Err(e) => log::warn!("ActionExecutor: macro enigo init: {e}"),
        }
    }

    fn map_btn(btn: &MouseBtn) -> Button {
        match btn {
            MouseBtn::Left => Button::Left,
            MouseBtn::Right => Button::Right,
            MouseBtn::Middle => Button::Middle,
            MouseBtn::Back => Button::Back,
            MouseBtn::Forward => Button::Forward,
        }
    }

    fn map_mod(m: &ModKey) -> Key {
        match m {
            ModKey::Ctrl => Key::Control,
            ModKey::Shift => Key::Shift,
            ModKey::Alt => Key::Alt,
            ModKey::Super => Key::Meta,
        }
    }
}

// ── ActionExecutor ────────────────────────────────────────────────────────────

pub struct ActionExecutor {
    inner: platform::Backend,
}

impl ActionExecutor {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: platform::Backend::new()?,
        })
    }

    /// Execute one action. `pressed` is true on button press, false on release.
    /// LayerShift, MomentaryDpi, and Native are handled by the engine before this is called.
    pub async fn execute(
        &self,
        action: &ButtonAction,
        pressed: bool,
        device_id: &str,
        app: Arc<AppState>,
    ) {
        match action {
            // Handled by KeyRemapEngine before reaching here, or intentional no-ops.
            ButtonAction::Native
            | ButtonAction::LayerShift
            | ButtonAction::MomentaryDpi { .. }
            | ButtonAction::Disable => {}

            // Fire on both press and release.
            ButtonAction::MouseButton { btn } => self.inner.mouse_button(btn, pressed).await,
            ButtonAction::KeyChord { key, modifiers } => {
                self.inner.key_chord(*key, modifiers, pressed).await
            }

            // Fire on press only.
            ButtonAction::Scroll { axis, clicks } if pressed => {
                self.inner.scroll(axis, *clicks).await
            }
            ButtonAction::MediaKey { key } if pressed => self.inner.media_key(key).await,
            ButtonAction::DpiCycle { direction } if pressed => {
                dpi_cycle(direction, device_id, app).await
            }
            ButtonAction::ProfileCycle { direction } if pressed => {
                profile_cycle(direction, device_id, app).await
            }
            ButtonAction::OpenApp { path } if pressed => spawn_process(path, &[]),
            ButtonAction::Command { cmd, args } if pressed => spawn_process(cmd, args),
            ButtonAction::Macro { steps } if pressed => platform::run_macro(steps.clone()),

            // Press-only actions with pressed=false — nothing to do.
            _ => {}
        }
    }
}
