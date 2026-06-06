use std::collections::VecDeque;
use halod_protocol::types::CanvasFrame;

/// Rolling FPS / dropped-frame counters for the canvas header.
pub(super) struct FrameStats {
    pub(super) history: VecDeque<(u64, u64)>,
    pub(super) total_dropped: u64,
    pub(super) last_frame_id: Option<u64>,
}

impl FrameStats {
    pub(super) fn new() -> Self {
        Self { history: VecDeque::new(), total_dropped: 0, last_frame_id: None }
    }

    pub(super) fn push(&mut self, frame: &CanvasFrame) {
        if let Some(last) = self.last_frame_id {
            if frame.frame_id > last + 1 {
                self.total_dropped += frame.frame_id - last - 1;
            }
        }
        self.last_frame_id = Some(frame.frame_id);
        self.history.push_back((frame.timestamp_ms, frame.frame_id));
        let cutoff = frame.timestamp_ms.saturating_sub(1000);
        while self.history.front().map_or(false, |(ts, _)| *ts < cutoff) {
            self.history.pop_front();
        }
    }

    pub(super) fn fps(&self) -> f32 {
        let n = self.history.len();
        if n < 2 { return 0.0; }
        let oldest = self.history.front().unwrap().0;
        let newest = self.history.back().unwrap().0;
        let elapsed_s = (newest - oldest) as f32 / 1000.0;
        if elapsed_s <= 0.0 { 0.0 } else { (n - 1) as f32 / elapsed_s }
    }
}
