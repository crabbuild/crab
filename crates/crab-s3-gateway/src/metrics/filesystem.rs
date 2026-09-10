use std::path::PathBuf;

use metrics::{Counter, Gauge, Key, KeyName, Recorder, Unit};

use super::METADATA;

pub(super) struct FilesystemMetrics {
    path: PathBuf,
    size: Gauge,
    free: Gauge,
    available: Gauge,
    probe_success: Gauge,
    probe_failures: Counter,
}

impl FilesystemMetrics {
    pub(super) fn new(recorder: &impl Recorder, path: PathBuf) -> Self {
        describe(recorder);
        Self {
            path,
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
            probe_success: recorder.register_gauge(
                &Key::from_static_name("crab_s3_gateway_scratch_filesystem_probe_success"),
                &METADATA,
            ),
            probe_failures: recorder.register_counter(
                &Key::from_static_name("crab_s3_gateway_scratch_filesystem_probe_failures_total"),
                &METADATA,
            ),
        }
    }

    pub(super) fn refresh(&self) {
        match fs4::statvfs(&self.path) {
            Ok(stats) => {
                self.size.set(stats.total_space() as f64);
                self.free.set(stats.free_space() as f64);
                self.available.set(stats.available_space() as f64);
                self.probe_success.set(1);
            }
            Err(_) => {
                // Capacity must fail closed: stale space from an earlier scrape
                // could otherwise suppress an exhaustion alert after mount loss.
                self.size.set(0);
                self.free.set(0);
                self.available.set(0);
                self.probe_success.set(0);
                self.probe_failures.increment(1);
            }
        }
    }
}

fn describe(recorder: &impl Recorder) {
    recorder.describe_gauge(
        KeyName::from_const_str("crab_s3_gateway_scratch_filesystem_size_bytes"),
        Some(Unit::Bytes),
        "Total capacity reported by the filesystem containing the process temporary directory."
            .into(),
    );
    recorder.describe_gauge(
        KeyName::from_const_str("crab_s3_gateway_scratch_filesystem_free_bytes"),
        Some(Unit::Bytes),
        "Unallocated capacity reported by the scratch filesystem.".into(),
    );
    recorder.describe_gauge(
        KeyName::from_const_str("crab_s3_gateway_scratch_filesystem_available_bytes"),
        Some(Unit::Bytes),
        "Scratch filesystem capacity available to the gateway process.".into(),
    );
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
}
