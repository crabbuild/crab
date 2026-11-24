use std::{sync::Arc, time::Duration};

use crab_auth::{TransferGrant, TransferTransport};
use crab_storage::{
    BuiltObjectStore, StorageError,
    cdn::{CdnConsumer, CdnProfile},
};
use futures_util::{FutureExt, future::BoxFuture};

use crate::{
    ManagedStore, Result, build_managed_direct,
    cdn::{backend_parts, bind_read_store, scope_changed},
    managed_grant_expiry, managed_grant_scope,
    refreshing_store::{ReadAuthority, RefreshingObjectStore, to_object_store_error},
};

pub(crate) fn build_managed_read_store<P: Send + Sync + 'static>(
    source: Arc<P>,
    grant: TransferGrant,
    consumer: Option<CdnConsumer>,
    refresh: impl Fn(Arc<P>) -> BoxFuture<'static, object_store::Result<TransferGrant>>
    + Send
    + Sync
    + 'static,
) -> Result<ManagedStore> {
    let TransferTransport::Direct { direct } = &grant.transport else {
        return Err(scope_changed().into());
    };
    let (built, authority) = managed_backend(&grant)?;
    let repository_prefix = grant.storage_scope.repository_prefix;
    let mut permissions = grant.permissions;
    permissions.sort_unstable();
    let binding = Arc::new((
        grant.schema_version,
        grant.repository_id,
        grant.operation,
        permissions,
    ));
    let refreshing = Arc::new(RefreshingObjectStore::new_bound(
        source,
        direct.container.clone(),
        repository_prefix.clone(),
        format!("{:?}", grant.operation),
        (backend_parts(&built), authority.clone()),
        move |source| {
            let next = refresh(source);
            let binding = Arc::clone(&binding);
            async move {
                let mut grant = next.await?;
                grant.permissions.sort_unstable();
                let (version, repository, operation, permissions) = binding.as_ref();
                // A validated grant may still describe a different authority
                // than the one whose data and view this operation captured.
                if grant.schema_version != *version
                    || grant.repository_id != *repository
                    || grant.operation != *operation
                    || grant.permissions != *permissions
                {
                    return Err(to_object_store_error(scope_changed()));
                }
                let (built, authority) = managed_backend(&grant).map_err(to_object_store_error)?;
                Ok((backend_parts(&built), authority))
            }
            .boxed()
        },
    ));
    let store = bind_read_store(
        &direct.container,
        direct.endpoint.as_deref(),
        &repository_prefix,
        built,
        &authority,
        refreshing,
        Duration::from_secs(300),
        consumer.map(|consumer| (consumer, CdnProfile::Managed)),
    )?;
    Ok(ManagedStore {
        store,
        repository_prefix,
    })
}

fn managed_backend(grant: &TransferGrant) -> Result<(BuiltObjectStore, ReadAuthority)> {
    let TransferTransport::Direct { direct } = &grant.transport else {
        return Err(scope_changed().into());
    };
    if grant.storage_scope.staging.is_some()
        || direct.object_prefix != grant.storage_scope.repository_prefix
    {
        return Err(StorageError::Forbidden {
            path: "managed read credential does not match its repository scope".into(),
        }
        .into());
    }
    let expires_at = managed_grant_expiry(grant)?;
    let built = build_managed_direct(direct, expires_at)?;
    let authority = ReadAuthority::new(
        &built,
        Some(managed_grant_scope(grant)),
        grant.cdn.clone(),
        expires_at,
    )?;
    Ok((built, authority))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crab_auth::{
        DirectObjectCredentials, ProviderCredentials, SecretString, TransferOperation,
        TransferPermission, TransferScope, credential_response::parse_credential_response,
    };
    use object_store::path::Path;
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;

    fn grant(repository_id: Uuid, key_id: &str, seconds: i64) -> TransferGrant {
        let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(seconds);
        let repository_prefix = format!("environments/test/repositories/{repository_id}");
        let cdn = parse_credential_response(&format!(r#"{{
            "provider": "aws", "credentials": {{}},
            "expires_at": "2030-01-01T00:00:00Z", "permissions": ["read"],
            "cdn": {{
                "provider": "cloudfront", "origin": "s3://crab-managed-fixture",
                "base_url": "https://downloads.example.com",
                "repo_prefix": "{repository_prefix}", "global_prefix": null,
                "expires_at": {}, "grants": [{{
                    "prefix": "{repository_prefix}/v2/history/",
                    "credential": "CloudFront-Policy=fixture-policy; CloudFront-Signature=fixture-signature; CloudFront-Key-Pair-Id=test-key; CloudFront-Hash-Algorithm=SHA256"
                }}]
            }}
        }}"#, expires_at.unix_timestamp())).unwrap().cdn;
        TransferGrant {
            schema_version: 2,
            grant_id: Uuid::now_v7(),
            repository_id,
            operation: TransferOperation::Fetch,
            expires_at,
            permissions: vec![
                TransferPermission::ReadObject,
                TransferPermission::ReadMetadata,
            ],
            storage_scope: TransferScope {
                repository_prefix: repository_prefix.clone(),
                staging: None,
            },
            transport: TransferTransport::Direct {
                direct: DirectObjectCredentials {
                    endpoint: None,
                    region: Some("us-east-1".into()),
                    container: "crab-managed-fixture".into(),
                    object_prefix: repository_prefix,
                    credentials: ProviderCredentials::Aws {
                        access_key_id: SecretString::new(key_id).unwrap(),
                        secret_access_key: SecretString::new("fixture-secret-key").unwrap(),
                        session_token: SecretString::new("fixture-session-token").unwrap(),
                    },
                },
            },
            cdn,
            cdn_approval_sha256: Some("a".repeat(64)),
        }
    }

    fn store(
        initial: TransferGrant,
        replacement: TransferGrant,
    ) -> (ManagedStore, Arc<(TransferGrant, AtomicUsize)>) {
        let source = Arc::new((replacement, AtomicUsize::new(0)));
        let store = build_managed_read_store(
            Arc::clone(&source),
            initial,
            Some(CdnConsumer::Cli),
            |source| {
                async move {
                    source.1.fetch_add(1, Ordering::Relaxed);
                    tokio::task::yield_now().await;
                    Ok(source.0.clone())
                }
                .boxed()
            },
        )
        .unwrap();
        (store, source)
    }

    #[tokio::test]
    async fn concurrent_managed_reads_rotate_native_credentials_and_keep_the_repo_route() {
        for use_cdn in [false, true] {
            let repo = Uuid::now_v7();
            let mut initial = grant(repo, "initial-access-id", 14);
            let mut replacement = grant(repo, "updated-access-id", 120);
            replacement.permissions.reverse();
            if use_cdn {
                // Model a viewer expiring between grant validation and store
                // use. Its expiry rotates the still-current native credentials.
                initial.expires_at += time::Duration::seconds(120);
                initial.cdn.as_mut().unwrap().expires_at = OffsetDateTime::now_utc()
                    .unix_timestamp()
                    .try_into()
                    .unwrap();
            } else {
                initial.cdn = None;
                replacement.cdn = None;
            }
            let (managed, source) = store(initial, replacement);
            let reader = &managed.store;
            let hash = "a".repeat(64);
            let path = Path::from(format!(
                "{}/v2/history/aa/{hash}",
                managed.repository_prefix
            ));
            let global = Path::from(format!(
                "{}/.crab/xorbs/aa/{hash}",
                managed.repository_prefix
            ));
            assert_eq!(
                reader.cdn_attempt(&path).is_some(),
                use_cdn && crab_storage::cdn::qualification_enabled()
            );
            assert!(reader.cdn_attempt(&global).is_none());

            let sign = || reader.signed_url(&path, Duration::from_secs(10));
            let (a, b) = tokio::join!(sign(), sign());
            for url in [a.unwrap(), b.unwrap()] {
                assert!(url.query_pairs().any(|(key, value)| {
                    key == "X-Amz-Credential" && value.starts_with("updated-access-id/")
                }));
            }
            assert_eq!(source.1.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test]
    async fn changed_managed_permissions_or_viewer_route_stop_origin_reads() {
        let repo = Uuid::now_v7();
        let mut changed_permissions = grant(repo, "updated-access-id", 120);
        changed_permissions.permissions.pop();
        let mut withdrawn_viewer = grant(repo, "updated-access-id", 120);
        withdrawn_viewer.cdn = None;
        for replacement in [changed_permissions, withdrawn_viewer] {
            let (managed, source) = store(grant(repo, "initial-access-id", 14), replacement);
            let path = Path::from(format!("{}/v2/root", managed.repository_prefix));
            for _ in 0..2 {
                let error = managed.store.get_with_etag(&path).await.unwrap_err();
                assert!(matches!(error, StorageError::Forbidden { .. }));
            }
            assert_eq!(source.1.load(Ordering::Relaxed), 1);
        }
    }
}
