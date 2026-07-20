// SPDX-License-Identifier: GPL-3.0-or-later
use std::{
    os::fd::{FromRawFd, IntoRawFd, OwnedFd},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex,
    },
};

/// Cached DmaBuf mmap — avoids mmap+munmap per frame (~33 MB for 4K). The
/// mapping is stable as long as `fd` stays the same; only re-mmap on change.
struct MmapCache {
    ptr: *mut libc::c_void,
    len: usize,
    fd: i32,
    identity: Option<(libc::dev_t, libc::ino_t)>,
}

impl MmapCache {
    #[cfg(test)]
    fn new(ptr: *mut libc::c_void, len: usize, fd: i32) -> Self {
        Self {
            ptr,
            len,
            fd,
            identity: None,
        }
    }

    /// Ensure the mapping covers `fd` (with size `len` at offset 0). Reuses
    /// an existing mapping when the fd matches, otherwise unmaps the old and
    /// maps the new. Returns a pointer to the mapped memory.
    fn ensure(&mut self, fd: i32, len: usize) -> Result<*mut libc::c_void, String> {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return Err(format!("fstat failed: {}", std::io::Error::last_os_error()));
        }
        let stat = unsafe { stat.assume_init() };
        let identity = (stat.st_dev, stat.st_ino);
        if fd == self.fd && self.identity == Some(identity) && len <= self.len {
            // Same fd, mapping still valid.
            return Ok(self.ptr);
        }
        // Unmap the old before mapping the new.
        self.unmap();
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
        }
        self.ptr = ptr;
        self.len = len;
        self.fd = fd;
        self.identity = Some(identity);
        Ok(ptr)
    }

    fn unmap(&mut self) {
        if !self.ptr.is_null() {
            unsafe { libc::munmap(self.ptr, self.len) };
            self.ptr = std::ptr::null_mut();
            self.len = 0;
            self.fd = -1;
            self.identity = None;
        }
    }
}

impl Drop for MmapCache {
    fn drop(&mut self) {
        self.unmap();
    }
}

// SAFETY: MmapCache is only accessed while holding its parent Mutex.
unsafe impl Send for MmapCache {}

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
/// Omitting the modifier tells the compositor to use MemFd (shared memory)
/// rather than DMA-BUF, so MAP_BUFFERS can map the data directly.
fn build_enum_format_pod() -> Result<Vec<u8>, String> {
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
                        default: Rectangle {
                            width: 1920,
                            height: 1080,
                        },
                        min: Rectangle {
                            width: 1,
                            height: 1,
                        },
                        max: Rectangle {
                            width: 8192,
                            height: 4320,
                        },
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

    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &obj)
        .map_err(|e| format!("PodSerializer::serialize: {e}"))?;
    Ok(cursor.into_inner())
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

    let (pw_tx, pw_rx) = std::sync::mpsc::sync_channel::<Result<(i32, u32, u32, u32), String>>(1);

    // The portal task owns the screencast session for its whole lifetime and
    // closes it when `stop` is set — dropping the Session does NOT close it, so
    // without this the compositor keeps the capture (and its sharing indicator)
    // alive after the effect is switched away.
    let stop_portal = Arc::clone(&stop);
    // Retain the handle so a panic inside `run_portal` produces a visible log
    // entry rather than silently closing `pw_tx`.
    let portal_handle = rt.spawn(run_portal(stop_portal, pw_tx));
    rt.spawn(async move {
        if let Err(e) = portal_handle.await {
            log::error!("Screen capture portal task panicked: {e}");
        }
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

/// Negotiated screencast: the proxy and session are returned so the caller can
/// keep them alive and explicitly close the session when capture stops.
type Portal = (
    ashpd::desktop::screencast::Screencast,
    ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>,
    i32,
    u32,
    u32,
    u32,
);

/// Runs the portal handshake, hands the PipeWire fd/size to the capture thread,
/// then holds the session open until `stop` is set and closes it — this is what
/// tears down the screencast on the compositor and clears the sharing indicator.
async fn run_portal(
    stop: Arc<AtomicBool>,
    pw_tx: std::sync::mpsc::SyncSender<Result<(i32, u32, u32, u32), String>>,
) {
    let negotiated = tokio::time::timeout(std::time::Duration::from_secs(60), acquire_portal())
        .await
        .map_err(|_| "portal dialog timed out after 60 s".to_string())
        .and_then(|r| r);

    let (proxy, session, fd, node_id, width, height) = match negotiated {
        Ok(v) => v,
        Err(e) => {
            let _ = pw_tx.send(Err(e));
            return;
        }
    };

    if pw_tx.send(Ok((fd, node_id, width, height))).is_err() {
        // Capture thread gone before it received the fd; still close the
        // session so we don't leak the screencast.
        let _ = session.close().await;
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    if let Err(e) = session.close().await {
        log::warn!("Screen capture: portal session close failed: {e}");
    }
    drop(proxy);
    log::info!("Screen capture portal session closed");
}

async fn acquire_portal() -> Result<Portal, String> {
    use ashpd::desktop::screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
        StartCastOptions,
    };
    use ashpd::desktop::{CreateSessionOptions, PersistMode};

    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("Screencast::new: {e}"))?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| format!("create_session: {e}"))?;

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Hidden)
                .set_sources(Some(SourceType::Monitor.into()))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .map_err(|e| format!("select_sources: {e}"))?;

    let response = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| format!("portal start: {e}"))?
        .response()
        .map_err(|e| format!("portal response: {e}"))?;

    let stream = response
        .streams()
        .first()
        .ok_or("portal returned no streams")?;
    let node_id = stream.pipe_wire_node_id();

    // Some compositors return (0, 0) when size isn't known ahead of negotiation;
    // treat that the same as None and fall back to a sane default.
    let raw_size = stream.size();
    let (width, height) = raw_size
        .filter(|&(w, h)| w > 0 && h > 0)
        .map(|s| (s.0 as u32, s.1 as u32))
        .unwrap_or((1920, 1080));
    log::info!(
        "PipeWire portal: node_id={node_id} portal_size={raw_size:?} using={width}x{height}"
    );

    let fd: OwnedFd = proxy
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(|e| format!("open_pipe_wire_remote: {e}"))?;

    Ok((proxy, session, fd.into_raw_fd(), node_id, width, height))
}

/// Typed, Send-safe wrapper around `*mut pw_main_loop`.
///
/// The raw pointer is not `Send` by default; we wrap it so the Mutex-protected
/// hand-off between the PipeWire thread and the stop-watcher thread is
/// type-checked rather than erasing the pointer to `usize`.
///
/// SAFETY invariant: the pointer is only dereferenced while the Mutex is held
/// AND the PipeWire main thread has not yet cleared it to `None` (it clears
/// before `ml` drops), so it is always valid at the moment of use.
struct MainLoopPtr(*mut pipewire::sys::pw_main_loop);
// SAFETY: pw_main_loop_quit is the only operation performed on this pointer,
// and it is protected by the Mutex that prevents concurrent access.
unsafe impl Send for MainLoopPtr {}

fn run_pipewire(
    pw_fd: i32,
    node_id: u32,
    width: u32,
    height: u32,
    frame_slot: Arc<Mutex<Option<RawFrame>>>,
    stop: Arc<AtomicBool>,
) {
    // Take ownership before any fallible PipeWire construction so every early
    // return closes the portal descriptor.
    let pw_fd = unsafe { OwnedFd::from_raw_fd(pw_fd) };
    pipewire::init();

    let ml = match pipewire::main_loop::MainLoopBox::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            log::error!("PipeWire mainloop: {e}");
            return;
        }
    };

    let ctx = match pipewire::context::ContextBox::new(ml.loop_(), None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("PipeWire context: {e}");
            return;
        }
    };

    let core = match ctx.connect_fd(pw_fd, None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("PipeWire connect_fd: {e}");
            return;
        }
    };

    let stream = pipewire::stream::StreamBox::new(
        &core,
        crate::constants::SCREEN_CAPTURE_CLIENT,
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
    let mmap_cache = Arc::new(Mutex::new(MmapCache {
        ptr: std::ptr::null_mut(),
        len: 0,
        fd: -1,
        identity: None,
    }));

    // Actual delivered frame size. Seeded from the portal's reported size as a
    // fallback, but the portal reports *logical* dimensions on scaled/HiDPI
    // outputs while PipeWire delivers *physical* pixels — trusting the portal
    // size makes the blit read only the top-left crop of the real buffer.
    // `param_changed` gives us the true negotiated size; use that when it lands.
    let frame_w = Arc::new(AtomicU32::new(width));
    let frame_h = Arc::new(AtomicU32::new(height));
    let (frame_w_pc, frame_h_pc) = (Arc::clone(&frame_w), Arc::clone(&frame_h));
    let mut last_copied_frame = None::<std::time::Instant>;

    let _listener = match stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_, (), id, pod| {
            use pipewire::spa::param::{
                format::{MediaSubtype, MediaType},
                format_utils::parse_format,
                video::VideoInfoRaw,
                ParamType,
            };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = pod else { return };
            let Ok((mtype, msubtype)) = parse_format(pod) else {
                return;
            };
            if mtype != MediaType::Video || msubtype != MediaSubtype::Raw {
                return;
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(pod).is_err() {
                return;
            }
            let size = info.size();
            if size.width > 0 && size.height > 0 {
                frame_w_pc.store(size.width, Ordering::Relaxed);
                frame_h_pc.store(size.height, Ordering::Relaxed);
                log::info!(
                    "PipeWire negotiated video size {}x{} (portal reported {}x{})",
                    size.width,
                    size.height,
                    width,
                    height
                );
            }
        })
        .process(move |s, ()| {
            let now = std::time::Instant::now();
            if last_copied_frame
                .is_some_and(|last| now.duration_since(last) < std::time::Duration::from_millis(33))
            {
                return;
            }
            last_copied_frame = Some(now);
            let Some(mut buf) = s.dequeue_buffer() else {
                return;
            };
            let datas = buf.datas_mut();
            let Some(d) = datas.first_mut() else { return };

            // Prefer the negotiated size from param_changed; fall back to the
            // portal size only until the first format is negotiated.
            let width = frame_w.load(Ordering::Relaxed);
            let height = frame_h.load(Ordering::Relaxed);

            let dtype = d.type_();
            // Stride can be padded past width * 4; fall back to packed if unreported.
            let stride = {
                let s = d.chunk().stride();
                if s > 0 {
                    s as u32
                } else {
                    width * 4
                }
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

            // MAP_BUFFERS skips DmaBuf, so we mmap the fd and DMA_BUF_IOCTL_SYNC
            // to wait for the GPU fence before reading (else CPU sees partial data).
            // The mmap is cached across frames to avoid ~33 MB mmap+munmap per
            // 4K frame; only re-mmap when the fd changes.
            let mmap_guard: Option<(i32, bool)>;
            let bytes: &[u8] = if dtype == DataType::DmaBuf {
                let raw = d.as_raw();
                let fd = raw.fd as i32;
                if fd < 0 {
                    log::warn!("PipeWire: DmaBuf frame has no fd, skipping");
                    return;
                }
                let mut cache = mmap_cache.lock().unwrap();
                let ptr = match cache.ensure(fd, expected) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("PipeWire: DmaBuf mmap failed: {e}");
                        return;
                    }
                };
                dmabuf_sync(fd, DMA_BUF_SYNC_START | DMA_BUF_SYNC_READ);
                // Track that we need to end-sync (but NOT unmap — cache retains it).
                mmap_guard = Some((fd, true));
                // SAFETY: mmap succeeded and we mapped at least `expected` bytes.
                unsafe { std::slice::from_raw_parts(ptr as *const u8, expected) }
            } else {
                mmap_guard = None;
                match d.data() {
                    Some(b) => &b[..expected],
                    None => {
                        log::warn!("PipeWire: no data for buffer type {dtype:?}");
                        return;
                    }
                }
            };

            // Copy raw BGRA; blit_letterboxed converts BGRA→linear per sampled
            // pixel, not the full source.
            let raw = bytes.to_vec();

            if let Some((fd, _)) = mmap_guard {
                dmabuf_sync(fd, DMA_BUF_SYNC_END | DMA_BUF_SYNC_READ);
                // mmap is NOT unmapped here — the MmapCache retains it across frames.
            }

            log::trace!(
                "PipeWire: stored frame {width}x{height} stride={stride} ({} bytes)",
                raw.len()
            );
            *frame_slot_c.lock().unwrap_or_else(|e| e.into_inner()) = Some(RawFrame {
                width,
                height,
                stride,
                data: Arc::new(raw),
                rotation: FrameRotation::None,
            });
        })
        .register()
    {
        Ok(l) => l,
        Err(e) => {
            log::error!("PipeWire stream listener: {e}");
            return;
        }
    };

    let format_pod_bytes = match build_enum_format_pod() {
        Ok(b) => b,
        Err(e) => {
            log::error!("PipeWire format pod: {e}");
            return;
        }
    };
    let format_pod = unsafe {
        pipewire::spa::pod::Pod::from_raw(
            format_pod_bytes.as_ptr() as *const pipewire::spa::sys::spa_pod
        )
    };

    if let Err(e) = stream.connect(
        pipewire::spa::utils::Direction::Input,
        Some(node_id),
        pipewire::stream::StreamFlags::AUTOCONNECT | pipewire::stream::StreamFlags::MAP_BUFFERS,
        &mut [format_pod],
    ) {
        log::error!("PipeWire stream connect: {e}");
        return;
    }

    // The watcher thread shares a Mutex-protected pointer to the MainLoop so
    // it can call pw_main_loop_quit when the stop flag is set. The Mutex is
    // cleared before `ml` drops, preventing use-after-free if ml.run() exits
    // early for any reason (PipeWire daemon crash, stream error, etc.).
    let stop_c = Arc::clone(&stop);
    let ml_ptr: Arc<Mutex<Option<MainLoopPtr>>> =
        Arc::new(Mutex::new(Some(MainLoopPtr(ml.as_raw_ptr()))));
    let ml_ptr_watcher = Arc::clone(&ml_ptr);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if stop_c.load(Ordering::Relaxed) {
            // SAFETY: The Mutex guarantees the MainLoop is still alive: the
            // main thread clears ml_ptr to None before `ml` drops, and we hold
            // the lock here while dereferencing the pointer, so the two
            // operations cannot interleave.
            let guard = ml_ptr_watcher.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(MainLoopPtr(ptr)) = *guard {
                unsafe {
                    pipewire::sys::pw_main_loop_quit(ptr);
                }
            }
            break;
        }
    });

    ml.run();

    // Clear the pointer under the lock before `ml` drops so the watcher
    // thread cannot call pw_main_loop_quit on freed memory.
    *ml_ptr.lock().unwrap_or_else(|e| e.into_inner()) = None;

    log::info!("Screen capture PipeWire thread exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    #[test]
    fn list_monitors_returns_exactly_one_entry() {
        let monitors = list_monitors();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].index, 0);
        assert!(!monitors[0].label.is_empty());
    }

    #[test]
    fn start_returns_error_without_tokio_runtime() {
        let slot = Arc::new(Mutex::new(None));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let result = start(0, slot, stop);
        assert!(
            result.is_err(),
            "start() should fail without a Tokio runtime"
        );
    }

    #[test]
    fn mmap_cache_reuses_covering_mapping_and_remaps_when_it_grows() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(8192).unwrap();
        let fd = file.as_raw_fd();
        let mut cache = MmapCache::new(std::ptr::null_mut(), 0, -1);

        let first = cache.ensure(fd, 4096).unwrap();
        assert!(!first.is_null());
        assert_eq!(cache.len, 4096);
        assert_eq!(cache.ensure(fd, 1024).unwrap(), first);
        assert_eq!(cache.len, 4096, "a smaller frame must reuse the mapping");

        let grown = cache.ensure(fd, 8192).unwrap();
        assert!(!grown.is_null());
        assert_eq!(cache.len, 8192);
        assert_eq!(cache.fd, fd);

        cache.unmap();
        assert!(cache.ptr.is_null());
        assert_eq!((cache.len, cache.fd), (0, -1));
    }

    #[test]
    fn mmap_cache_rejects_an_invalid_fd_without_retaining_state() {
        let mut cache = MmapCache::new(std::ptr::null_mut(), 0, -1);
        assert!(cache.ensure(-1, 4096).is_err());
        assert!(cache.ptr.is_null());
        assert_eq!((cache.len, cache.fd), (0, -1));
    }
}
