use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use metrics::{Counter, Gauge, Key, KeyName, Recorder, Unit};

use super::{METADATA, key};

const MIN_HEADROOM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HEADROOM_BYTES: u64 = 1024 * 1024 * 1024;
const STREAM_RESERVATION_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ScratchCapacityError {
    #[error("scratch filesystem capacity is exhausted")]
    Exhausted,
    #[error("scratch filesystem capacity is unavailable")]
    Unavailable(#[source] std::io::Error),
}

#[derive(Clone)]
pub(super) struct FilesystemMetrics {
    inner: Arc<FilesystemInner>,
}

struct FilesystemInner {
    path: PathBuf,
    pending: Mutex<u64>,
    size: Gauge,
    free: Gauge,
    available: Gauge,
    headroom: Gauge,
    pending_gauge: Gauge,
    probe_success: Gauge,
    probe_failures: Counter,
    exhausted: Counter,
    probe_rejections: Counter,
}

pub(crate) struct ScratchReservation {
    filesystem: FilesystemMetrics,
    remaining: u64,
}

pub(crate) struct ScratchWritePermit {
    filesystem: FilesystemMetrics,
    bytes: u64,
}

impl FilesystemMetrics {
    pub(super) fn new(recorder: &impl Recorder, path: PathBuf) -> Self {
        describe(recorder);
        Self {
            inner: Arc::new(FilesystemInner {
                path,
                pending: Mutex::new(0),
                size: recorder.register_gauge(
                    &Key::from_static_name("crab_s3_gateway_scratch_filesystem_size_bytes"),
                    &METADATA,
                ),
                free: recorder.register_gauge(
                    &Key::from_static_name("crab_s3_gateway_scratch_filesystem_free_bytes"),
                    &METADATA,
                ),
                available: recorder.register_gauge(
                    &Key::from_static_name("crab_s3_gateway_scratch_filesystem_available_bytes"),
                    &METADATA,
                ),
                headroom: recorder.register_gauge(
                    &Key::from_static_name("crab_s3_gateway_scratch_headroom_bytes"),
                    &METADATA,
                ),
                pending_gauge: recorder.register_gauge(
                    &Key::from_static_name("crab_s3_gateway_scratch_pending_bytes"),
                    &METADATA,
                ),
                probe_success: recorder.register_gauge(
                    &Key::from_static_name("crab_s3_gateway_scratch_filesystem_probe_success"),
                    &METADATA,
                ),
                probe_failures: recorder.register_counter(
                    &Key::from_static_name(
                        "crab_s3_gateway_scratch_filesystem_probe_failures_total",
                    ),
                    &METADATA,
                ),
                exhausted: recorder.register_counter(
                    &key(
                        "crab_s3_gateway_scratch_capacity_rejections_total",
                        &[("reason", "exhausted")],
                    ),
                    &METADATA,
                ),
                probe_rejections: recorder.register_counter(
                    &key(
                        "crab_s3_gateway_scratch_capacity_rejections_total",
                        &[("reason", "probe_error")],
                    ),
                    &METADATA,
                ),
            }),
        }
    }

    pub(super) fn reserve(&self, bytes: u64) -> Result<ScratchReservation, ScratchCapacityError> {
        let remaining = self.reserve_at_least(bytes, bytes)?;
        Ok(ScratchReservation {
            filesystem: self.clone(),
            remaining,
        })
    }

    pub(super) fn refresh(&self) {
        let pending = self.pending();
        match fs4::statvfs(&self.inner.path) {
            Ok(stats) => self.record_stats(&stats),
            Err(error) => {
                self.record_probe_failure(error);
            }
        }
        self.inner.pending_gauge.set(*pending as f64);
    }

    fn reserve_at_least(&self, required: u64, preferred: u64) -> Result<u64, ScratchCapacityError> {
        if required == 0 {
            return Ok(0);
        }
        // Materialization releases this same lock only after its filesystem
        // write returns. Taking the stat while locked prevents a reservation
        // from pairing a pre-write availability snapshot with post-write state.
        let mut pending = self.pending();
        let stats = fs4::statvfs(&self.inner.path).map_err(|error| {
            self.inner.probe_rejections.increment(1);
            self.record_probe_failure(error)
        })?;
        self.record_stats(&stats);
        let available_for_reservations = stats
            .available_space()
            .saturating_sub(scratch_headroom(stats.total_space()));
        match reserve_pending(
            &mut pending,
            available_for_reservations,
            required,
            preferred,
        ) {
            Ok(reserved) => Ok(reserved),
            Err(error) => {
                self.inner.exhausted.increment(1);
                Err(error)
            }
        }
    }

    fn record_stats(&self, stats: &fs4::FsStats) {
        self.inner.size.set(stats.total_space() as f64);
        self.inner.free.set(stats.free_space() as f64);
        self.inner.available.set(stats.available_space() as f64);
        self.inner
            .headroom
            .set(scratch_headroom(stats.total_space()) as f64);
        self.inner.probe_success.set(1);
    }

    fn record_probe_failure(&self, error: std::io::Error) -> ScratchCapacityError {
        // Capacity must fail closed: stale space from an earlier probe could
        // otherwise admit writes after mount loss.
        self.inner.size.set(0);
        self.inner.free.set(0);
        self.inner.available.set(0);
        self.inner.headroom.set(0);
        self.inner.probe_success.set(0);
        self.inner.probe_failures.increment(1);
        ScratchCapacityError::Unavailable(error)
    }

    fn release(&self, bytes: u64) {
        if bytes != 0 {
            let mut pending = self.pending();
            *pending = pending.saturating_sub(bytes);
        }
    }

    fn pending(&self) -> MutexGuard<'_, u64> {
        match self.inner.pending.lock() {
            Ok(pending) => pending,
            // The guarded integer remains valid even if an unrelated panic
            // poisoned the mutex; capacity continues to fail closed below.
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl ScratchReservation {
    pub(crate) fn reserve_write(
        &mut self,
        bytes: u64,
    ) -> Result<ScratchWritePermit, ScratchCapacityError> {
        if bytes > self.remaining {
            let missing = bytes - self.remaining;
            let preferred = missing.max(STREAM_RESERVATION_BYTES);
            self.remaining = self
                .remaining
                .saturating_add(self.filesystem.reserve_at_least(missing, preferred)?);
        }
        self.remaining -= bytes;
        Ok(ScratchWritePermit {
            filesystem: self.filesystem.clone(),
            bytes,
        })
    }
}

impl Drop for ScratchReservation {
    fn drop(&mut self) {
        self.filesystem.release(self.remaining);
    }
}

impl Drop for ScratchWritePermit {
    fn drop(&mut self) {
        self.filesystem.release(self.bytes);
    }
}

fn scratch_headroom(total: u64) -> u64 {
    (total / 10)
        .clamp(MIN_HEADROOM_BYTES, MAX_HEADROOM_BYTES)
        .min(total)
}

fn reserve_pending(
    pending: &mut u64,
    limit: u64,
    required: u64,
    preferred: u64,
) -> Result<u64, ScratchCapacityError> {
    let remaining = limit.saturating_sub(*pending);
    if required > remaining {
        return Err(ScratchCapacityError::Exhausted);
    }
    let reserved = preferred.min(remaining).max(required);
    *pending = pending
        .checked_add(reserved)
        .ok_or(ScratchCapacityError::Exhausted)?;
    Ok(reserved)
}

fn describe(recorder: &impl Recorder) {
    for (name, description) in [
        (
            "crab_s3_gateway_scratch_filesystem_size_bytes",
            "Total capacity reported by the filesystem containing the process temporary directory.",
        ),
        (
            "crab_s3_gateway_scratch_filesystem_free_bytes",
            "Unallocated capacity reported by the scratch filesystem.",
        ),
        (
            "crab_s3_gateway_scratch_filesystem_available_bytes",
            "Scratch filesystem capacity available to the gateway process.",
        ),
        (
            "crab_s3_gateway_scratch_headroom_bytes",
            "Scratch capacity retained outside gateway reservations.",
        ),
        (
            "crab_s3_gateway_scratch_pending_bytes",
            "Scratch bytes reserved for writes that have not completed.",
        ),
    ] {
        recorder.describe_gauge(
            KeyName::from_const_str(name),
            Some(Unit::Bytes),
            description.into(),
        );
    }
    recorder.describe_gauge(
        KeyName::from_const_str("crab_s3_gateway_scratch_filesystem_probe_success"),
        None,
        "Whether the most recent scratch filesystem capacity probe succeeded.".into(),
    );
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_scratch_filesystem_probe_failures_total"),
        None,
        "Failed scratch filesystem capacity probes.".into(),
    );
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_scratch_capacity_rejections_total"),
        None,
        "Scratch reservations rejected by bounded reason.".into(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headroom_is_bounded_for_small_and_large_filesystems() {
        assert_eq!(scratch_headroom(512 * 1024 * 1024), 64 * 1024 * 1024);
        assert_eq!(
            scratch_headroom(8 * 1024 * 1024 * 1024),
            8 * 1024_u64.pow(3) / 10
        );
        assert_eq!(
            scratch_headroom(100 * 1024 * 1024 * 1024),
            1024 * 1024 * 1024
        );
    }

    #[test]
    fn pending_reservations_cannot_exceed_available_capacity() {
        let mut pending = 0;
        assert_eq!(reserve_pending(&mut pending, 100, 60, 60).unwrap(), 60);
        assert!(matches!(
            reserve_pending(&mut pending, 100, 41, 41),
            Err(ScratchCapacityError::Exhausted)
        ));
        assert_eq!(reserve_pending(&mut pending, 100, 20, 50).unwrap(), 40);
        assert_eq!(pending, 100);
    }

    #[test]
    fn streaming_reservation_uses_remaining_capacity_without_false_rejection() {
        let mut pending = 90;
        assert_eq!(reserve_pending(&mut pending, 100, 5, 20).unwrap(), 10);
        assert_eq!(pending, 100);
    }
}
