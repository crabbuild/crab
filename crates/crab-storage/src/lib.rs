//! Object-store layout and transport helpers for Crab.

pub mod cas;
pub mod error;
pub mod error_map;
pub mod external;
pub mod head_batch;
pub mod identity;
pub mod layout;
pub mod provider_options;
pub mod provider_store;
pub mod retry;
pub mod store;

pub use cas::{
    DEFAULT_MAX_ATTEMPTS, MAX_CAS_OBJECT_BYTES, cas_update, cas_update_bounded, cas_update_default,
};
pub use crab_types::storage::StorageScope;
pub use error::{Result, StorageError};
pub use error_map::{classify_auth_error, map_object_store_error};
pub use external::{
    ExternalByteStream, ExternalCapabilities, ExternalDataStore, ExternalObjectMeta,
    ObjectStoreExternalDataStore,
};
pub use head_batch::{HeadBatchConfig, HeadBatchOutcome, HeadBatchStore, head_batch};
pub use identity::{BucketIdentity, StorageProviderKind};
pub use layout::{
    GLOBAL_CONTENT_FANOUT_WIDTH, GLOBAL_PREFIX, ObjectType, StorageScopeProvider, StoreLayout,
    canonical_global_content_path, content_hash_from_path, global_content_partition_prefix,
    global_content_path, global_content_prefix, repo_pack_index_path, repo_pack_metadata_path,
    repo_pack_path, repo_pack_reverse_index_path,
};
pub use provider_options::{
    apply_s3_env_overrides, default_client_options, parse_sas_query_pairs, s3_endpoint_from_env,
    s3_virtual_hosted_style_from_env,
};
pub use provider_store::{
    AzureAuthorization, BuiltObjectStore, ObjectStoreCredentials, StaticEnvStoreTarget,
    StaticEnvStoreTargetSelection, StaticEnvStoreUrlForm, StaticEnvStoreUrlParts, UrlObjectStore,
    build_object_store, build_object_store_with_endpoint,
    build_static_env_azure_account_container_store, build_static_env_store,
    build_static_env_target_store, build_url_object_store, resolve_static_env_provider,
    resolve_static_env_provider_value, static_env_target_selection,
    static_env_target_selection_for_provider, validate_static_env_url_provider,
};
pub use retry::{RetryClass, RetryPolicy, retry, retry_class};
pub use store::{ETag, StagedWrite, StorageReadKind, Store};
