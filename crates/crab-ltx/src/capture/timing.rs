//! Capture timing recorder and the engine's telemetry hooks.

use super::*;

pub(crate) struct TimingRecorder {
    started: Instant,
    active: Option<(TimingPhase, Instant)>,
    timing: crate::CaptureTiming,
}

impl TimingRecorder {
    pub(crate) fn new(started: Instant) -> Self {
        Self {
            started,
            active: None,
            timing: crate::CaptureTiming::default(),
        }
    }

    pub(crate) fn begin(&mut self, phase: TimingPhase, now: Instant) {
        if let Some((active, started)) = self.active.take() {
            self.add_phase(active, now.saturating_duration_since(started));
        }
        self.active = Some((phase, now));
    }

    pub(crate) fn end(&mut self, phase: TimingPhase, now: Instant) {
        if let Some((active, started)) = self.active
            && active == phase
        {
            self.add_phase(active, now.saturating_duration_since(started));
            self.active = None;
        }
    }

    pub(crate) fn add_wal_bytes(&mut self, bytes: u64) {
        self.timing.wal_bytes = self.timing.wal_bytes.saturating_add(bytes);
    }

    pub(crate) fn add_database_bytes(&mut self, bytes: u64) {
        self.timing.database_bytes = self.timing.database_bytes.saturating_add(bytes);
    }

    pub(crate) fn add_ltx_bytes(&mut self, bytes: u64) {
        self.timing.ltx_bytes = self.timing.ltx_bytes.saturating_add(bytes);
    }

    pub(crate) fn add_segment(&mut self) {
        self.timing.segment_count = self.timing.segment_count.saturating_add(1);
    }

    pub(crate) fn add_phase_nanos(&mut self, phase: TimingPhase, elapsed: u64) {
        self.add_phase(phase, Duration::from_nanos(elapsed));
    }

    pub(crate) fn observe_wal_image(&mut self, sparse: bool, fallback: bool, bytes: usize) {
        if sparse {
            self.timing.wal_sparse_reads = self.timing.wal_sparse_reads.saturating_add(1);
        } else {
            self.timing.wal_full_reads = self.timing.wal_full_reads.saturating_add(1);
        }
        if fallback {
            self.timing.wal_fallback_reads = self.timing.wal_fallback_reads.saturating_add(1);
        }
        self.timing.wal_image_bytes = self.timing.wal_image_bytes.max(bytes as u64);
    }

    pub(crate) fn observe_wal_transfer(&mut self, file_bytes: u64, read_bytes: u64) {
        self.timing.wal_file_bytes = self.timing.wal_file_bytes.max(file_bytes);
        self.timing.wal_read_bytes = self.timing.wal_read_bytes.saturating_add(read_bytes);
    }

    pub(crate) fn observe_wal_snapshot(&mut self) {
        self.timing.wal_snapshot_reads = self.timing.wal_snapshot_reads.saturating_add(1);
    }

    pub(crate) fn checkpoint_run(&mut self) {
        self.timing.checkpoint_runs = self.timing.checkpoint_runs.saturating_add(1);
    }

    pub(crate) fn checkpoint_result(&mut self, busy: bool, frames: i64, backfilled: i64) {
        self.timing.checkpoint_busy = self.timing.checkpoint_busy.saturating_add(u32::from(busy));
        self.timing.checkpoint_frames = self
            .timing
            .checkpoint_frames
            .saturating_add(u64::try_from(frames.max(0)).unwrap_or_default());
        self.timing.checkpoint_backfilled = self
            .timing
            .checkpoint_backfilled
            .saturating_add(u64::try_from(backfilled.max(0)).unwrap_or_default());
    }

    pub(crate) fn checkpoint_busy_error(&mut self) {
        self.timing.checkpoint_busy_errors = self.timing.checkpoint_busy_errors.saturating_add(1);
    }

    pub(crate) fn checkpoint_restart(&mut self) {
        self.timing.checkpoint_restarts = self.timing.checkpoint_restarts.saturating_add(1);
    }

    pub(crate) fn finish(mut self, now: Instant) -> crate::CaptureTiming {
        if let Some((active, started)) = self.active.take() {
            self.add_phase(active, now.saturating_duration_since(started));
        }
        self.timing.total_nanos = nanos(now.saturating_duration_since(self.started));
        self.timing
    }

    fn add_phase(&mut self, phase: TimingPhase, elapsed: Duration) {
        let target = match phase {
            TimingPhase::Preparation => &mut self.timing.preparation_nanos,
            TimingPhase::SchemaCheck => &mut self.timing.schema_check_nanos,
            TimingPhase::WalExistence => &mut self.timing.wal_existence_nanos,
            TimingPhase::PositionResolution => &mut self.timing.position_resolution_nanos,
            TimingPhase::WalRead => &mut self.timing.wal_read_nanos,
            TimingPhase::PageCollection => &mut self.timing.page_collection_nanos,
            TimingPhase::Verification => &mut self.timing.verification_nanos,
            TimingPhase::Encode => &mut self.timing.encode_nanos,
            TimingPhase::LocalWrite => &mut self.timing.local_write_nanos,
            TimingPhase::Fsync => &mut self.timing.fsync_nanos,
            TimingPhase::ParentSync => &mut self.timing.parent_sync_nanos,
            TimingPhase::Checkpoint => &mut self.timing.checkpoint_nanos,
        };
        *target = target.saturating_add(nanos(elapsed));
    }
}

pub(super) fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

impl CaptureEngine {
    pub(crate) fn start_timing(&mut self, now: Instant) {
        self.timing = Some(TimingRecorder::new(now));
    }

    pub(crate) fn finish_timing(&mut self, now: Instant) -> crate::CaptureTiming {
        self.timing
            .take()
            .map(|recorder| recorder.finish(now))
            .unwrap_or_default()
    }

    pub(crate) fn timing_begin(&mut self, phase: TimingPhase) {
        if self.timing.is_some() {
            let now = self.host.now_monotonic();
            if let Some(recorder) = &mut self.timing {
                recorder.begin(phase, now);
            }
        }
    }

    pub(crate) fn timing_end(&mut self, phase: TimingPhase) {
        if self.timing.is_some() {
            let now = self.host.now_monotonic();
            if let Some(recorder) = &mut self.timing {
                recorder.end(phase, now);
            }
        }
    }

    pub(crate) fn timing_add_wal_bytes(&mut self, bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_wal_bytes(bytes);
        }
    }

    pub(crate) fn timing_add_database_bytes(&mut self, bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_database_bytes(bytes);
        }
    }

    pub(crate) fn timing_add_ltx_bytes(&mut self, bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_ltx_bytes(bytes);
        }
    }

    pub(crate) fn timing_add_segment(&mut self) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_segment();
        }
    }

    pub(crate) fn timing_add_phase_nanos(&mut self, phase: TimingPhase, elapsed: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.add_phase_nanos(phase, elapsed);
        }
    }

    pub(crate) fn timing_observe_wal_image(&mut self, sparse: bool, fallback: bool, bytes: usize) {
        if let Some(recorder) = &mut self.timing {
            recorder.observe_wal_image(sparse, fallback, bytes);
        }
    }

    pub(crate) fn timing_observe_wal_transfer(&mut self, file_bytes: u64, read_bytes: u64) {
        if let Some(recorder) = &mut self.timing {
            recorder.observe_wal_transfer(file_bytes, read_bytes);
        }
    }

    pub(crate) fn timing_observe_wal_snapshot(&mut self) {
        if let Some(recorder) = &mut self.timing {
            recorder.observe_wal_snapshot();
        }
    }
}

#[cfg(test)]
mod timing_tests {
    use super::*;

    #[test]
    fn recorder_uses_monotonic_instants_without_affecting_capture_state() {
        let start = Instant::now();
        let mut recorder = TimingRecorder::new(start);
        recorder.begin(TimingPhase::Preparation, start + Duration::from_millis(1));
        recorder.end(TimingPhase::Preparation, start + Duration::from_millis(3));
        recorder.begin(TimingPhase::WalRead, start + Duration::from_millis(4));
        recorder.end(TimingPhase::WalRead, start + Duration::from_millis(9));
        recorder.add_wal_bytes(11);
        recorder.add_database_bytes(22);
        recorder.add_ltx_bytes(33);
        recorder.add_segment();

        let timing = recorder.finish(start + Duration::from_millis(10));
        assert_eq!(timing.total_nanos, 10_000_000);
        assert_eq!(timing.preparation_nanos, 2_000_000);
        assert_eq!(timing.wal_read_nanos, 5_000_000);
        assert_eq!(timing.wal_bytes, 11);
        assert_eq!(timing.database_bytes, 22);
        assert_eq!(timing.ltx_bytes, 33);
        assert_eq!(timing.segment_count, 1);
    }

    #[test]
    fn recorder_closes_an_incomplete_phase_and_saturates_counters() {
        let start = Instant::now();
        let mut recorder = TimingRecorder::new(start);
        recorder.begin(TimingPhase::Encode, start);
        recorder.add_wal_bytes(u64::MAX);
        recorder.add_wal_bytes(1);
        recorder.add_segment();
        recorder.add_segment();

        let timing = recorder.finish(start + Duration::from_nanos(7));
        assert_eq!(timing.encode_nanos, 7);
        assert_eq!(timing.wal_bytes, u64::MAX);
        assert_eq!(timing.segment_count, 2);
    }
}
