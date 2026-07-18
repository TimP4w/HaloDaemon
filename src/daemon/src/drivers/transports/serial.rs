// SPDX-License-Identifier: GPL-3.0-or-later
//! A serial/COM byte-stream transport for plugins that talk to a device over a
//! UART (directly or through a USB-serial adapter) instead of a hardware bus.
//!
//! Like TCP, a serial line has no framing of its own: `read(size)` is
//! **read-exact** — it returns exactly `size` bytes or errors on timeout. Wire
//! protocols built on this transport read a fixed header, then exactly the
//! payload length the header declares.
//!
//! Two extras beyond the plain byte stream, both opt-in from the manifest:
//! - **Events** — when the plugin declares an `event()` callback, a reader
//!   thread on a cloned handle delivers unsolicited inbound bytes through the
//!   shared [`Transport`] event path (`drain_events`), exactly like HID.
//! - **Reconnect** — on an I/O failure the port is reopened once with the same
//!   settings before the error surfaces, so an unplug/replug of a USB adapter
//!   recovers without tearing down the worker.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};
use serialport::SerialPort;

use crate::drivers::Metered;
use crate::plugin::manifest::{SerialConfig, SerialParity};

use super::{Transport, TransportEvent};

const EVENT_QUEUE_CAPACITY: usize = 256;
const EVENT_ENDPOINT: &str = "serial";
/// The event reader's per-read timeout — short so a drained queue reflects the
/// live line quickly without busy-spinning the reader thread.
const EVENT_READ_TIMEOUT: Duration = Duration::from_millis(100);
/// Pause between event-reader reopen attempts after a disconnect, so a device
/// that stays absent doesn't spin the thread.
const RECONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Enumerate the host's serial ports for the GUI's `serial_port` config-field
/// dropdown. USB adapters are labelled by product/manufacturer, and their stored
/// value is a replug-stable `/dev/serial/by-id` path when one resolves.
pub fn list_ports() -> Vec<halod_shared::types::SerialPortInfo> {
    use halod_shared::types::SerialPortInfo;
    let mut ports: Vec<SerialPortInfo> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|port| {
            let descr = match &port.port_type {
                serialport::SerialPortType::UsbPort(info) => info
                    .product
                    .clone()
                    .or_else(|| info.manufacturer.clone())
                    .unwrap_or_default(),
                _ => String::new(),
            };
            let value = stable_by_id(&port.port_name).unwrap_or_else(|| port.port_name.clone());
            let label = if descr.is_empty() {
                port.port_name.clone()
            } else {
                format!("{descr} ({})", port.port_name)
            };
            SerialPortInfo { value, label }
        })
        .collect();
    ports.sort_by(|a, b| a.label.cmp(&b.label));
    ports
}

/// Resolve a device path (e.g. `/dev/ttyUSB0`) to its replug-stable
/// `/dev/serial/by-id/...` symlink, so a stored value survives re-enumeration.
#[cfg(target_os = "linux")]
fn stable_by_id(port_name: &str) -> Option<String> {
    let target = std::fs::canonicalize(port_name).ok()?;
    let entries = std::fs::read_dir("/dev/serial/by-id").ok()?;
    for entry in entries.flatten() {
        let link = entry.path();
        if std::fs::canonicalize(&link).ok()? == target {
            return Some(link.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn stable_by_id(_port_name: &str) -> Option<String> {
    None
}

/// Line-control operations unique to a serial port, exposed to Lua through
/// [`Transport::as_serial`] the same way HID-only ops go through `as_hid`.
pub trait SerialControl: Send + Sync {
    fn set_dtr(&self, level: bool) -> Result<()>;
    fn set_rts(&self, level: bool) -> Result<()>;
    /// Assert BREAK for `duration_ms`, then clear it.
    fn send_break(&self, duration_ms: u64) -> Result<()>;
    /// Discard any buffered inbound bytes (a stale-frame reset).
    fn flush_input(&self) -> Result<()>;
}

/// The owned parameters needed to (re)open the port, so a reconnect can rebuild
/// an identical handle without re-consulting the manifest.
#[derive(Clone)]
struct SerialSettings {
    path: String,
    baud: u32,
    data_bits: serialport::DataBits,
    parity: serialport::Parity,
    stop_bits: serialport::StopBits,
    timeout: Duration,
    dtr: Option<bool>,
    rts: Option<bool>,
    flush_on_open: bool,
}

impl SerialSettings {
    fn from_config(path: &str, config: &SerialConfig) -> Self {
        let data_bits = match config.data_bits {
            5 => serialport::DataBits::Five,
            6 => serialport::DataBits::Six,
            7 => serialport::DataBits::Seven,
            _ => serialport::DataBits::Eight,
        };
        let parity = match config.parity {
            SerialParity::None => serialport::Parity::None,
            SerialParity::Odd => serialport::Parity::Odd,
            SerialParity::Even => serialport::Parity::Even,
        };
        let stop_bits = if config.stop_bits == 2 {
            serialport::StopBits::Two
        } else {
            serialport::StopBits::One
        };
        Self {
            path: path.to_owned(),
            baud: config.baud,
            data_bits,
            parity,
            stop_bits,
            timeout: Duration::from_millis(config.read_timeout_ms.max(1)),
            dtr: config.dtr,
            rts: config.rts,
            flush_on_open: config.flush_on_open,
        }
    }

    fn open(&self) -> Result<Box<dyn SerialPort>> {
        let mut port = serialport::new(&self.path, self.baud)
            .data_bits(self.data_bits)
            .parity(self.parity)
            .stop_bits(self.stop_bits)
            .timeout(self.timeout)
            .open()
            .with_context(|| format!("opening serial port '{}'", self.path))?;
        if let Some(level) = self.dtr {
            port.write_data_terminal_ready(level)
                .context("setting DTR")?;
        }
        if let Some(level) = self.rts {
            port.write_request_to_send(level).context("setting RTS")?;
        }
        if self.flush_on_open {
            port.clear(serialport::ClearBuffer::Input)
                .context("clearing serial input")?;
        }
        Ok(port)
    }
}

/// Bounded queue of unsolicited inbound bytes plus a wake channel, mirroring the
/// HID event queue so the worker's event loop drives HID and serial identically.
struct SerialEvents {
    bytes: StdMutex<VecDeque<TransportEvent>>,
    wake: tokio::sync::watch::Sender<u64>,
    listening: AtomicBool,
}

impl SerialEvents {
    fn new() -> Self {
        let (wake, _) = tokio::sync::watch::channel(0);
        Self {
            bytes: StdMutex::new(VecDeque::with_capacity(EVENT_QUEUE_CAPACITY)),
            wake,
            listening: AtomicBool::new(false),
        }
    }

    fn bump_wake(&self) {
        let next = self.wake.borrow().wrapping_add(1);
        self.wake.send_replace(next);
    }

    fn push(&self, data: Vec<u8>) {
        let mut queue = self.bytes.lock().unwrap();
        if queue.len() == EVENT_QUEUE_CAPACITY {
            queue.pop_front();
            log::debug!("[SerialTransport] event queue full; dropping oldest event");
        }
        queue.push_back(TransportEvent {
            endpoint: EVENT_ENDPOINT,
            data,
        });
        drop(queue);
        self.bump_wake();
    }

    fn drain(&self, limit: usize) -> Vec<TransportEvent> {
        let mut queue = self.bytes.lock().unwrap();
        let count = queue.len().min(limit);
        let events = queue.drain(..count).collect();
        let remaining = !queue.is_empty();
        drop(queue);
        if remaining {
            self.bump_wake();
        }
        events
    }
}

pub struct SerialTransport {
    port: Metered<StdMutex<Box<dyn SerialPort>>>,
    settings: SerialSettings,
    events: Arc<SerialEvents>,
    events_enabled: bool,
    read_timeout: Duration,
    max_bytes: usize,
    reconnect: bool,
}

impl SerialTransport {
    /// Open the configured port with the manifest's line settings. Blocking — a
    /// real open can stall, so callers run it off the async runtime.
    pub fn open_blocking(path: &str, config: &SerialConfig) -> Result<Self> {
        let settings = SerialSettings::from_config(path, config);
        let port = settings.open()?;
        Ok(Self {
            port: Metered::new(
                StdMutex::new(port),
                config
                    .max_bytes_per_sec
                    .map(|max_bytes_per_sec| WriteRateLimit { max_bytes_per_sec }),
            ),
            read_timeout: settings.timeout,
            settings,
            events: Arc::new(SerialEvents::new()),
            events_enabled: config.events,
            max_bytes: config.max_bytes,
            reconnect: config.reconnect,
        })
    }

    fn check_len(&self, len: usize) -> Result<()> {
        if len > self.max_bytes {
            bail!(
                "serial payload {len} exceeds the declared max_bytes {}",
                self.max_bytes
            );
        }
        Ok(())
    }

    /// Reopen the port in place after a disconnect, so the *next* operation uses
    /// the fresh handle. Best-effort; the current operation still fails. A failed
    /// operation is never replayed.
    fn reopen(&self) -> Result<()> {
        let fresh = self.settings.open()?;
        *self.port.read_access().lock().unwrap() = fresh;
        Ok(())
    }

    /// On a disconnect-class failure (not a timeout), reopen in place so the next
    /// call recovers. No-op when reconnect is off or the error is transient.
    fn recover_if_disconnected(&self, error: &std::io::Error) {
        if self.reconnect && is_disconnect(error) {
            if let Err(e) = self.reopen() {
                log::debug!("serial reopen after disconnect failed: {e:#}");
            }
        }
    }

    fn read_exact_blocking(&self, size: usize) -> Result<Vec<u8>> {
        let timeout = self.read_timeout;
        let mut guard = self.port.read_access().lock().unwrap();
        let mut buf = vec![0u8; size];
        let mut filled = 0;
        while filled < size {
            match guard.read(&mut buf[filled..]) {
                Ok(0) => {
                    let eof = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
                    drop(guard);
                    self.recover_if_disconnected(&eof);
                    bail!("serial read returned EOF before {size} bytes ({filled}/{size})");
                }
                Ok(n) => filled += n,
                // A timeout is a normal absent/late response, never a disconnect:
                // fail fast without reopening (which would double the wait).
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    bail!("serial read timed out after {timeout:?} ({filled}/{size} bytes)")
                }
                Err(e) => {
                    drop(guard);
                    self.recover_if_disconnected(&e);
                    return Err(anyhow::Error::from(e).context("serial read failed"));
                }
            }
        }
        Ok(buf)
    }
}

/// Classify an I/O error as a device disconnect (worth reopening) vs. a
/// transient condition like a timeout. Deliberately excludes `TimedOut`,
/// `WouldBlock`, and `Interrupted`.
fn is_disconnect(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    if matches!(
        error.kind(),
        BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected | UnexpectedEof
    ) {
        return true;
    }
    // ENXIO(6), EIO(5), ENODEV(19): a removed serial device on Unix.
    matches!(error.raw_os_error(), Some(5 | 6 | 19))
}

#[async_trait]
impl Transport for SerialTransport {
    async fn write(&self, data: &[u8]) -> Result<()> {
        self.check_len(data.len())?;
        // Apply the write-rate gate, then do blocking I/O on the worker's own
        // thread (the caller `block_on`s this), mirroring the USB transport's
        // documented blocking write contract.
        self.port.write_access(data.len()).await?;
        let mut guard = self.port.read_access().lock().unwrap();
        let result = match guard.write_all(data) {
            Ok(()) => guard.flush(),
            Err(e) => Err(e),
        };
        if let Err(error) = result {
            drop(guard);
            // Never replay a partially transmitted write; just reopen for the
            // next call so a replug recovers transparently.
            self.recover_if_disconnected(&error);
            return Err(anyhow::Error::from(error).context("serial write failed"));
        }
        Ok(())
    }

    async fn read(&self, size: usize) -> Result<Vec<u8>> {
        self.check_len(size)?;
        self.read_exact_blocking(size)
    }

    fn as_serial(&self) -> Option<&dyn SerialControl> {
        Some(self)
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.port.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.port.set_limit(limit);
    }

    fn event_receiver(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        // No wake source unless the manifest opted into events, so a request/
        // reply device never spins up the competing event task.
        self.events_enabled.then(|| self.events.wake.subscribe())
    }

    async fn drain_events(&self, limit: usize) -> Result<Vec<TransportEvent>> {
        Ok(self.events.drain(limit))
    }

    fn enable_event_listener(&self) -> Result<()> {
        if !self.events_enabled {
            // A plugin declaring event() while the manifest left events off does
            // not get a reader — declining here keeps the reply stream intact.
            return Ok(());
        }
        if self.events.listening.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let reader = clone_reader(&self.port)?;
        let events = Arc::downgrade(&self.events);
        let port = self.port.clone();
        let settings = self.settings.clone();
        let reconnect = self.reconnect;
        std::thread::Builder::new()
            .name(format!("serial-events:{}", self.settings.path))
            .spawn(move || serial_event_loop(reader, port, settings, reconnect, events))
            .context("spawning the serial event reader")?;
        Ok(())
    }
}

/// Clone the primary handle into a short-timeout reader for the event thread.
fn clone_reader(port: &Metered<StdMutex<Box<dyn SerialPort>>>) -> Result<Box<dyn SerialPort>> {
    let mut reader = port
        .read_access()
        .lock()
        .unwrap()
        .try_clone()
        .context("cloning the serial handle for the event reader")?;
    reader
        .set_timeout(EVENT_READ_TIMEOUT)
        .context("setting event-reader timeout")?;
    Ok(reader)
}

/// Read unsolicited bytes until the owning transport drops (its `Arc` gone). On a
/// disconnect it either reopens the port and re-clones the reader (when the
/// transport reconnects) or clears the listening flag and exits so a later
/// `enable_event_listener` can restart cleanly.
fn serial_event_loop(
    mut reader: Box<dyn SerialPort>,
    port: Metered<StdMutex<Box<dyn SerialPort>>>,
    settings: SerialSettings,
    reconnect: bool,
    events: Weak<SerialEvents>,
) {
    let mut buf = [0u8; 512];
    loop {
        let Some(shared) = events.upgrade() else {
            return;
        };
        let disconnected = match reader.read(&mut buf) {
            Ok(0) => true,
            Ok(n) => {
                shared.push(buf[..n].to_vec());
                false
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                ) =>
            {
                false
            }
            Err(_) => true,
        };
        if !disconnected {
            continue;
        }
        if !reconnect {
            shared.listening.store(false, Ordering::SeqCst);
            return;
        }
        // Reopen the primary and re-clone so events survive a replug. Release the
        // Arc while waiting so a dropped transport can end this thread.
        drop(shared);
        std::thread::sleep(RECONNECT_BACKOFF);
        if events.upgrade().is_none() {
            return;
        }
        if let Ok(opened) = settings.open() {
            *port.read_access().lock().unwrap() = opened;
            if let Ok(new_reader) = clone_reader(&port) {
                reader = new_reader;
            }
        }
    }
}

impl SerialControl for SerialTransport {
    fn set_dtr(&self, level: bool) -> Result<()> {
        let mut guard = self.port.read_access().lock().unwrap();
        guard
            .write_data_terminal_ready(level)
            .context("setting DTR")
    }

    fn set_rts(&self, level: bool) -> Result<()> {
        let mut guard = self.port.read_access().lock().unwrap();
        guard.write_request_to_send(level).context("setting RTS")
    }

    fn send_break(&self, duration_ms: u64) -> Result<()> {
        {
            let guard = self.port.read_access().lock().unwrap();
            guard.set_break().context("asserting BREAK")?;
        }
        // Bound the hold so a plugin can't wedge the worker thread on BREAK.
        std::thread::sleep(Duration::from_millis(duration_ms.min(5_000)));
        let guard = self.port.read_access().lock().unwrap();
        guard.clear_break().context("clearing BREAK")
    }

    fn flush_input(&self) -> Result<()> {
        let guard = self.port.read_access().lock().unwrap();
        guard
            .clear(serialport::ClearBuffer::Input)
            .context("clearing serial input")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_queue_is_bounded_and_drops_oldest() {
        let events = SerialEvents::new();
        for value in 0..=EVENT_QUEUE_CAPACITY {
            events.push(vec![value as u8]);
        }
        let drained = events.drain(EVENT_QUEUE_CAPACITY + 1);
        assert_eq!(drained.len(), EVENT_QUEUE_CAPACITY);
        // The very first push was evicted; the queue kept the newest ones.
        assert_eq!(
            drained.last().unwrap().data,
            vec![EVENT_QUEUE_CAPACITY as u8]
        );
    }

    #[test]
    fn drain_respects_the_limit_and_preserves_order() {
        let events = SerialEvents::new();
        events.push(vec![1]);
        events.push(vec![2]);
        events.push(vec![3]);
        let first = events.drain(2);
        assert_eq!(
            first.iter().map(|e| e.data.clone()).collect::<Vec<_>>(),
            [vec![1], vec![2]]
        );
        let rest = events.drain(10);
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].data, vec![3]);
    }

    #[test]
    fn timeouts_and_transient_errors_are_not_disconnects() {
        use std::io::{Error, ErrorKind};
        // A normal absent/late response must never reopen the port.
        assert!(!is_disconnect(&Error::from(ErrorKind::TimedOut)));
        assert!(!is_disconnect(&Error::from(ErrorKind::WouldBlock)));
        assert!(!is_disconnect(&Error::from(ErrorKind::Interrupted)));
        // Disconnect-class errors do.
        assert!(is_disconnect(&Error::from(ErrorKind::BrokenPipe)));
        assert!(is_disconnect(&Error::from(ErrorKind::UnexpectedEof)));
        assert!(is_disconnect(&Error::from(ErrorKind::NotConnected)));
        // A removed USB serial device surfaces ENODEV.
        assert!(is_disconnect(&Error::from_raw_os_error(19)));
    }

    #[test]
    fn settings_map_manifest_line_parameters() {
        let config = SerialConfig {
            baud: 9600,
            data_bits: 7,
            parity: SerialParity::Even,
            stop_bits: 2,
            ..SerialConfig::default()
        };
        let settings = SerialSettings::from_config("/dev/null", &config);
        assert_eq!(settings.baud, 9600);
        assert!(matches!(settings.data_bits, serialport::DataBits::Seven));
        assert!(matches!(settings.parity, serialport::Parity::Even));
        assert!(matches!(settings.stop_bits, serialport::StopBits::Two));
    }
}
