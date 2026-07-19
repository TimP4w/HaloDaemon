// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc, Mutex,
};

use super::{dsp, dsp::SpectrumAnalyzer, AudioHandle, StopGuard, SESSION_RETRY_MS};

/// Typed, Send-safe wrapper around `*mut pw_main_loop` — mirrors the pattern
/// in `engines/rgb_engine/canvas/screen_capture/linux.rs`.
struct MainLoopPtr(*mut pipewire::sys::pw_main_loop);
// SAFETY: pw_main_loop_quit is the only operation performed on this pointer,
// and it is protected by the Mutex that prevents concurrent access.
unsafe impl Send for MainLoopPtr {}

/// Spawns the PipeWire loopback capture thread. Returns immediately. The
/// thread runs capture sessions until no consumer has read for the idle
/// window, retrying failed sessions (rate-limited) while readers remain.
pub fn start(handle: Arc<AudioHandle>) {
    std::thread::spawn(move || {
        let _guard = StopGuard(Arc::clone(&handle));
        pipewire::init();
        log::info!("Audio capture: PipeWire thread started");
        loop {
            if handle.idle_expired(super::monotonic_ms()) {
                break;
            }
            run_session(&handle);
            std::thread::sleep(std::time::Duration::from_millis(SESSION_RETRY_MS));
        }
    });
}

fn run_session(handle: &Arc<AudioHandle>) {
    let ml = match pipewire::main_loop::MainLoopBox::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            log::error!("Audio capture: PipeWire mainloop: {e}");
            return;
        }
    };

    let ctx = match pipewire::context::ContextBox::new(ml.loop_(), None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Audio capture: PipeWire context: {e}");
            return;
        }
    };

    let core = match ctx.connect(None) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Audio capture: PipeWire connect: {e}");
            return;
        }
    };

    let quit_flag = Arc::new(AtomicBool::new(false));

    // A core error means the server rejected or lost our session; end it and
    // let the session loop retry with a fresh connection.
    let quit_flag_err = Arc::clone(&quit_flag);
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, res, message| {
            log::warn!("Audio capture: PipeWire core error on {id} (res {res}): {message}");
            quit_flag_err.store(true, Ordering::Relaxed);
        })
        .register();

    let stream = pipewire::stream::StreamBox::new(
        &core,
        "halod-audio",
        pipewire::properties::properties! {
            *pipewire::keys::MEDIA_TYPE     => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE     => "Music",
            *pipewire::keys::STREAM_CAPTURE_SINK => "true",
        },
    );
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            log::error!("Audio capture: PipeWire stream: {e}");
            return;
        }
    };

    let sample_rate = Arc::new(AtomicU32::new(0));
    let sample_rate_pc = Arc::clone(&sample_rate);
    let channels = Arc::new(AtomicU32::new(0));
    let channels_pc = Arc::clone(&channels);

    let analyzer: Arc<Mutex<Option<SpectrumAnalyzer>>> = Arc::new(Mutex::new(None));
    let analyzer_pc = Arc::clone(&analyzer);
    let handle_process = Arc::clone(handle);

    let _listener = match stream
        .add_local_listener_with_user_data(())
        .param_changed(move |_, (), id, pod| {
            use pipewire::spa::param::{
                audio::AudioInfoRaw,
                format::{MediaSubtype, MediaType},
                format_utils::parse_format,
                ParamType,
            };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = pod else { return };
            let Ok((mtype, msubtype)) = parse_format(pod) else {
                return;
            };
            if mtype != MediaType::Audio || msubtype != MediaSubtype::Raw {
                return;
            }
            let mut info = AudioInfoRaw::new();
            if info.parse(pod).is_err() {
                return;
            }
            let rate = info.rate();
            let chans = info.channels();
            if rate > 0 {
                sample_rate_pc.store(rate, Ordering::Relaxed);
                channels_pc.store(chans.max(1), Ordering::Relaxed);
                *analyzer_pc.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SpectrumAnalyzer::new(rate));
                log::info!("Audio capture: negotiated {rate} Hz, {chans} channel(s)");
            }
        })
        .process(move |s, ()| {
            let Some(mut buf) = s.dequeue_buffer() else {
                return;
            };
            let datas = buf.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let chunk_size = d.chunk().size() as usize;
            let Some(bytes) = d.data() else { return };
            let bytes = &bytes[..chunk_size.min(bytes.len())];

            let chans = channels.load(Ordering::Relaxed).max(1) as usize;
            let mono = dsp::downmix_to_mono(dsp::le_f32_samples(bytes), chans);

            let mut guard = analyzer.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(a) = guard.as_mut() {
                for frame in a.feed(&mono) {
                    handle_process.publish(frame);
                }
            }
        })
        .register()
    {
        Ok(l) => l,
        Err(e) => {
            log::error!("Audio capture: PipeWire stream listener: {e}");
            return;
        }
    };

    let format_pod_bytes = match build_enum_format_pod() {
        Ok(b) => b,
        Err(e) => {
            log::error!("Audio capture: format pod: {e}");
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
        None,
        pipewire::stream::StreamFlags::AUTOCONNECT
            | pipewire::stream::StreamFlags::MAP_BUFFERS
            | pipewire::stream::StreamFlags::RT_PROCESS,
        &mut [format_pod],
    ) {
        log::error!("Audio capture: PipeWire stream connect: {e}");
        return;
    }

    let ml_ptr: Arc<Mutex<Option<MainLoopPtr>>> =
        Arc::new(Mutex::new(Some(MainLoopPtr(ml.as_raw_ptr()))));
    let ml_ptr_watcher = Arc::clone(&ml_ptr);
    let quit_flag_watcher = Arc::clone(&quit_flag);
    let handle_watcher = Arc::clone(handle);
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if quit_flag_watcher.load(Ordering::Relaxed)
            || handle_watcher.idle_expired(super::monotonic_ms())
        {
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

    *ml_ptr.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Builds an SPA_PARAM_EnumFormat pod for interleaved F32 audio, any rate.
fn build_enum_format_pod() -> Result<Vec<u8>, String> {
    use pipewire::spa::{
        param::{
            audio::AudioFormat,
            format::{FormatProperties, MediaSubtype, MediaType},
            ParamType,
        },
        pod::{serialize::PodSerializer, Object, Property, Value},
        utils::{Id, SpaTypes},
    };

    let obj = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                FormatProperties::MediaType.as_raw(),
                Value::Id(Id(MediaType::Audio.as_raw())),
            ),
            Property::new(
                FormatProperties::MediaSubtype.as_raw(),
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                FormatProperties::AudioFormat.as_raw(),
                Value::Id(Id(AudioFormat::F32LE.as_raw())),
            ),
        ],
    });

    let (cursor, _) = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &obj)
        .map_err(|e| format!("PodSerializer::serialize: {e}"))?;
    Ok(cursor.into_inner())
}
