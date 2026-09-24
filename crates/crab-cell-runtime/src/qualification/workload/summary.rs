//! Measured run summary and latency histogram.

use std::time::Duration;

use crate::identity::Digest;
use crate::qualification::profile::{QUALIFICATION_CASE_COVERAGE_BYTES, QUALIFICATION_PRIMITIVES};
use crate::qualification::receipt::{
    QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION, QualificationMetric, QualificationRunArtifact,
    validate_metrics, validate_resource_metric_list,
};
use crate::{Error, Result};

use super::QualificationWorkload;
use super::operation::QualificationPrimitiveCounts;

/// Bounded latency histogram used by a qualification run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationLatencyHistogram {
    pub(in crate::qualification) buckets: [u64; 64],
    pub(in crate::qualification) samples: u64,
}

impl Default for QualificationLatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; 64],
            samples: 0,
        }
    }
}

impl QualificationLatencyHistogram {
    pub(in crate::qualification) fn record(&mut self, latency: Duration) {
        let micros = latency.as_micros().max(1).min(u128::from(u64::MAX)) as u64;
        let bucket = (u64::BITS - micros.leading_zeros() - 1) as usize;
        self.buckets[bucket.min(self.buckets.len() - 1)] =
            self.buckets[bucket.min(self.buckets.len() - 1)].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
    }

    pub(in crate::qualification) fn percentile_ms(&self, percentile: u64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let rank = self.samples.saturating_mul(percentile).saturating_add(99) / 100;
        let mut seen = 0_u64;
        for (bucket, count) in self.buckets.iter().copied().enumerate() {
            seen = seen.saturating_add(count);
            if seen >= rank {
                let micros = 1_u64 << bucket.min(63);
                return micros.saturating_add(999) / 1_000;
            }
        }
        u64::MAX / 1_000
    }
}

/// Measured result of executing a canonical workload through typed APIs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QualificationRunSummary {
    pub(in crate::qualification) profile: String,
    pub(in crate::qualification) profile_digest: Digest,
    pub(in crate::qualification) seed: u64,
    pub(in crate::qualification) cells: u64,
    pub(in crate::qualification) operations: u64,
    pub(in crate::qualification) elapsed: Duration,
    pub(in crate::qualification) primitive_counts: Vec<QualificationPrimitiveCounts>,
    pub(in crate::qualification) case_coverage: Vec<u8>,
    pub(in crate::qualification) outcome_digest: Digest,
    pub(in crate::qualification) latency: QualificationLatencyHistogram,
}

impl QualificationRunSummary {
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    #[must_use]
    pub const fn profile_digest(&self) -> Digest {
        self.profile_digest
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    #[must_use]
    pub const fn cells(&self) -> u64 {
        self.cells
    }

    #[must_use]
    pub const fn operations(&self) -> u64 {
        self.operations
    }

    #[must_use]
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    #[must_use]
    pub fn primitive_counts(&self) -> &[QualificationPrimitiveCounts] {
        &self.primitive_counts
    }

    /// Returns one bounded bitset of observed primitive/lifecycle pairs.
    #[must_use]
    pub fn case_coverage(&self) -> &[u8] {
        &self.case_coverage
    }

    #[must_use]
    pub const fn outcome_digest(&self) -> Digest {
        self.outcome_digest
    }

    /// Returns the measured latency percentile rounded up to milliseconds.
    #[must_use]
    pub fn latency_percentile_ms(&self, percentile: u64) -> u64 {
        self.latency.percentile_ms(percentile)
    }

    /// Returns receipt-compatible bounded metrics from this run.
    pub fn metrics(&self) -> Result<Vec<QualificationMetric>> {
        let duration_secs = self
            .elapsed
            .as_secs()
            .saturating_add(u64::from(self.elapsed.subsec_nanos() != 0))
            .max(1);
        let throughput = self.operations / duration_secs;
        Ok(vec![
            QualificationMetric::new("cells".into(), self.cells, "cells".into())?,
            QualificationMetric::new("operations".into(), self.operations, "operations".into())?,
            QualificationMetric::new("duration_secs".into(), duration_secs, "seconds".into())?,
            QualificationMetric::new("throughput_ops_per_sec".into(), throughput, "ops/s".into())?,
            QualificationMetric::new(
                "p50_latency_ms".into(),
                self.latency_percentile_ms(50),
                "ms".into(),
            )?,
            QualificationMetric::new(
                "p95_latency_ms".into(),
                self.latency_percentile_ms(95),
                "ms".into(),
            )?,
            QualificationMetric::new(
                "p99_latency_ms".into(),
                self.latency_percentile_ms(99),
                "ms".into(),
            )?,
            QualificationMetric::new(
                "max_latency_ms".into(),
                self.latency_percentile_ms(100),
                "ms".into(),
            )?,
        ])
    }

    /// Encodes the measured result together with the exact canonical workload.
    pub fn artifact(&self, workload: &QualificationWorkload) -> Result<QualificationRunArtifact> {
        self.artifact_with_resource_metrics(workload, &[])
    }

    /// Encodes a measured result and the resource observations captured for the same run.
    ///
    /// Protected profiles require every
    /// [`QUALIFICATION_RESOURCE_METRICS`](crate::qualification::QUALIFICATION_RESOURCE_METRICS)
    /// observation. The receipt verifier compares these values with the signed
    /// execution measurements, so a harness cannot substitute a different
    /// machine's resource envelope after the workload has completed.
    pub fn artifact_with_resource_metrics(
        &self,
        workload: &QualificationWorkload,
        resource_metrics: &[QualificationMetric],
    ) -> Result<QualificationRunArtifact> {
        if self.profile != workload.profile
            || self.profile_digest != workload.profile_digest()
            || self.seed != workload.seed
            || self.cells != workload.cells
            || self.operations != workload.operations
        {
            return Err(Error::Control("qualification run workload identity"));
        }
        validate_resource_metric_list(resource_metrics)?;
        let mut metrics = self.metrics()?;
        metrics.extend(resource_metrics.iter().cloned());
        validate_metrics(&metrics)?;
        Ok(QualificationRunArtifact {
            schema_version: QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
            workload: workload.clone(),
            profile: self.profile.clone(),
            profile_digest: *self.profile_digest.as_bytes(),
            seed: self.seed,
            cells: self.cells,
            operations: self.operations,
            elapsed_ms: self.elapsed.as_millis().max(1).min(u128::from(u64::MAX)) as u64,
            primitive_counts: self.primitive_counts.clone(),
            case_coverage: self.case_coverage.clone(),
            outcome_digest: *qualification_run_outcome_digest(
                workload,
                &self.primitive_counts,
                &self.case_coverage,
            )?
            .as_bytes(),
            metrics,
        })
    }
}

pub(in crate::qualification) fn qualification_run_outcome_digest(
    workload: &QualificationWorkload,
    counts: &[QualificationPrimitiveCounts],
    case_coverage: &[u8],
) -> Result<Digest> {
    if case_coverage.len() != QUALIFICATION_CASE_COVERAGE_BYTES {
        return Err(Error::Control("qualification run case coverage"));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab-cell-runtime/qualification/run-v3");
    hasher.update(workload.outcome_digest().as_bytes());
    hasher.update(workload.profile_digest().as_bytes());
    hasher.update(&workload.seed.to_be_bytes());
    hasher.update(&workload.cells.to_be_bytes());
    hasher.update(&workload.operations.to_be_bytes());
    hasher.update(case_coverage);
    for primitive in QUALIFICATION_PRIMITIVES {
        let primitive_counts = counts
            .iter()
            .find(|counts| counts.primitive == *primitive)
            .ok_or(Error::Control("qualification run primitive identity"))?;
        hasher.update(&(primitive.len() as u64).to_be_bytes());
        hasher.update(primitive.as_bytes());
        for value in [
            primitive_counts.attempted,
            primitive_counts.acknowledged,
            primitive_counts.rejected,
            primitive_counts.ambiguous,
            primitive_counts.retried,
            primitive_counts.verified,
        ] {
            hasher.update(&value.to_be_bytes());
        }
    }
    Ok(Digest::from_bytes(*hasher.finalize().as_bytes()))
}
