use metrics::{Counter, Gauge, KeyName, Recorder, Unit};

use super::{METADATA, describe_counter, describe_gauge, key};

const PURPOSES: [&str; 3] = ["content_spool", "xet_reconstruction", "git_pack"];
const FAILURES: [&str; 4] = ["create", "write", "flush", "read"];

pub(super) struct ScratchMetrics {
    purposes: [PurposeMetrics; PURPOSES.len()],
}

struct PurposeMetrics {
    files: Gauge,
    bytes: Gauge,
    written: Counter,
    failures: [Counter; FAILURES.len()],
}

#[derive(Clone, Copy)]
pub(crate) enum ScratchPurpose {
    ContentSpool,
    XetReconstruction,
    GitPack,
}

#[derive(Clone, Copy)]
pub(crate) enum ScratchFailure {
    Create,
    Write,
    Flush,
    Read,
}

pub(crate) struct ScratchUsage {
    files: Gauge,
    bytes: Gauge,
    written: Counter,
    failures: [Counter; FAILURES.len()],
    owned_bytes: u64,
}

impl ScratchPurpose {
    const fn index(self) -> usize {
        self as usize
    }
}

impl ScratchFailure {
    const fn index(self) -> usize {
        self as usize
    }
}

impl ScratchMetrics {
    pub(super) fn new(recorder: &impl Recorder) -> Self {
        describe_gauge(
            recorder,
            "crab_s3_gateway_scratch_files",
            "Temporary content files currently owned by the gateway.",
        );
        describe_gauge(
            recorder,
            "crab_s3_gateway_scratch_bytes",
            "Logical bytes reserved by temporary content files currently owned by the gateway.",
        );
        recorder.describe_counter(
            KeyName::from_const_str("crab_s3_gateway_scratch_bytes_written_total"),
            Some(Unit::Bytes),
            "Bytes successfully written to temporary content files.".into(),
        );
        describe_counter(
            recorder,
            "crab_s3_gateway_scratch_io_failures_total",
            "Temporary content I/O failures by bounded operation.",
        );
        Self {
            purposes: PURPOSES.map(|purpose| PurposeMetrics::new(recorder, purpose)),
        }
    }

    pub(super) fn start(&self, purpose: ScratchPurpose) -> ScratchUsage {
        let metrics = &self.purposes[purpose.index()];
        metrics.files.increment(1);
        ScratchUsage {
            files: metrics.files.clone(),
            bytes: metrics.bytes.clone(),
            written: metrics.written.clone(),
            failures: metrics.failures.clone(),
            owned_bytes: 0,
        }
    }
}

impl PurposeMetrics {
    fn new(recorder: &impl Recorder, purpose: &'static str) -> Self {
        Self {
            files: recorder.register_gauge(
                &key("crab_s3_gateway_scratch_files", &[("purpose", purpose)]),
                &METADATA,
            ),
            bytes: recorder.register_gauge(
                &key("crab_s3_gateway_scratch_bytes", &[("purpose", purpose)]),
                &METADATA,
            ),
            written: recorder.register_counter(
                &key(
                    "crab_s3_gateway_scratch_bytes_written_total",
                    &[("purpose", purpose)],
                ),
                &METADATA,
            ),
            failures: FAILURES.map(|operation| {
                recorder.register_counter(
                    &key(
                        "crab_s3_gateway_scratch_io_failures_total",
                        &[("purpose", purpose), ("operation", operation)],
                    ),
                    &METADATA,
                )
            }),
        }
    }
}

impl ScratchUsage {
    pub(crate) fn reserve(&mut self, bytes: u64) {
        let owned_bytes = self.owned_bytes.saturating_add(bytes);
        self.bytes
            .increment(owned_bytes.saturating_sub(self.owned_bytes) as f64);
        self.owned_bytes = owned_bytes;
    }

    pub(crate) fn record_written(&self, bytes: u64) {
        self.written.increment(bytes);
    }

    pub(crate) fn record_failure(&self, failure: ScratchFailure) {
        self.failures[failure.index()].increment(1);
    }
}

impl Drop for ScratchUsage {
    fn drop(&mut self) {
        self.bytes.decrement(self.owned_bytes as f64);
        self.files.decrement(1);
    }
}
