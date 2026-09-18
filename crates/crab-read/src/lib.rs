//! Read and hydration orchestration over Crab storage, metadata, cache, and Xet data.

pub mod capsule_protocol;
pub mod dependency_proof;
mod error;
mod fetch_admission;
mod hidden_refs;
mod hydrator;
mod integrity;
pub mod pointer_proof;
mod ref_advertisement;
mod selection;
mod store_client;
mod term_resolver;
mod upload_pack;
pub mod upload_pack_wire;

pub use error::{ReadError, ReconstructionError, Result};
pub use fetch_admission::{
    FetchAdmissionPolicy, FetchAdmissionReject, FetchWant, validate_fetch_wants_with_manifest,
};
pub use hydrator::{ReadRuntimeBuilder, ReadStoreLayout, ReconstructionStream, ShardHydrator};
pub use integrity::verify_origin_recipe;
pub use ref_advertisement::{
    ManifestRefAdvertisement, ManifestRefEntry, capsule_ref_advertisement,
    capsule_ref_view_advertisement, manifest_ref_advertisement, root_ref_advertisement,
};
pub use selection::{
    DEFAULT_READINESS_CACHE_TTL_MS, ReadReplicaCandidate, ReadReplicaFallback,
    ReadReplicaProbeResult, ReadReplicaReadiness, ReadReplicaSelection, ReadRoutingPolicy,
    ReadSource, ReadStoreChoice, ReadStoreSelection, ReadStoreTarget, ReadinessCheckOptions,
    ReadinessProbeStats, ReadyReadReplica, check_capsule_read_replica_readiness,
    check_legacy_read_replica_readiness, select_read_replicas, select_read_store_choice,
    select_ready_read_replica, verify_capsule_pointer_catalog_objects,
};
pub use store_client::{ReadMetrics, StoreClient, XorbAvailability};
pub use term_resolver::TermResolver;
pub use upload_pack::{
    PackPlan, UPLOAD_PACK_MAX_DURATION, UploadPackFilter, UploadPackFilterError,
    UploadPackObjectType, UploadPackRequest, combine_upload_pack_filters, parse_upload_pack_filter,
    plan_upload_pack, plan_upload_pack_catalog, upload_pack_repository_options,
};
