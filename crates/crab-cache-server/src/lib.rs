//! Cache-server runtime contracts for Crab.
//!
//! This crate owns server-side configuration, HTTP error semantics, origin
//! object-store access, SQLite metadata opening, cache persistence, background
//! eviction, Prometheus metrics, cache-server auth, HTTP handlers, router
//! state, server bootstrap, preflight/evidence/onboarding checks, and chunk
//! dedup indexing.

pub mod auth;
pub mod cache_store;
pub mod chunk_index;
pub mod config;
pub mod db;
pub mod error;
pub mod evictor;
pub mod evidence;
pub mod handlers;
pub mod metrics;
pub mod onboarding;
pub mod origin_client;
pub mod preflight;
pub mod server;
pub mod state;

pub use auth::{
    AuthPolicy, AuthPolicyDiagnostics, ClientIdentity, PolicyRule, TlsClientIdentity,
    auth_middleware,
};
pub use cache_store::{
    CacheEvictionStats, CacheIntegrityStats, CacheRangeRead, CacheRuntimeIntegrityStats,
    CacheStats, CacheStore, CachedRange, EvictFilter, EvictStats, ObjectMeta, ObjectType,
    ServerObjectKey, TempPathCommitError, TempPathCommitRecovery, parse_hash_hex,
};
pub use chunk_index::{ChunkIndex, ChunkLocation, DedupResult};
pub use config::{AuthConfig, CacheServerConfig, DedupScope, MutablePathMode, TlsConfig};
pub use db::{CACHE_DB_FILE, CacheDb};
pub use error::{CacheServiceError, Result};
pub use evictor::{EvictorHandle, start_evictor_task};
pub use evidence::{
    EvidenceArtifactSummary, EvidenceCacheSummary, EvidenceDedupSummary, EvidenceDoctorCategory,
    EvidenceDoctorReport, EvidenceEnterpriseSummary, EvidenceHydrateSummary,
    EvidenceReleaseVerification, EvidenceRouteContractSummary, EvidenceSummary,
    EvidenceVerificationCheck, EvidenceVerificationReport, EvidenceVerificationStatus,
    doctor_evidence_verification, find_release_evidence_report, summarize_evidence_report,
    verify_evidence_report, verify_release_evidence_report,
};
pub use onboarding::{
    OnboardingBundle, OnboardingCheck, OnboardingCheckReport, OnboardingClientProbeReport,
    OnboardingProbeOptions, OnboardingProbeReport, OnboardingRenderOptions,
    check_onboarding_bundle, probe_onboarding_bundle, render_onboarding_bundle,
};
pub use origin_client::{ORIGIN_HEALTH_PROBE_PATH, OriginClient, origin_probe_reached_origin};
pub use preflight::{
    CacheServerPreflightReport, PreflightCheck, PreflightProfile, PreflightProfileOptions,
    PreflightStatus, apply_preflight_profile, redacted_origin_url, run_preflight,
};
pub use server::{
    PreparedServer, ServerStartupOptions, build_rustls_config, prepare_server, run_server,
};
pub use state::{
    AppState, DedupIndexIngestionError, DedupIndexRebuildStats, MAX_CACHE_OBJECT_BYTES,
    build_router,
};
