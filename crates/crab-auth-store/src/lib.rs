//! Auth credential to storage-store adapter.

#[cfg(feature = "refreshing-store")]
mod gateway_store;
#[cfg(feature = "managed-service")]
mod managed_repository;
#[cfg(feature = "refreshing-store")]
pub mod refreshing_store;

use crab_auth::{AzureToken, CloudCredentials};
#[cfg(feature = "refreshing-store")]
use crab_auth::{ProviderCredentials, TransferGrant, TransferTransport};
use crab_storage::{
    AzureAuthorization, BuiltObjectStore, ObjectStoreCredentials, StorageError, Store,
    build_object_store,
};

#[cfg(feature = "managed-service")]
pub use managed_repository::{
    ManagedControlPlane, ManagedPush, ManagedRepositoryDiagnostic, ManagedRepositoryError,
    ManagedRepositoryResolver,
};
#[cfg(feature = "refreshing-store")]
pub use refreshing_store::{RefreshingObjectStore, RefreshingStoreParts};

/// Result alias for auth/storage composition.
pub type Result<T> = std::result::Result<T, AuthStoreError>;

/// Store and physical prefix resolved from one validated managed transfer grant.
#[cfg(feature = "refreshing-store")]
pub struct ManagedStore {
    pub store: Store,
    pub repository_prefix: String,
}

/// Builds the canonical Crab store abstraction from a validated managed grant.
#[cfg(feature = "refreshing-store")]
pub fn build_store_from_transfer_grant(
    grant: &TransferGrant,
    gateway_client: reqwest::Client,
) -> Result<ManagedStore> {
    let repository_prefix = grant.storage_scope.repository_prefix.clone();
    let global_prefix = format!("{repository_prefix}/.crab");
    let expires_at = std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(
            grant.expires_at.unix_timestamp().try_into().map_err(|_| {
                AuthStoreError::InvalidCredentials {
                    reason: "managed transfer grant expiry predates the Unix epoch".to_owned(),
                }
            })?,
        ))
        .ok_or_else(|| AuthStoreError::InvalidCredentials {
            reason: "managed transfer grant expiry is outside the platform range".to_owned(),
        })?;
    let store = match &grant.transport {
        TransferTransport::Direct { direct } => {
            let credentials = match &direct.credentials {
                ProviderCredentials::Aws {
                    access_key_id,
                    secret_access_key,
                    session_token,
                } => CloudCredentials::Aws {
                    access_key_id: access_key_id.expose_secret().to_owned(),
                    secret_access_key: secret_access_key.expose_secret().to_owned(),
                    session_token: Some(session_token.expose_secret().to_owned()),
                    expires_at,
                    region: direct.region.clone().ok_or_else(|| {
                        AuthStoreError::InvalidCredentials {
                            reason: "managed AWS grant omitted its region".to_owned(),
                        }
                    })?,
                },
                ProviderCredentials::Gcp { access_token } => CloudCredentials::Gcp {
                    access_token: access_token.expose_secret().to_owned(),
                    expires_at,
                },
                ProviderCredentials::Azure { account, token } => CloudCredentials::Azure {
                    account: account.clone(),
                    token: AzureToken::Bearer(token.expose_secret().to_owned()),
                    expires_at,
                },
            };
            let built = crab_storage::build_object_store_with_endpoint(
                &direct.container,
                object_store_credentials(credentials)?,
                direct.endpoint.as_deref(),
            )?;
            let identity = crab_storage::BucketIdentity::new(
                built.provider,
                direct.endpoint.as_deref().unwrap_or(&direct.container),
                &direct.container,
            );
            let multipart_identity = built.multipart_identity;
            let mut store = Store::new(built.inner)
                .with_bucket_identity(identity)
                .with_target_identity(built.target_identity);
            if let Some(signer) = built.signer {
                store = store.with_signer(signer);
            }
            if let (Some(multipart), Some(identity)) = (built.multipart, multipart_identity) {
                store = store.with_multipart(multipart, identity);
            }
            store
        }
        TransferTransport::Gateway { gateway } => {
            let inner = std::sync::Arc::new(gateway_store::GatewayObjectStore::new(
                gateway_client,
                &gateway.service_url,
                gateway.token.expose_secret(),
                &repository_prefix,
                grant
                    .storage_scope
                    .staging
                    .as_ref()
                    .map(|staging| staging.prefix.as_str()),
            )?);
            let target_identity = crab_storage::identity::endpoint_identity(&gateway.service_url)?;
            Store::new(inner)
                .with_bucket_identity(crab_storage::BucketIdentity::local_unset())
                .with_target_identity(target_identity)
        }
    }
    .with_storage_scope(crab_types::storage::StorageScope {
        repo_prefix: repository_prefix.clone(),
        global_prefix,
        source_repo: repository_prefix.clone(),
        scope_hash: grant.repository_id.simple().to_string().repeat(2),
    });
    Ok(ManagedStore {
        store,
        repository_prefix,
    })
}

/// Composes canonical reads with immutable writes under one push session.
///
/// The staging grant must belong to the same physical repository as the read
/// store. Canonical writes are rewritten beneath the grant's exact staging
/// prefix and recorded for protected finalization.
#[cfg(feature = "refreshing-store")]
pub fn build_push_store_from_transfer_grant(
    read: ManagedStore,
    staging_grant: &TransferGrant,
    gateway_client: reqwest::Client,
) -> Result<ManagedStore> {
    let staging = staging_grant
        .storage_scope
        .staging
        .as_ref()
        .ok_or_else(|| AuthStoreError::InvalidCredentials {
            reason: "managed push grant omitted its staging scope".to_owned(),
        })?;
    if staging_grant.operation != crab_auth::TransferOperation::PushUpload
        || staging_grant.storage_scope.repository_prefix != read.repository_prefix
    {
        return Err(AuthStoreError::AuthFailed {
            path: "managed push grant did not match the resolved repository".to_owned(),
        });
    }
    let write = build_store_from_transfer_grant(staging_grant, gateway_client)?;
    let store = read.store.with_staging_write_store(
        staging.prefix.clone(),
        std::sync::Arc::clone(write.store.inner()),
    );
    Ok(ManagedStore {
        store,
        repository_prefix: read.repository_prefix,
    })
}

/// Errors raised while composing auth credentials into storage handles.
#[derive(Debug, thiserror::Error)]
pub enum AuthStoreError {
    /// Storage-domain object-store construction failed.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// Auth credentials cannot be used for the requested store shape.
    #[error("invalid auth storage credentials: {reason}")]
    InvalidCredentials {
        /// Reason the credentials are not valid for this store shape.
        reason: String,
    },

    /// Auth credentials failed a local authorization contract check.
    #[error("authentication failed for {path}")]
    AuthFailed {
        /// Path or scope that failed validation.
        path: String,
    },
}

/// Builds a provider object-store from resolved auth credentials.
pub fn build_object_store_from_credentials(
    bucket: &str,
    credentials: CloudCredentials,
) -> Result<BuiltObjectStore> {
    let credentials = object_store_credentials(credentials)?;
    Ok(build_object_store(bucket, credentials)?)
}

/// Builds a storage-domain store from resolved auth credentials.
pub fn build_store_from_credentials(bucket: &str, credentials: CloudCredentials) -> Result<Store> {
    let built = build_object_store_from_credentials(bucket, credentials)?;
    let identity = crab_storage::BucketIdentity::new(built.provider, bucket, bucket);
    let multipart_identity = built.multipart_identity;
    let mut store = Store::new(built.inner)
        .with_bucket_identity(identity)
        .with_target_identity(built.target_identity);
    if let Some(signer) = built.signer {
        store = store.with_signer(signer);
    }
    if let (Some(multipart), Some(identity)) = (built.multipart, multipart_identity) {
        store = store.with_multipart(multipart, identity);
    }
    Ok(store)
}

/// Builds a protected-push store, including scoped Azure read routes when present.
pub fn build_protected_push_store(
    bucket: &str,
    credentials: CloudCredentials,
    upload_prefix: &str,
) -> Result<Store> {
    match credentials {
        CloudCredentials::AzureScoped {
            account,
            read_scopes,
            write_token,
            write_prefix,
            expires_at,
        } => {
            if write_prefix.trim_matches('/') != upload_prefix.trim_matches('/') {
                return Err(AuthStoreError::AuthFailed {
                    path: "crab-auth Azure write SAS prefix did not match prepare upload_prefix"
                        .into(),
                });
            }

            let write = build_object_store_from_credentials(
                bucket,
                CloudCredentials::Azure {
                    account: account.clone(),
                    token: write_token,
                    expires_at,
                },
            )?;

            let mut read_routes = Vec::with_capacity(read_scopes.len());
            let mut default_read = write.inner.clone();
            for scope in read_scopes {
                let read = build_object_store_from_credentials(
                    bucket,
                    CloudCredentials::Azure {
                        account: account.clone(),
                        token: scope.token,
                        expires_at,
                    },
                )?;
                if read_routes.is_empty() {
                    default_read = read.inner.clone();
                }
                read_routes.push((scope.prefix, read.inner));
            }
            let identity = crab_storage::BucketIdentity::new(
                crab_storage::StorageProviderKind::Azure,
                bucket,
                bucket,
            );
            let store = Store::new(default_read)
                .with_bucket_identity(identity)
                .with_target_identity(write.target_identity)
                .with_read_routes(read_routes);
            Ok(store.with_staging_write_store(upload_prefix.to_owned(), write.inner))
        }
        other => Ok(build_store_from_credentials(bucket, other)?
            .with_staging_writes(upload_prefix.to_owned())),
    }
}

fn object_store_credentials(credentials: CloudCredentials) -> Result<ObjectStoreCredentials> {
    match credentials {
        CloudCredentials::StaticEnv { provider } => {
            Ok(ObjectStoreCredentials::StaticEnv { provider })
        }
        CloudCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            region,
            ..
        } => Ok(ObjectStoreCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            region,
        }),
        CloudCredentials::Gcp { access_token, .. } => {
            Ok(ObjectStoreCredentials::Gcp { access_token })
        }
        CloudCredentials::Azure { account, token, .. } => Ok(ObjectStoreCredentials::Azure {
            account,
            token: azure_authorization(token),
        }),
        CloudCredentials::AzureScoped { .. } => Err(AuthStoreError::InvalidCredentials {
            reason: "Azure split credentials are only valid for protected push stores".into(),
        }),
    }
}

fn azure_authorization(token: AzureToken) -> AzureAuthorization {
    match token {
        AzureToken::Bearer(token) => AzureAuthorization::Bearer(token),
        AzureToken::Sas(sas) => AzureAuthorization::Sas(sas),
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crab_auth::AzureReadScope;
    #[cfg(feature = "refreshing-store")]
    use crab_auth::{
        GatewayAccess, SecretString, StagingScope, TransferGrant, TransferOperation,
        TransferPermission, TransferScope, TransferTransport,
    };
    use crab_types::storage::StorageProviderKind;

    use super::*;

    #[test]
    fn build_store_from_credentials_sets_provider_identity() {
        let store = build_store_from_credentials(
            "bucket",
            CloudCredentials::Aws {
                access_key_id: "access".into(),
                secret_access_key: "secret".into(),
                session_token: Some("session".into()),
                expires_at: SystemTime::UNIX_EPOCH,
                region: "us-east-1".into(),
            },
        )
        .expect("static S3 builder does not perform network I/O");

        assert_eq!(
            store.bucket_identity(),
            crab_storage::BucketIdentity::new(StorageProviderKind::S3, "bucket", "bucket")
        );
    }

    #[test]
    fn build_store_from_credentials_rejects_scoped_azure_credentials() {
        let result = build_store_from_credentials(
            "bucket",
            CloudCredentials::AzureScoped {
                account: "account".into(),
                read_scopes: Vec::new(),
                write_token: AzureToken::Bearer("token".into()),
                write_prefix: "repo/staging".into(),
                expires_at: SystemTime::UNIX_EPOCH,
            },
        );

        assert!(matches!(
            result,
            Err(AuthStoreError::InvalidCredentials { .. })
        ));
    }

    #[test]
    fn protected_push_store_rejects_scoped_azure_write_prefix_mismatch() {
        let result = build_protected_push_store(
            "container",
            CloudCredentials::AzureScoped {
                account: "account".into(),
                read_scopes: vec![AzureReadScope {
                    prefix: "repo/manifest".into(),
                    token: AzureToken::Bearer("read".into()),
                }],
                write_token: AzureToken::Bearer("write".into()),
                write_prefix: "repo/staging".into(),
                expires_at: SystemTime::UNIX_EPOCH,
            },
            "other/staging",
        );

        assert!(matches!(result, Err(AuthStoreError::AuthFailed { .. })));
    }

    #[test]
    fn protected_push_store_accepts_scoped_azure_credentials() {
        let store = build_protected_push_store(
            "container",
            CloudCredentials::AzureScoped {
                account: "account".into(),
                read_scopes: vec![AzureReadScope {
                    prefix: "repo/manifest".into(),
                    token: AzureToken::Bearer("read".into()),
                }],
                write_token: AzureToken::Bearer("write".into()),
                write_prefix: "repo/staging".into(),
                expires_at: SystemTime::UNIX_EPOCH,
            },
            "repo/staging",
        )
        .expect("Azure builder construction does not perform network I/O");

        assert_eq!(store.staging_write_prefix(), Some("repo/staging"));
        assert_eq!(
            store.bucket_identity(),
            crab_storage::BucketIdentity::new(StorageProviderKind::Azure, "container", "container")
        );
    }

    #[cfg(feature = "refreshing-store")]
    #[test]
    fn managed_grant_routes_global_objects_inside_its_repository_scope() {
        let repository_id = uuid::Uuid::now_v7();
        let repository_prefix = format!("environments/test/repositories/{repository_id}");
        let grant = TransferGrant {
            schema_version: 1,
            grant_id: uuid::Uuid::now_v7(),
            repository_id,
            operation: TransferOperation::Fetch,
            expires_at: time::OffsetDateTime::now_utc() + time::Duration::minutes(5),
            permissions: vec![TransferPermission::ReadObject],
            storage_scope: TransferScope {
                repository_prefix: repository_prefix.clone(),
                staging: None,
            },
            transport: TransferTransport::Gateway {
                gateway: GatewayAccess {
                    service_url: "https://objects.crab.test/v1".to_owned(),
                    token: SecretString::new("grant-token").expect("valid test token"),
                },
            },
        };

        let managed = build_store_from_transfer_grant(&grant, reqwest::Client::new())
            .expect("build managed store");
        let layout =
            crab_storage::StoreLayout::new(managed.store, managed.repository_prefix.clone());

        assert_eq!(layout.repo_prefix(), repository_prefix);
        assert_eq!(layout.global_prefix(), format!("{repository_prefix}/.crab"));
    }

    #[cfg(feature = "refreshing-store")]
    #[test]
    fn managed_push_store_preserves_canonical_reads_and_routes_exact_staging_writes() {
        let repository_id = uuid::Uuid::now_v7();
        let push_id = uuid::Uuid::now_v7();
        let repository_prefix = format!("environments/test/repositories/{repository_id}");
        let staging_prefix = format!("{repository_prefix}/staging/{}", push_id.simple());
        let read = ManagedStore {
            store: Store::new(std::sync::Arc::new(object_store::memory::InMemory::new())),
            repository_prefix: repository_prefix.clone(),
        };
        let grant = TransferGrant {
            schema_version: 1,
            grant_id: uuid::Uuid::now_v7(),
            repository_id,
            operation: TransferOperation::PushUpload,
            expires_at: time::OffsetDateTime::now_utc() + time::Duration::minutes(5),
            permissions: vec![TransferPermission::CreateImmutableObject],
            storage_scope: TransferScope {
                repository_prefix: repository_prefix.clone(),
                staging: Some(StagingScope {
                    push_id,
                    prefix: staging_prefix.clone(),
                }),
            },
            transport: TransferTransport::Gateway {
                gateway: GatewayAccess {
                    service_url: "https://objects.crab.test/v1".to_owned(),
                    token: SecretString::new("push-token").expect("valid test token"),
                },
            },
        };

        let managed = build_push_store_from_transfer_grant(read, &grant, reqwest::Client::new())
            .expect("compose managed push store");

        assert_eq!(managed.repository_prefix, repository_prefix);
        assert_eq!(
            managed.store.staging_write_prefix(),
            Some(staging_prefix.as_str())
        );
    }
}
