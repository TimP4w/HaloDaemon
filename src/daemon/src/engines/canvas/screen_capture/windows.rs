use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use super::{FrameRotation, MonitorInfo, RawFrame};

pub fn list_monitors() -> Vec<MonitorInfo> {
    enumerate_dxgi_monitors().unwrap_or_else(|e| {
        log::warn!("DXGI monitor enumeration failed: {e}");
        vec![MonitorInfo { index: 0, label: "Monitor 0".to_string() }]
    })
}

pub fn start(
    monitor_index: usize,
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    std::thread::spawn(move || {
        if let Err(e) = run_dxgi_capture(monitor_index, frame_slot, stop) {
            log::error!("DXGI capture error: {e}");
        }
    });
    Ok(())
}

fn dxgi_to_frame_rotation(
    r: windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION,
) -> FrameRotation {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180,
        DXGI_MODE_ROTATION_ROTATE270,
    };
    match r {
        x if x == DXGI_MODE_ROTATION_ROTATE90 => FrameRotation::Cw90,
        x if x == DXGI_MODE_ROTATION_ROTATE180 => FrameRotation::Cw180,
        x if x == DXGI_MODE_ROTATION_ROTATE270 => FrameRotation::Cw270,
        _ => FrameRotation::None,
    }
}

fn enumerate_dxgi_monitors() -> Result<Vec<MonitorInfo>, String> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};

    let mut monitors = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let mut a = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let mut o = 0u32;
            while let Ok(output) = adapter.EnumOutputs(o) {
                let desc = output.GetDesc().map_err(|e| format!("GetDesc: {e}"))?;
                let w = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left)
                    .unsigned_abs();
                let h = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top)
                    .unsigned_abs();
                let idx = monitors.len();
                monitors.push(MonitorInfo {
                    index: idx,
                    label: format!("Monitor {idx}: {w}×{h}"),
                });
                o += 1;
            }
            a += 1;
        }
    }
    if monitors.is_empty() {
        monitors.push(MonitorInfo { index: 0, label: "Monitor 0".to_string() });
    }
    Ok(monitors)
}

fn run_dxgi_capture(
    monitor_index: usize,
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    use windows::{
        core::Interface,
        Win32::Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ,
                D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
                D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::DXGI_SAMPLE_DESC,
                CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1,
                DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
            },
        },
    };

    unsafe {
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = {
            let mut dev = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
            .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
            (dev.unwrap(), ctx.unwrap())
        };

        // Locate the requested monitor output across all adapters.
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let mut output1: Option<IDXGIOutput1> = None;
        let mut flat = 0usize;
        'search: {
            let mut a = 0u32;
            while let Ok(adapter) = factory.EnumAdapters1(a) {
                let mut o = 0u32;
                while let Ok(out) = adapter.EnumOutputs(o) {
                    if flat == monitor_index {
                        output1 = Some(out.cast().map_err(|e| format!("IDXGIOutput1: {e}"))?);
                        break 'search;
                    }
                    flat += 1;
                    o += 1;
                }
                a += 1;
            }
        }
        // Fall back to the first output if the index is out of range.
        let output1 = match output1 {
            Some(o) => o,
            None => {
                let a = factory.EnumAdapters1(0).map_err(|e| format!("EnumAdapters1: {e}"))?;
                let o = a.EnumOutputs(0).map_err(|e| format!("EnumOutputs: {e}"))?;
                o.cast().map_err(|e| format!("IDXGIOutput1 cast: {e}"))?
            }
        };

        let mut duplication = output1
            .DuplicateOutput(&device)
            .map_err(|e| format!("DuplicateOutput: {e}"))?;

        // Read DXGI's rotation hint so callers can un-rotate the buffer for
        // portrait monitors. DXGI hands back the buffer in the source surface's
        // natural (landscape) pixel layout, so the rotation has to be applied
        // when the buffer is consumed — not the other way around.
        let mut rotation = dxgi_to_frame_rotation(duplication.GetDesc().Rotation);

        // The staging texture must match the *captured* frame's dimensions and
        // format — `IDXGIOutput::GetDesc().DesktopCoordinates` can disagree
        // with the real frame on a non-primary monitor when its DPI scale or
        // orientation differs from the primary, and a mismatched CopyResource
        // silently produces undefined output (typically all zeros → black
        // canvas). So we (re)create staging from the source texture's own
        // descriptor whenever the dimensions or format change.
        let mut staging: Option<ID3D11Texture2D> = None;
        let mut staging_w = 0u32;
        let mut staging_h = 0u32;
        let mut staging_format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(0);

        let frame_interval = std::time::Duration::from_millis(67); // ~15 fps
        let mut last = std::time::Instant::now()
            .checked_sub(frame_interval)
            .unwrap_or_else(std::time::Instant::now);

        while !stop.load(Ordering::Relaxed) {
            let now = std::time::Instant::now();
            if now.duration_since(last) < frame_interval {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match duplication.AcquireNextFrame(100, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    // The duplication was invalidated (resolution change,
                    // fullscreen app, UAC/secure-desktop prompt, session
                    // switch). The object is dead — recreate it.
                    log::debug!("Desktop duplication lost; recreating");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    match output1.DuplicateOutput(&device) {
                        Ok(d) => {
                            duplication = d;
                            rotation = dxgi_to_frame_rotation(duplication.GetDesc().Rotation);
                        }
                        Err(e) => {
                            log::warn!("DuplicateOutput (recreate): {e}");
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                    continue;
                }
                Err(e) => {
                    log::warn!("AcquireNextFrame: {e}");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            }

            if let Some(res) = resource {
                if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                    let mut src_desc = D3D11_TEXTURE2D_DESC::default();
                    tex.GetDesc(&mut src_desc);

                    if staging.is_none()
                        || src_desc.Width != staging_w
                        || src_desc.Height != staging_h
                        || src_desc.Format != staging_format
                    {
                        let staging_desc = D3D11_TEXTURE2D_DESC {
                            Width: src_desc.Width,
                            Height: src_desc.Height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: src_desc.Format,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_STAGING,
                            BindFlags: 0,
                            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                            MiscFlags: 0,
                        };
                        let mut new_staging = None;
                        match device.CreateTexture2D(
                            &staging_desc,
                            None,
                            Some(&mut new_staging),
                        ) {
                            Ok(()) => {
                                staging = new_staging;
                                staging_w = src_desc.Width;
                                staging_h = src_desc.Height;
                                staging_format = src_desc.Format;
                            }
                            Err(e) => {
                                log::warn!("CreateTexture2D (staging): {e}");
                                let _ = duplication.ReleaseFrame();
                                continue;
                            }
                        }
                    }

                    if let Some(staging_ref) = staging.as_ref() {
                        context.CopyResource(staging_ref, &tex);
                        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                        if context
                            .Map(staging_ref, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                            .is_ok()
                        {
                            let stride = mapped.RowPitch;
                            let byte_len = (stride * staging_h) as usize;
                            let raw = std::slice::from_raw_parts(
                                mapped.pData as *const u8,
                                byte_len,
                            )
                            .to_vec();
                            context.Unmap(staging_ref, 0);
                            *frame_slot.lock().unwrap() = Some(RawFrame {
                                width: staging_w,
                                height: staging_h,
                                stride,
                                data: Arc::new(raw),
                                rotation,
                            });
                            last = now;
                        }
                    }
                }
            }
            let _ = duplication.ReleaseFrame();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_monitors_never_returns_empty() {
        // enumerate_dxgi_monitors may fail on CI; list_monitors falls back to
        // a single "Monitor 0" entry so the dropdown always has something.
        let monitors = list_monitors();
        assert!(!monitors.is_empty(), "list_monitors should always return at least one entry");
    }

    #[test]
    fn list_monitors_indices_match_position() {
        let monitors = list_monitors();
        for (pos, m) in monitors.iter().enumerate() {
            assert_eq!(m.index, pos, "monitor at position {pos} should have index {pos}");
        }
    }

    #[test]
    fn start_returns_ok_and_spawns_background_thread() {
        // start() is non-blocking: it spawns a thread and returns immediately.
        // We only verify it returns Ok; stopping is via the AtomicBool.
        let slot = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        let result = start(0, slot, Arc::clone(&stop));
        // Signal the background thread to stop before the test exits.
        stop_c.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(result.is_ok());
    }

    #[test]
    fn dxgi_rotation_maps_to_frame_rotation() {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90,
            DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
            DXGI_MODE_ROTATION_UNSPECIFIED,
        };
        assert_eq!(
            dxgi_to_frame_rotation(DXGI_MODE_ROTATION_IDENTITY),
            FrameRotation::None,
        );
        assert_eq!(
            dxgi_to_frame_rotation(DXGI_MODE_ROTATION_ROTATE90),
            FrameRotation::Cw90,
        );
        assert_eq!(
            dxgi_to_frame_rotation(DXGI_MODE_ROTATION_ROTATE180),
            FrameRotation::Cw180,
        );
        assert_eq!(
            dxgi_to_frame_rotation(DXGI_MODE_ROTATION_ROTATE270),
            FrameRotation::Cw270,
        );
        // UNSPECIFIED (driver reported no rotation info) falls back to None
        // rather than producing a nonsense rotation.
        assert_eq!(
            dxgi_to_frame_rotation(DXGI_MODE_ROTATION_UNSPECIFIED),
            FrameRotation::None,
        );
    }
}
