use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

/// How the buffer's pixels are oriented relative to what the user sees on the monitor.
/// For a portrait display Windows reports `Cw90` (or `Cw270`); the buffer itself stays
/// in landscape pixel layout, so consumers must rotate when reading to get an upright
/// image.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum FrameRotation {
    #[default]
    None,
    Cw90,
    Cw180,
    Cw270,
}

pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    /// Row stride in bytes (>= width * 4 when the compositor adds alignment padding).
    pub stride: u32,
    /// Raw BGRA bytes from the compositor, row-major. Use `stride` to index rows.
    /// Wrapped in Arc so cloning `latest_frame()` is O(1) regardless of resolution.
    pub data: Arc<Vec<u8>>,
    pub rotation: FrameRotation,
}

pub struct MonitorInfo {
    pub index: usize,
    /// Human-readable label for the UI dropdown, e.g. "Monitor 0: 1920×1080".
    pub label: String,
}

pub struct CaptureHandle {
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
}

impl CaptureHandle {
    pub fn latest_frame(&self) -> Option<RawFrame> {
        let guard = self.frame_slot.lock().unwrap();
        guard.as_ref().map(|f| RawFrame {
            width: f.width,
            height: f.height,
            stride: f.stride,
            data: Arc::clone(&f.data),
            rotation: f.rotation,
        })
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Returns available monitors for the UI dropdown.
/// On Linux returns a single "Default" entry because the portal dialog handles selection.
/// On Windows returns one entry per physical monitor enumerated from DXGI.
pub fn list_monitors() -> Vec<MonitorInfo> {
    #[cfg(target_os = "linux")]
    return linux::list_monitors();

    #[cfg(target_os = "windows")]
    return windows::list_monitors();

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    vec![]
}

/// Spawns the platform capture backend. Returns immediately; frames arrive
/// asynchronously and can be read via `CaptureHandle::latest_frame()`.
pub fn start_capture(monitor_index: usize) -> Result<CaptureHandle, String> {
    let frame_slot: Arc<Mutex<Option<RawFrame>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));

    #[cfg(target_os = "linux")]
    linux::start(monitor_index, Arc::clone(&frame_slot), Arc::clone(&stop))?;

    #[cfg(target_os = "windows")]
    windows::start(monitor_index, Arc::clone(&frame_slot), Arc::clone(&stop))?;

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    return Err("screen capture is not supported on this platform".into());

    Ok(CaptureHandle { frame_slot, stop })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(w: u32, h: u32) -> RawFrame {
        let stride = w * 4;
        let data = vec![0u8; (stride * h) as usize];
        RawFrame {
            width: w,
            height: h,
            stride,
            data: Arc::new(data),
            rotation: FrameRotation::None,
        }
    }

    #[test]
    fn latest_frame_returns_none_when_slot_is_empty() {
        let frame_slot = Arc::new(Mutex::new(None));
        let handle = CaptureHandle { frame_slot, stop: Arc::new(AtomicBool::new(false)) };
        assert!(handle.latest_frame().is_none());
    }

    #[test]
    fn latest_frame_returns_frame_with_correct_dimensions() {
        let frame_slot = Arc::new(Mutex::new(Some(make_frame(320, 240))));
        let handle = CaptureHandle { frame_slot, stop: Arc::new(AtomicBool::new(false)) };
        let frame = handle.latest_frame().unwrap();
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.stride, 320 * 4);
    }

    #[test]
    fn latest_frame_arc_clone_shares_data_with_stored_frame() {
        let frame_slot = Arc::new(Mutex::new(Some(make_frame(4, 4))));
        let stored_ptr = {
            let guard = frame_slot.lock().unwrap();
            Arc::as_ptr(&guard.as_ref().unwrap().data)
        };
        let handle = CaptureHandle { frame_slot, stop: Arc::new(AtomicBool::new(false)) };
        let frame = handle.latest_frame().unwrap();
        // latest_frame should Arc::clone the data, not copy it — same pointer.
        assert_eq!(Arc::as_ptr(&frame.data), stored_ptr);
    }

    #[test]
    fn drop_sets_stop_flag() {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        {
            let _handle = CaptureHandle {
                frame_slot: Arc::new(Mutex::new(None)),
                stop,
            };
        }
        assert!(stop_c.load(Ordering::Relaxed), "stop flag should be set after drop");
    }
}
