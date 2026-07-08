/// Cross-platform input injection for key remapper actions.
///
/// Linux: evdev uinput — works on X11, Wayland, and headless. Requires /dev/uinput
/// write access (the HaloDaemon udev rules grant this via TAG+="uaccess").
///
/// Windows: enigo, which drives SendInput via the Win32 API.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

use crate::state::AppState;
use halod_shared::types::{ButtonAction, CycleDir, DpiMode, MacroAtom, MacroStep, MouseBtn};

fn cycle_index(dir: &CycleDir, current: usize, len: usize) -> usize {
    match dir {
        CycleDir::Up => (current + 1) % len,
        CycleDir::Down => {
            if current == 0 {
                len - 1
            } else {
                current - 1
            }
        }
    }
}

async fn dpi_cycle(direction: &CycleDir, device_id: &str, app: Arc<AppState>) {
    let Some(device) = app.find_device_by_id(device_id).await else {
        return;
    };
    let Some(sw) = device.as_dpi() else { return };
    let status = sw.dpi_status().await;
    if status.steps.is_empty() || status.mode != DpiMode::Host {
        return;
    }
    let next = cycle_index(direction, status.current_index, status.steps.len());
    if let Err(e) = sw.set_dpi_index(next).await {
        log::warn!("ActionExecutor: dpi_cycle: {e}");
    } else {
        crate::ipc::broadcast_state(&app).await;
    }
}

async fn profile_cycle(direction: &CycleDir, device_id: &str, app: Arc<AppState>) {
    let Some(device) = app.find_device_by_id(device_id).await else {
        return;
    };
    let Some(op) = device.as_onboard_profiles() else {
        return;
    };
    let wire = device.serialize().await;
    let info = wire.capabilities.iter().find_map(|c| {
        if let halod_shared::types::DeviceCapability::OnboardProfiles(p) = c {
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
    let next = cycle_index(direction, pos, enabled.len());
    if let Err(e) = op.switch_profile(enabled[next]).await {
        log::warn!("ActionExecutor: profile_cycle: {e}");
    } else {
        crate::ipc::broadcast_state(&app).await;
    }
}

/// Spawn a user-configured `Command`/`OpenApp` button action.
///
/// Refused while elevated (Windows): the daemon may run at high integrity for
/// PawnIO SMBus access, and we don't want a button mapping to become a
/// privilege-escalation path from a medium-integrity UI compromise.
fn spawn_process(cmd: &str, args: &[String]) {
    #[cfg(windows)]
    if crate::platform::elevation::is_elevated() {
        log::warn!(
            "ActionExecutor: refusing to spawn {cmd:?} while running elevated \
             (Command/OpenApp actions are disabled when the daemon is elevated)"
        );
        return;
    }
    if let Err(e) = tokio::process::Command::new(cmd).args(args).spawn() {
        log::warn!("ActionExecutor: spawn {cmd:?}: {e}");
    }
}

/// Keys/buttons still held when a macro's steps end (down without a later
/// up). `run_macro` releases these after the last step so an unbalanced
/// sequence can't leave the virtual devices stuck.
fn unreleased(steps: &[MacroStep]) -> (Vec<u32>, Vec<MouseBtn>) {
    let mut keys: Vec<u32> = Vec::new();
    let mut btns: Vec<MouseBtn> = Vec::new();
    for step in steps {
        match &step.kind {
            MacroAtom::KeyDown { key } => {
                if !keys.contains(key) {
                    keys.push(*key);
                }
            }
            MacroAtom::KeyUp { key } => keys.retain(|k| k != key),
            MacroAtom::MouseDown { btn } => {
                if !btns.contains(btn) {
                    btns.push(btn.clone());
                }
            }
            MacroAtom::MouseUp { btn } => btns.retain(|b| b != btn),
            MacroAtom::Delay => {}
        }
    }
    (keys, btns)
}

/// Press each key in `keys` via `press`, in order. On the first failure, undo
/// every key already pressed (in reverse order) via `release` and stop.
/// Returns the keys left held: all of `keys` on success, none on rollback.
fn press_with_rollback<K: Copy>(
    keys: &[K],
    mut press: impl FnMut(K) -> bool,
    mut release: impl FnMut(K),
) -> Vec<K> {
    let mut held = Vec::new();
    for &k in keys {
        if press(k) {
            held.push(k);
        } else {
            while let Some(h) = held.pop() {
                release(h);
            }
            return Vec::new();
        }
    }
    held
}

#[cfg(target_os = "linux")]
mod platform {
    use anyhow::Result;
    use evdev::{
        uinput::VirtualDeviceBuilder, AttributeSet, EventType, InputEvent, Key as EvKey,
        RelativeAxisType,
    };
    use halod_shared::types::{MacroAtom, MacroStep, MediaAction, ModKey, MouseBtn, ScrollAxis};
    use tokio::sync::Mutex;

    pub struct Backend {
        kbd: Mutex<evdev::uinput::VirtualDevice>,
        ptr: Mutex<evdev::uinput::VirtualDevice>,
    }

    impl Backend {
        pub fn new() -> Result<Self> {
            let mut kbd_keys = AttributeSet::<EvKey>::new();
            for code in 1u16..=767 {
                kbd_keys.insert(EvKey::new(code));
            }
            let kbd = VirtualDeviceBuilder::new()?
                .name(crate::constants::VIRTUAL_KEYBOARD_NAME)
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
                .name(crate::constants::VIRTUAL_POINTER_NAME)
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

        fn validate_key_code(key: u32) -> Option<u16> {
            let k = u16::try_from(key).ok()?;
            if !(1..=767).contains(&k) {
                log::warn!("ActionExecutor: key code {key} out of registered range (1-767)");
                return None;
            }
            Some(k)
        }

        pub async fn key_chord(&self, key: u32, modifiers: &[ModKey], pressed: bool) {
            let Some(k) = Self::validate_key_code(key) else {
                return;
            };
            let mut kbd = self.kbd.lock().await;
            if pressed {
                let codes: Vec<u16> = modifiers.iter().map(mod_key).chain([k]).collect();
                let kbd_cell = std::cell::RefCell::new(&mut *kbd);
                let held = super::press_with_rollback(
                    &codes,
                    |code| {
                        kbd_cell
                            .borrow_mut()
                            .emit(&[InputEvent::new(EventType::KEY, code, 1)])
                            .is_ok()
                    },
                    |code| {
                        let _ =
                            kbd_cell
                                .borrow_mut()
                                .emit(&[InputEvent::new(EventType::KEY, code, 0)]);
                    },
                );
                if held.len() != codes.len() {
                    log::warn!("ActionExecutor: key chord press failed; rolled back");
                }
            } else {
                if let Err(e) = kbd.emit(&[InputEvent::new(EventType::KEY, k, 0)]) {
                    log::warn!("ActionExecutor: key chord release: {e}");
                }
                for m in modifiers.iter().rev() {
                    if let Err(e) = kbd.emit(&[InputEvent::new(EventType::KEY, mod_key(m), 0)]) {
                        log::warn!("ActionExecutor: key chord release modifier: {e}");
                    }
                }
            }
        }

        pub async fn media_key(&self, key: &MediaAction) {
            let code = media_key(key);
            let mut kbd = self.kbd.lock().await;
            if let Err(e) = kbd.emit(&[InputEvent::new(EventType::KEY, code, 1)]) {
                log::warn!("ActionExecutor: media_key press failed: {e}");
            }
            if let Err(e) = kbd.emit(&[InputEvent::new(EventType::KEY, code, 0)]) {
                log::warn!("ActionExecutor: media_key release failed: {e}");
            }
        }
    }

    fn emit(dev: &mut evdev::uinput::VirtualDevice, code: u16, val: i32) {
        if let Err(e) = dev.emit(&[InputEvent::new(EventType::KEY, code, val)]) {
            log::warn!("ActionExecutor: macro emit: {e}");
        }
    }

    pub fn run_macro(
        steps: Vec<MacroStep>,
        exec: std::sync::Arc<Backend>,
        active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        if active.swap(true, std::sync::atomic::Ordering::AcqRel) {
            log::debug!("ActionExecutor: macro already playing; ignoring play request");
            return false;
        }
        tokio::spawn(async move {
            let _guard = super::MacroGuard(active);
            for step in &steps {
                match &step.kind {
                    MacroAtom::KeyDown { key } => {
                        if let Some(k) = Backend::validate_key_code(*key) {
                            emit(&mut *exec.kbd.lock().await, k, 1);
                        }
                    }
                    MacroAtom::KeyUp { key } => {
                        if let Some(k) = Backend::validate_key_code(*key) {
                            emit(&mut *exec.kbd.lock().await, k, 0);
                        }
                    }
                    MacroAtom::MouseDown { btn } => {
                        emit(&mut *exec.ptr.lock().await, mouse_btn(btn), 1);
                    }
                    MacroAtom::MouseUp { btn } => {
                        emit(&mut *exec.ptr.lock().await, mouse_btn(btn), 0);
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
            let (keys, btns) = super::unreleased(&steps);
            if !keys.is_empty() {
                let mut kbd = exec.kbd.lock().await;
                for key in keys {
                    if let Some(k) = Backend::validate_key_code(key) {
                        emit(&mut kbd, k, 0);
                    }
                }
            }
            if !btns.is_empty() {
                let mut ptr = exec.ptr.lock().await;
                for btn in btns {
                    emit(&mut ptr, mouse_btn(&btn), 0);
                }
            }
        });
        true
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
    use halod_shared::types::{MacroAtom, MacroStep, MediaAction, ModKey, MouseBtn, ScrollAxis};
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
                let keys: Vec<Key> = modifiers
                    .iter()
                    .map(map_mod)
                    .chain(std::iter::once(Key::Other(key)))
                    .collect();
                let eng_cell = std::cell::RefCell::new(&mut *eng);
                let held = super::press_with_rollback(
                    &keys,
                    |k| eng_cell.borrow_mut().key(k, Direction::Press).is_ok(),
                    |k| {
                        let _ = eng_cell.borrow_mut().key(k, Direction::Release);
                    },
                );
                if held.len() != keys.len() {
                    log::warn!("ActionExecutor: key chord press failed; rolled back");
                }
            } else {
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

    /// Windows virtual-key codes are 8-bit (1..=0xFE); guard against an
    /// out-of-range code from a hand-crafted `PlayMacro` payload, mirroring
    /// the Linux backend's `validate_key_code`.
    fn valid_vk(key: u32) -> bool {
        (1..=0xFE).contains(&key)
    }

    fn press_key(eng: &mut Enigo, key: u32, dir: Direction) {
        if valid_vk(key) {
            if let Err(e) = eng.key(Key::Other(key), dir) {
                log::warn!("ActionExecutor: macro key: {e}");
            }
        }
    }

    fn press_btn(eng: &mut Enigo, btn: &MouseBtn, dir: Direction) {
        if let Err(e) = eng.button(map_btn(btn), dir) {
            log::warn!("ActionExecutor: macro button: {e}");
        }
    }

    pub fn run_macro(
        steps: Vec<MacroStep>,
        exec: std::sync::Arc<Backend>,
        active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        if active.swap(true, std::sync::atomic::Ordering::AcqRel) {
            log::debug!("ActionExecutor: macro already playing; ignoring play request");
            return false;
        }
        tokio::spawn(async move {
            let _guard = super::MacroGuard(active);
            for step in &steps {
                let mut eng = exec.enigo.lock().await;
                match &step.kind {
                    MacroAtom::KeyDown { key } => press_key(&mut eng, *key, Direction::Press),
                    MacroAtom::KeyUp { key } => press_key(&mut eng, *key, Direction::Release),
                    MacroAtom::MouseDown { btn } => press_btn(&mut eng, btn, Direction::Press),
                    MacroAtom::MouseUp { btn } => press_btn(&mut eng, btn, Direction::Release),
                    MacroAtom::Delay => {}
                }
                drop(eng);
                if step.delay_after_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        step.delay_after_ms as u64,
                    ))
                    .await;
                }
            }
            let (keys, btns) = super::unreleased(&steps);
            let mut eng = exec.enigo.lock().await;
            for key in keys {
                press_key(&mut eng, key, Direction::Release);
            }
            for btn in btns {
                press_btn(&mut eng, &btn, Direction::Release);
            }
        });
        true
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

/// Clears the "macro playing" flag when a `run_macro` task ends, so an error
/// or panic mid-macro can't wedge playback permanently.
struct MacroGuard(Arc<AtomicBool>);

impl Drop for MacroGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct ActionExecutor {
    inner: Arc<platform::Backend>,
    /// Set while a macro is playing. Only one runs at a time so sequences
    /// can't interleave key-down/up events on the shared virtual devices.
    macro_active: Arc<AtomicBool>,
}

impl ActionExecutor {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: Arc::new(platform::Backend::new()?),
            macro_active: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Run a macro immediately (editor test play). Returns false if one is
    /// already playing, in which case this call is a no-op.
    pub fn play_macro(&self, steps: Vec<MacroStep>) -> bool {
        platform::run_macro(
            steps,
            Arc::clone(&self.inner),
            Arc::clone(&self.macro_active),
        )
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

            ButtonAction::MouseButton { btn } => self.inner.mouse_button(btn, pressed).await,
            ButtonAction::KeyChord { key, modifiers } => {
                self.inner.key_chord(*key, modifiers, pressed).await
            }

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
            ButtonAction::Macro { steps } if pressed => {
                platform::run_macro(
                    steps.clone(),
                    Arc::clone(&self.inner),
                    Arc::clone(&self.macro_active),
                );
            }

            // Press-only actions with pressed=false — nothing to do.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, drivers::Device, test_support::MockDevice};
    use halod_shared::types::CycleDir;

    #[test]
    fn cycle_up_wraps_at_end() {
        assert_eq!(cycle_index(&CycleDir::Up, 3, 4), 0);
        assert_eq!(cycle_index(&CycleDir::Up, 0, 4), 1);
    }

    #[test]
    fn cycle_down_wraps_at_zero() {
        assert_eq!(cycle_index(&CycleDir::Down, 0, 4), 3);
        assert_eq!(cycle_index(&CycleDir::Down, 2, 4), 1);
    }

    #[test]
    fn cycle_on_single_element() {
        assert_eq!(cycle_index(&CycleDir::Up, 0, 1), 0);
        assert_eq!(cycle_index(&CycleDir::Down, 0, 1), 0);
    }

    #[tokio::test]
    async fn dpi_cycle_is_noop_when_no_steps_configured() {
        // MockDevice's dpi_status() always reports an empty `steps` list, so
        // DpiCycle must be a no-op (the `steps.is_empty()` guard), never
        // calling set_dpi_index.
        let app = Arc::new(AppState::new(Config::default()));
        let dev = Arc::new(MockDevice::new("dev1").with_dpi_initial(800));
        app.devices
            .write()
            .await
            .push(Arc::clone(&dev) as Arc<dyn Device>);

        dpi_cycle(&CycleDir::Up, "dev1", Arc::clone(&app)).await;

        assert_eq!(
            *dev.dpi_direct_last.as_ref().unwrap().lock().unwrap(),
            None,
            "dpi_cycle must not touch DPI when no steps are configured"
        );
    }

    #[test]
    fn press_with_rollback_holds_all_on_success() {
        let mut released = Vec::new();
        let held = press_with_rollback(&[1, 2, 3], |_| true, |k| released.push(k));
        assert_eq!(held, vec![1, 2, 3]);
        assert!(released.is_empty());
    }

    #[test]
    fn press_with_rollback_releases_already_held_on_mid_chord_failure() {
        let mut released = Vec::new();
        let held = press_with_rollback(&[1, 2, 3], |k| k != 3, |k| released.push(k));
        assert!(held.is_empty());
        assert_eq!(released, vec![2, 1]);
    }

    #[test]
    fn press_with_rollback_first_key_failure_releases_nothing() {
        let mut released = Vec::new();
        let held = press_with_rollback(&[1, 2, 3], |_| false, |k| released.push(k));
        assert!(held.is_empty());
        assert!(released.is_empty());
    }

    fn step(kind: MacroAtom) -> MacroStep {
        MacroStep {
            kind,
            delay_after_ms: 0,
        }
    }

    #[test]
    fn macro_guard_gate_admits_one_and_resets_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        // First claim wins; a concurrent claim is rejected while it's held.
        assert!(!flag.swap(true, Ordering::AcqRel));
        {
            let _guard = MacroGuard(Arc::clone(&flag));
            assert!(
                flag.swap(true, Ordering::AcqRel),
                "second macro must be rejected"
            );
        }
        // Guard drop clears the flag so the next macro can start.
        assert!(!flag.load(Ordering::Acquire));
        assert!(!flag.swap(true, Ordering::AcqRel));
    }

    #[test]
    fn unreleased_empty_for_balanced_sequence() {
        let steps = vec![
            step(MacroAtom::KeyDown { key: 30 }),
            step(MacroAtom::MouseDown {
                btn: MouseBtn::Left,
            }),
            step(MacroAtom::Delay),
            step(MacroAtom::MouseUp {
                btn: MouseBtn::Left,
            }),
            step(MacroAtom::KeyUp { key: 30 }),
        ];
        let (keys, btns) = unreleased(&steps);
        assert!(keys.is_empty());
        assert!(btns.is_empty());
    }

    #[test]
    fn unreleased_detects_held_key_and_button() {
        let steps = vec![
            step(MacroAtom::KeyDown { key: 30 }),
            step(MacroAtom::KeyDown { key: 31 }),
            step(MacroAtom::KeyUp { key: 30 }),
            step(MacroAtom::MouseDown {
                btn: MouseBtn::Right,
            }),
        ];
        let (keys, btns) = unreleased(&steps);
        assert_eq!(keys, vec![31]);
        assert_eq!(btns, vec![MouseBtn::Right]);
    }

    // The linux column of the shared key table must match the evdev codes the
    // uinput backend injects (the daemon-side pin for the shared constants).
    #[cfg(target_os = "linux")]
    #[test]
    fn keycode_table_matches_evdev_constants() {
        use evdev::Key as EvKey;
        for (name, ev) in [
            ("A", EvKey::KEY_A),
            ("Z", EvKey::KEY_Z),
            ("1", EvKey::KEY_1),
            ("0", EvKey::KEY_0),
            ("Space", EvKey::KEY_SPACE),
            ("Enter", EvKey::KEY_ENTER),
            ("Escape", EvKey::KEY_ESC),
            ("Tab", EvKey::KEY_TAB),
            ("Backspace", EvKey::KEY_BACKSPACE),
            ("F1", EvKey::KEY_F1),
            ("F12", EvKey::KEY_F12),
            ("F24", EvKey::KEY_F24),
            ("Up", EvKey::KEY_UP),
            ("Down", EvKey::KEY_DOWN),
            ("Left", EvKey::KEY_LEFT),
            ("Right", EvKey::KEY_RIGHT),
            ("Home", EvKey::KEY_HOME),
            ("End", EvKey::KEY_END),
            ("PageUp", EvKey::KEY_PAGEUP),
            ("PageDown", EvKey::KEY_PAGEDOWN),
            ("Insert", EvKey::KEY_INSERT),
            ("Delete", EvKey::KEY_DELETE),
            ("Minus", EvKey::KEY_MINUS),
            ("Equals", EvKey::KEY_EQUAL),
            ("Comma", EvKey::KEY_COMMA),
            ("Period", EvKey::KEY_DOT),
            ("Slash", EvKey::KEY_SLASH),
            ("Backslash", EvKey::KEY_BACKSLASH),
            ("Semicolon", EvKey::KEY_SEMICOLON),
            ("Quote", EvKey::KEY_APOSTROPHE),
            ("OpenBracket", EvKey::KEY_LEFTBRACE),
            ("CloseBracket", EvKey::KEY_RIGHTBRACE),
            ("Backtick", EvKey::KEY_GRAVE),
            ("IntlBackslash", EvKey::KEY_102ND),
            ("BrowserBack", EvKey::KEY_BACK),
            ("ShiftLeft", EvKey::KEY_LEFTSHIFT),
            ("ShiftRight", EvKey::KEY_RIGHTSHIFT),
            ("ControlLeft", EvKey::KEY_LEFTCTRL),
            ("ControlRight", EvKey::KEY_RIGHTCTRL),
            ("AltLeft", EvKey::KEY_LEFTALT),
            ("AltRight", EvKey::KEY_RIGHTALT),
            ("SuperLeft", EvKey::KEY_LEFTMETA),
            ("SuperRight", EvKey::KEY_RIGHTMETA),
        ] {
            assert_eq!(
                halod_shared::keycodes::by_name(name).map(|k| k.linux),
                Some(ev.code() as u32),
                "table/evdev mismatch for {name}"
            );
        }
    }

    proptest::proptest! {
        // Appending the releases `unreleased` reports always yields a fully
        // balanced sequence — the trailing-release safety can't miss anything.
        #[test]
        fn unreleased_releases_balance_any_sequence(ops in proptest::collection::vec((0u8..5, 1u32..10), 0..40)) {
            let steps: Vec<MacroStep> = ops
                .into_iter()
                .map(|(op, n)| {
                    let btn = match n % 3 {
                        0 => MouseBtn::Left,
                        1 => MouseBtn::Right,
                        _ => MouseBtn::Middle,
                    };
                    step(match op {
                        0 => MacroAtom::KeyDown { key: n },
                        1 => MacroAtom::KeyUp { key: n },
                        2 => MacroAtom::MouseDown { btn },
                        3 => MacroAtom::MouseUp { btn },
                        _ => MacroAtom::Delay,
                    })
                })
                .collect();
            let (keys, btns) = unreleased(&steps);
            let mut balanced = steps;
            balanced.extend(keys.into_iter().map(|key| step(MacroAtom::KeyUp { key })));
            balanced.extend(btns.into_iter().map(|btn| step(MacroAtom::MouseUp { btn })));
            let (k2, b2) = unreleased(&balanced);
            proptest::prop_assert!(k2.is_empty() && b2.is_empty());
        }
    }
}
