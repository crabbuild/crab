//! Read and hydration orchestration over Crab storage, metadata, cache, and Xet data.

mod error;
mod fetch_admission;
mod hidden_refs;
mod hydrator;
mod ref_advertisement;
mod selection;
mod store_client;
mod term_resolver;
mod upload_pack;

pub use error::{ReadError, Result};
pub use fetch_admission::{
    FetchAdmissionPolicy, FetchAdmissionReject, FetchWant, validate_fetch_wants_with_manifest,
};
pub use hydrator::{ReadStoreLayout, ShardHydrator, fixed_hydrate_concurrency};
pub use ref_advertisement::{
    ManifestRefAdvertisement, ManifestRefEntry, manifest_ref_advertisement,
};
pub use selection::{
    DEFAULT_READINESS_CACHE_TTL_MS, ReadReplicaCandidate, ReadReplicaFallback,
    ReadReplicaProbeResult, ReadReplicaReadiness, ReadReplicaSelection, ReadRoutingPolicy,
    ReadSource, ReadStoreChoice, ReadStoreSelection, ReadStoreTarget, ReadinessCheckOptions,
    ReadinessProbeStats, ReadyReadReplica, check_read_replica_readiness, select_read_replicas,
    select_read_store_choice, select_ready_read_replica,
};
pub use store_client::{SharedShardHints, StoreClient};
pub use term_resolver::TermResolver;
pub use upload_pack::{
    PackPlan, UploadPackFilter, UploadPackFilterError, UploadPackObjectType, UploadPackRequest,
    combine_upload_pack_filters, parse_upload_pack_filter, plan_upload_pack,
    plan_upload_pack_catalog,
};
