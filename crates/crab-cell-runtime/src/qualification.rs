//! Qualification profiles, receipts, matrix manifests, and cluster validation.
pub mod cluster;

pub use profile::{
    QUALIFICATION_CASE_COVERAGE_BYTES, QUALIFICATION_CASE_COVERAGE_OPERATIONS, QUALIFICATION_CASES,
    QUALIFICATION_MATRIX_ROWS, QUALIFICATION_MATRIX_SCHEMA_VERSION, QUALIFICATION_PRIMITIVES,
    QUALIFICATION_PROFILE_SCHEMA_VERSION, QUALIFICATION_PROTECTED_EVIDENCE_MAX_AGE_MS,
    QUALIFICATION_PROTECTED_EVIDENCE_MAX_CLOCK_SKEW_MS, QUALIFICATION_RESOURCE_METRICS,
    QUALIFICATION_SCHEMA_VERSION, QualificationCase, QualificationProfile,
};
pub use receipt::{
    QUALIFICATION_PROVIDER_EVIDENCE_SCHEMA_VERSION, QUALIFICATION_RUN_ARTIFACT_SCHEMA_VERSION,
    QualificationExecutionEvidence, QualificationMatrixEntry, QualificationMatrixManifest,
    QualificationMetric, QualificationOwnership, QualificationProviderEvidence,
    QualificationReceipt, QualificationRunArtifact, QualificationRunner,
};
pub use workload::{
    QualificationExecution, QualificationLatencyHistogram, QualificationOperation,
    QualificationOperationExecutor, QualificationOperationIter, QualificationOutcome,
    QualificationPrimitiveCounts, QualificationRunSummary, QualificationWorkload,
};
mod profile;
mod receipt;
mod workload;

use std::{
    collections::BTreeSet,
    future::Future,
    path::Path,
    time::{Duration, Instant},
};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};

use crate::identity::Digest;
use crate::{Error, Result};

#[cfg(test)]
mod tests;
