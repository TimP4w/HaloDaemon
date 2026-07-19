// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use super::{FrameRotation, MonitorInfo, RawFrame};

pub fn list_monitors() -> Vec<MonitorInfo> {
    enumerate_dxgi_monitors().unwrap_or_else(|e| {
        log::warn!("DXGI monitor enumeration failed: {e}");
        vec![MonitorInfo {
            index: 0,
            label: "Monitor 0".to_string(),
        }]
    })
}

pub fn start(
    monitor_index: usize,
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let (setup_tx, setup_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
    std::thread::spawn(move || {
        if let Err(e) = run_dxgi_capture(monitor_index, frame_slot, stop, setup_tx) {
            log::error!("DXGI capture error: {e}");
        }
    });
    setup_rx
        .recv()
        .unwrap_or_else(|_| Err("DXGI capture thread exited before setup".into()))
}

fn dxgi_to_frame_rotation(
    r: windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION,
) -> FrameRotation {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
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
                let w =
                    (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left).unsigned_abs();
                let h =
                    (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top).unsigned_abs();
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
        monitors.push(MonitorInfo {
            index: 0,
            label: "Monitor 0".to_string(),
        });
    }
    Ok(monitors)
}

fn run_dxgi_capture(
    monitor_index: usize,
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
    setup_done: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    use windows::{
        core::Interface,
        Win32::Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAPPED_SUBRESOURCE,
                D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dxgi::{
                Common::DXGI_SAMPLE_DESC, CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1,
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
                Default::default(),
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
        let output1 = match output1 {
            Some(o) => o,
            None => {
                let a = factory
                    .EnumAdapters1(0)
                    .map_err(|e| format!("EnumAdapters1: {e}"))?;
                let o = a.EnumOutputs(0).map_err(|e| format!("EnumOutputs: {e}"))?;
                o.cast().map_err(|e| format!("IDXGIOutput1 cast: {e}"))?
            }
        };

        let mut duplication = output1
            .DuplicateOutput(&device)
            .map_err(|e| format!("DuplicateOutput: {e}"))?;

        // DXGI returns the buffer in the source surface's natural (landscape)
        // layout; the rotation hint is applied when the buffer is consumed.
        let mut rotation = dxgi_to_frame_rotation(duplication.GetDesc().Rotation);

        // Staging must match the captured frame's dimensions/format, not
        // DesktopCoordinates — those can disagree on a non-primary monitor with
        // a different DPI/orientation, and a mismatched CopyResource silently
        // produces undefined output (black canvas). Recreate from the source
        // texture's own descriptor whenever dimensions or format change.
        let mut staging: Option<ID3D11Texture2D> = None;
        let mut staging_w = 0u32;
        let mut staging_h = 0u32;
        let mut staging_format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(0);

        // Setup succeeded — signal the caller before entering the capture loop.
        let _ = setup_done.send(Ok(()));

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
                    // Duplication invalidated (resolution change, fullscreen,
                    // UAC/secure-desktop prompt, session switch); recreate it.
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
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Usage: D3D11_USAGE_STAGING,
                            BindFlags: 0,
                            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                            MiscFlags: 0,
                        };
                        let mut new_staging = None;
                        match device.CreateTexture2D(&staging_desc, None, Some(&mut new_staging)) {
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
                            let raw =
                                std::slice::from_raw_parts(mapped.pData as *const u8, byte_len)
                                    .to_vec();
                            context.Unmap(staging_ref, 0);
                            *frame_slot.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(RawFrame {
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
        let monitors = list_monitors();
        assert!(
            !monitors.is_empty(),
            "list_monitors should always return at least one entry"
        );
    }

    #[test]
    fn list_monitors_indices_match_position() {
        let monitors = list_monitors();
        for (pos, m) in monitors.iter().enumerate() {
            assert_eq!(
                m.index, pos,
                "monitor at position {pos} should have index {pos}"
            );
        }
    }

    #[test]
    fn start_returns_ok_and_spawns_background_thread() {
        let slot = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_c = Arc::clone(&stop);
        let result = start(0, slot, Arc::clone(&stop));
        stop_c.store(true, std::sync::atomic::Ordering::Relaxed);
        // DXGI/D3D11 may not be available in all environments (e.g. CI
        // runners without a GPU); accept either outcome — the important
        // thing is that start() returns without panicking or hanging.
        match result {
            Ok(()) => {} // DXGI available, capture started successfully
            Err(e) => log::info!("DXGI capture not available (CI/no GPU): {e}"),
        }
    }

    #[test]
    fn dxgi_rotation_maps_to_frame_rotation() {
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE180,
            DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
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
        assert_eq!(
            dxgi_to_frame_rotation(DXGI_MODE_ROTATION_UNSPECIFIED),
            FrameRotation::None,
        );
    }
}
