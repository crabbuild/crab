//! Read and hydration orchestration over Crab storage, metadata, cache, and Xet data.

pub mod capsule_protocol;
#[cfg(feature = "hydration")]
pub mod dependency_proof;
mod error;
#[cfg(feature = "hydration")]
mod fetch_admission;
#[cfg(feature = "hydration")]
mod hidden_refs;
#[cfg(feature = "hydration")]
mod hydrator;
#[cfg(feature = "hydration")]
mod integrity;
#[cfg(feature = "hydration")]
pub mod pointer_proof;
#[cfg(feature = "local-staging")]
mod pointer_reader;
#[cfg(feature = "hydration")]
mod ref_advertisement;
#[cfg(feature = "hydration")]
mod selection;
#[cfg(feature = "hydration")]
mod store_client;
#[cfg(feature = "hydration")]
mod term_resolver;
#[cfg(feature = "hydration")]
mod upload_pack;
#[cfg(feature = "hydration")]
pub mod upload_pack_wire;

#[cfg(feature = "hydration")]
pub use error::ReconstructionError;
pub use error::{ReadError, Result};
#[cfg(feature = "hydration")]
pub use fetch_admission::{
    FetchAdmissionPolicy, FetchAdmissionReject, FetchWant, validate_fetch_wants_with_manifest,
};
#[cfg(feature = "hydration")]
pub use hydrator::{ReadRuntimeBuilder, ReadStoreLayout, ReconstructionStream, ShardHydrator};
#[cfg(feature = "hydration")]
pub use integrity::{verify_catalog_file_recipe, verify_origin_recipe};
#[cfg(feature = "local-staging")]
pub use pointer_reader::{PointerRead, PointerReader};
#[cfg(feature = "hydration")]
pub use ref_advertisement::{
    ManifestRefAdvertisement, ManifestRefEntry, capsule_ref_advertisement,
    capsule_ref_view_advertisement, manifest_ref_advertisement, root_ref_advertisement,
};
#[cfg(feature = "hydration")]
pub use selection::{
    DEFAULT_READINESS_CACHE_TTL_MS, ReadReplicaCandidate, ReadReplicaFallback,
    ReadReplicaProbeResult, ReadReplicaReadiness, ReadReplicaSelection, ReadRoutingPolicy,
    ReadSource, ReadStoreChoice, ReadStoreSelection, ReadStoreTarget, ReadinessCheckOptions,
    ReadinessProbeStats, ReadyReadReplica, check_capsule_read_replica_readiness,
    select_read_replicas, select_read_store_choice, select_ready_read_replica,
    verify_capsule_pointer_catalog_objects,
};
#[cfg(feature = "hydration")]
pub use store_client::{ReadMetrics, StoreClient, XorbAvailability};
#[cfg(feature = "hydration")]
pub use term_resolver::TermResolver;
#[cfg(feature = "hydration")]
pub use upload_pack::{
    PackPlan, UPLOAD_PACK_MAX_DURATION, UploadPackFilter, UploadPackFilterError,
    UploadPackObjectType, UploadPackRequest, combine_upload_pack_filters, parse_upload_pack_filter,
    plan_upload_pack, plan_upload_pack_catalog, plan_upload_pack_tip_bound,
    plan_upload_pack_tip_bound_with_transitions, tip_bound_transitions_cover_wants,
    upload_pack_repository_options,
};
