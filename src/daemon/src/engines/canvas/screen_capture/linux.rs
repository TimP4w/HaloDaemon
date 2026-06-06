use std::{
    os::fd::{FromRawFd, IntoRawFd, OwnedFd},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use pipewire::spa::buffer::DataType;

use super::{FrameRotation, MonitorInfo, RawFrame};

// DMA-BUF CPU-access synchronization.
// _IOW('b', 0, struct dma_buf_sync) on x86_64 Linux.
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x40086200;
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_START: u64 = 0 << 2;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

fn dmabuf_sync(fd: i32, flags: u64) {
    let s = DmaBufSync { flags };
    unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &s) };
}

/// Builds an SPA_PARAM_EnumFormat pod for video/raw BGRA without DRM modifiers.
/// Omitting the modifier tells the compositor to use MemFd (shared memory) rather
/// than DMA-BUF, so MAP_BUFFERS can map the data directly and we avoid GPU tiling.
fn build_enum_format_pod() -> Vec<u8> {
    use pipewire::spa::{
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            ParamType,
        },
        pod::{serialize::PodSerializer, ChoiceValue, Object, Property, Value},
        utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes},
    };

    let obj = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                FormatProperties::MediaType.as_raw(),
                Value::Id(Id(MediaType::Video.as_raw())),
            ),
            Property::new(
                FormatProperties::MediaSubtype.as_raw(),
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                FormatProperties::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(pipewire::spa::sys::SPA_VIDEO_FORMAT_BGRx),
                        alternatives: vec![Id(pipewire::spa::sys::SPA_VIDEO_FORMAT_BGRA)],
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle { width: 1920, height: 1080 },
                        min: Rectangle { width: 1, height: 1 },
                        max: Rectangle { width: 8192, height: 4320 },
                    },
                ))),
            ),
            Property::new(
                FormatProperties::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::None(Fraction { num: 0, denom: 1 }),
                ))),
            ),
        ],
    });

    let (cursor, _) =
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &obj).expect("format pod");
    cursor.into_inner()
}

pub fn list_monitors() -> Vec<MonitorInfo> {
    vec![MonitorInfo {
        index: 0,
        label: "Default (select in portal dialog)".to_string(),
    }]
}

pub fn start(
    _monitor_index: usize,
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let rt = tokio::runtime::Handle::try_current()
        .map_err(|e| format!("no tokio runtime available: {e}"))?;

    let (pw_tx, pw_rx) =
        std::sync::mpsc::sync_channel::<Result<(i32, u32, u32, u32), String>>(1);

    rt.spawn(async move {
        let _ = pw_tx.send(acquire_portal().await);
    });

    std::thread::spawn(move || match pw_rx.recv() {
        Ok(Ok((fd, node_id, width, height))) => {
            run_pipewire(fd, node_id, width, height, frame_slot, stop);
        }
        Ok(Err(e)) => log::error!("Screen capture portal failed: {e}"),
        Err(_) => log::error!("Screen capture portal channel closed unexpectedly"),
    });

    Ok(())
}

async fn acquire_portal() -> Result<(i32, u32, u32, u32), String> {
    use ashpd::desktop::PersistMode;
    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};

    let proxy = Screencast::new().await.map_err(|e| format!("Screencast::new: {e}"))?;
    let session = proxy
        .create_session()
        .await
        .map_err(|e| format!("create_session: {e}"))?;

    proxy
        .select_sources(
            &session,
            CursorMode::Hidden,
            SourceType::Monitor.into(),
            false,
            None,
            PersistMode::DoNot,
        )
        .await
        .map_err(|e| format!("select_sources: {e}"))?;

    let response = proxy
        .start(&session, &ashpd::WindowIdentifier::default())
        .await
        .map_err(|e| format!("portal start: {e}"))?
        .response()
        .map_err(|e| format!("portal response: {e}"))?;

    let stream = response.streams().first().ok_or("portal returned no streams")?;
    let node_id = stream.pipe_wire_node_id();

    // Some compositors return (0, 0) when size isn't known ahead of negotiation;
    // treat that the same as None and fall back to a sane default.
    let raw_size = stream.size();
    let (width, height) = raw_size
        .filter(|&(w, h)| w > 0 && h > 0)
        .map(|s| (s.0 as u32, s.1 as u32))
        .unwrap_or((1920, 1080));
    log::info!("PipeWire portal: node_id={node_id} portal_size={raw_size:?} using={width}x{height}");

    let fd: OwnedFd = proxy
        .open_pipe_wire_remote(&session)
        .await
        .map_err(|e| format!("open_pipe_wire_remote: {e}"))?;

    Ok((fd.into_raw_fd(), node_id, width, height))
}

fn run_pipewire(
    pw_fd: i32,
    node_id: u32,
    width: u32,
    height: u32,
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
) {
    pipewire::init();

    let ml = match pipewire::main_loop::MainLoop::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            log::error!("PipeWire mainloop: {e}");
            return;
        }
    };

    let ctx = match pipewire::context::Context::new(&ml) {
        Ok(c) => c,
        Err(e) => {
            log::error!("PipeWire context: {e}");
            return;
        }
    };

    let core = match ctx.connect_fd(unsafe { OwnedFd::from_raw_fd(pw_fd) }, None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("PipeWire connect_fd: {e}");
            return;
        }
    };

    let stream = pipewire::stream::Stream::new(
        &core,
        "halod-screen",
        pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE     => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE     => "Screen",
        },
    );
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            log::error!("PipeWire stream: {e}");
            return;
        }
    };

    let frame_slot_c = Arc::clone(&frame_slot);

    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |s, ()| {
            let Some(mut buf) = s.dequeue_buffer() else { return };
            let datas = buf.datas_mut();
            let Some(d) = datas.first_mut() else { return };

            let dtype = d.type_();
            // Stride can be padded past width * 4; fall back to packed if unreported.
            let stride = {
                let s = d.chunk().stride();
                if s > 0 { s as u32 } else { width * 4 }
            };
            let size = d.chunk().size() as usize;
            let expected = (stride * height) as usize;
            log::trace!(
                "PipeWire frame: type={dtype:?} size={size} stride={stride} \
                 expected={expected} portal_wh={width}x{height} \
                 chunk_offset={} flags={:?}",
                d.chunk().offset(),
                d.chunk().flags(),
            );
            if size < expected {
                log::warn!("PipeWire: frame too small ({size} < {expected}), dropping");
                return;
            }

            // MAP_BUFFERS maps MemFd/MemPtr but explicitly skips DmaBuf.
            // For DmaBuf we mmap the fd and use DMA_BUF_IOCTL_SYNC to wait for the
            // GPU fence before reading — without this the CPU sees partially-written data.
            let mmap_guard: Option<(*mut libc::c_void, usize, i32)>;
            let bytes: &[u8] = if dtype == DataType::DmaBuf {
                let raw = d.as_raw();
                let fd = raw.fd as i32;
                if fd < 0 {
                    log::warn!("PipeWire: DmaBuf frame has no fd, skipping");
                    return;
                }
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        size,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        fd,
                        raw.mapoffset as libc::off_t,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    log::warn!(
                        "PipeWire: DmaBuf mmap failed: {}",
                        std::io::Error::last_os_error()
                    );
                    return;
                }
                // Wait for GPU writes to complete before reading.
                dmabuf_sync(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
                mmap_guard = Some((ptr, size, fd));
                unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }
            } else {
                mmap_guard = None;
                match d.data() {
                    Some(b) => &b[..size],
                    None => {
                        log::warn!("PipeWire: no data for buffer type {dtype:?}");
                        return;
                    }
                }
            };

            // Copy the raw BGRA bytes and release the mmap immediately.
            // blit_letterboxed handles the BGRA→linear conversion per-sampled-pixel,
            // so we never pay to convert the full 5K+ source resolution.
            let raw = bytes[..expected].to_vec();

            if let Some((ptr, len, fd)) = mmap_guard {
                dmabuf_sync(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
                unsafe { libc::munmap(ptr, len) };
            }

            log::trace!("PipeWire: stored frame {width}x{height} stride={stride} ({} bytes)", raw.len());
            *frame_slot_c.lock().unwrap() = Some(RawFrame {
                width,
                height,
                stride,
                data: Arc::new(raw),
                rotation: FrameRotation::None,
            });
        })
        .register()
        .expect("PipeWire stream listener");

    // Request video/raw in shared memory by omitting DRM modifiers.
    // Without a modifier the compositor uses MemFd (SHM), which MAP_BUFFERS can
    // map directly — avoiding the GPU tiling layout issues of DMA-BUF.
    let format_pod_bytes = build_enum_format_pod();
    let format_pod = unsafe {
        pipewire::spa::pod::Pod::from_raw(
            format_pod_bytes.as_ptr() as *const pipewire::spa::sys::spa_pod,
        )
    };

    stream
        .connect(
            pipewire::spa::utils::Direction::Input,
            Some(node_id),
            pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
            &mut [format_pod],
        )
        .expect("PipeWire stream connect");

    // Send the raw pointer as usize so the watcher thread (which needs Send) can quit the loop.
    let stop_c = Arc::clone(&stop);
    let ml_ptr = ml.as_raw_ptr() as usize;
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if stop_c.load(Ordering::Relaxed) {
            unsafe {
                pipewire::sys::pw_main_loop_quit(ml_ptr as *mut pipewire::sys::pw_main_loop);
            }
            break;
        }
    });

    ml.run();

    log::info!("Screen capture PipeWire thread exiting");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_monitors_returns_exactly_one_entry() {
        // On Linux, the portal dialog handles monitor selection, so we always
        // expose a single "Default" entry rather than enumerating physical outputs.
        let monitors = list_monitors();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].index, 0);
        assert!(!monitors[0].label.is_empty());
    }

    #[test]
    fn start_returns_error_without_tokio_runtime() {
        // In a plain (non-async) test there is no Tokio runtime, so
        // start() must fail early rather than panicking.
        let slot = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = start(0, slot, stop);
        assert!(result.is_err(), "start() should fail without a Tokio runtime");
    }
}
