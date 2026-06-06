use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use halod_protocol::types::LogEntry;

const BUFFER_CAP: usize = 500;

/// Wraps env_logger's Logger and also appends entries to a shared ring-buffer
/// so the IPC layer can broadcast recent log lines to connected UI clients.
pub struct BufferingLogger {
    inner: env_logger::Logger,
    buffer: Arc<Mutex<VecDeque<LogEntry>>>,
}

impl BufferingLogger {
    /// Build from an already-configured env_logger Logger.
    pub fn new(inner: env_logger::Logger, buffer: Arc<Mutex<VecDeque<LogEntry>>>) -> Self {
        Self { inner, buffer }
    }
}

impl log::Log for BufferingLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        self.inner.log(record);

        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let entry = LogEntry {
            level: record.level().to_string(),
            target: record.target().to_string(),
            message: record.args().to_string(),
            timestamp_ms,
        };

        if let Ok(mut buf) = self.buffer.lock() {
            if buf.len() >= BUFFER_CAP {
                buf.pop_front();
            }
            buf.push_back(entry);
        }
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_protocol::types::LogEntry;

    fn make_entry(msg: &str) -> LogEntry {
        LogEntry { level: "INFO".into(), target: "test".into(), message: msg.into(), timestamp_ms: 0 }
    }

    fn push(buf: &Arc<Mutex<VecDeque<LogEntry>>>, entry: LogEntry) {
        let mut b = buf.lock().unwrap();
        if b.len() >= BUFFER_CAP {
            b.pop_front();
        }
        b.push_back(entry);
    }

    #[test]
    fn buffer_stores_entries_in_order() {
        let buf = Arc::new(Mutex::new(VecDeque::new()));
        push(&buf, make_entry("first"));
        push(&buf, make_entry("second"));
        let b = buf.lock().unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].message, "first");
        assert_eq!(b[1].message, "second");
    }

    #[test]
    fn buffer_evicts_oldest_at_capacity() {
        let buf = Arc::new(Mutex::new(VecDeque::new()));
        for i in 0..BUFFER_CAP {
            push(&buf, make_entry(&i.to_string()));
        }
        // At capacity; pushing one more should evict "0".
        push(&buf, make_entry("overflow"));
        let b = buf.lock().unwrap();
        assert_eq!(b.len(), BUFFER_CAP);
        assert_eq!(b.front().unwrap().message, "1");
        assert_eq!(b.back().unwrap().message, "overflow");
    }
}
