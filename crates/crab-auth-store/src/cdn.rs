//! Compose refreshable viewer authority with the selected origin store.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use crab_auth::{
    CloudCredentials, CredentialProvider, CredentialResolution,
    cdn::{CdnProvider, CdnViewerGrant},
};
use crab_storage::{
    BuiltObjectStore, ReadAuthorization, StorageError, Store,
    cdn::{
        CdnAccess, CdnConsumer, CdnCredential, CdnCredentials, CdnProfile, CdnRoute, CdnTransport,
    },
};
use futures_util::FutureExt;
use object_store::path::Path;

use crate::{
    AuthStoreError, Result, build_object_store_from_credentials,
    refreshing_store::{
        ReadAuthority, RefreshingObjectStore, RefreshingStoreParts, to_object_store_error,
    },
    store_from_built,
};

/// Builds a direct read store with origin and viewer authority refreshed together.
///
/// The initial resolution must come from the supplied provider for this
/// repository and operation. Rotation must preserve the native target, the
/// captured view, and the approved viewer route.
pub fn build_direct_cdn_store<P: CredentialProvider + ?Sized + 'static>(
    provider: Arc<P>,
    bucket: String,
    source_repo: String,
    operation: String,
    resolution: CredentialResolution,
    timeout: Duration,
    consumer: CdnConsumer,
) -> Result<Store> {
    let (built, authority) = direct_backend(&bucket, resolution)?;
    let repository_prefix = source_repo.clone();
    let request = (bucket.clone(), source_repo.clone(), operation.clone());
    let refreshing = Arc::new(RefreshingObjectStore::new_bound(
        provider,
        bucket.clone(),
        source_repo,
        operation,
        (backend_parts(&built), authority.clone()),
        move |provider| {
            let (bucket, source_repo, operation) = request.clone();
            async move {
                let next = provider
                    .refresh_for(&bucket, &source_repo, &operation)
                    .await
                    .map_err(to_object_store_error)?;
                let (built, authority) =
                    direct_backend(&bucket, next).map_err(to_object_store_error)?;
                Ok((backend_parts(&built), authority))
            }
            .boxed()
        },
    ));
    bind_read_store(
        &bucket,
        None,
        &repository_prefix,
        built,
        &authority,
        refreshing,
        timeout,
        Some((consumer, CdnProfile::Direct)),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "bound native store and CDN composition share the captured read context"
)]
pub(crate) fn bind_read_store<P: Send + Sync + ?Sized + 'static>(
    bucket: &str,
    endpoint: Option<&str>,
    repository_prefix: &str,
    mut built: BuiltObjectStore,
    authority: &ReadAuthority,
    refreshing: Arc<RefreshingObjectStore<P>>,
    timeout: Duration,
    context: Option<(CdnConsumer, CdnProfile)>,
) -> Result<Store> {
    built.inner = refreshing.clone();
    built.signer = built.signer.map(|_| refreshing.clone() as _);
    built.multipart = built.multipart.map(|_| refreshing.clone() as _);
    let mut store = store_from_built(bucket, endpoint, built);
    if let Some(scope) = &authority.scope {
        store = store.with_storage_scope(scope.clone());
    }
    store = store.with_read_authorization(refreshing.clone(), repository_prefix)?;
    if let (Some(grant), Some((consumer, profile))) = (&authority.viewer, context) {
        store = attach_route(
            store,
            grant,
            Arc::new(Viewer(refreshing)),
            timeout,
            consumer,
            profile,
        )?;
    }
    Ok(store)
}

fn direct_backend(
    bucket: &str,
    resolution: CredentialResolution,
) -> Result<(BuiltObjectStore, ReadAuthority)> {
    let expires_at = match &resolution.credentials {
        CloudCredentials::Aws { expires_at, .. }
        | CloudCredentials::Gcp { expires_at, .. }
        | CloudCredentials::Azure { expires_at, .. } => *expires_at,
        _ => {
            return Err(AuthStoreError::InvalidCredentials {
                reason: "direct CDN authorization requires expiring native credentials".into(),
            });
        }
    };
    let built = build_object_store_from_credentials(bucket, resolution.credentials)?;
    let authority =
        ReadAuthority::new(&built, resolution.storage_scope, resolution.cdn, expires_at)?;
    Ok((built, authority))
}

pub(crate) fn backend_parts(built: &BuiltObjectStore) -> RefreshingStoreParts {
    RefreshingStoreParts {
        inner: Arc::clone(&built.inner),
        signer: built.signer.clone(),
        multipart: built.multipart.clone(),
        multipart_identity: built.multipart_identity.clone(),
        target_identity: built.target_identity,
    }
}

struct Viewer<P: ?Sized>(Arc<RefreshingObjectStore<P>>);

#[async_trait::async_trait]
impl<P: Send + Sync + ?Sized + 'static> ReadAuthorization for RefreshingObjectStore<P> {
    async fn check(&self) -> crab_storage::Result<()> {
        self.viewer(None)
            .await
            .map(|_| ())
            .map_err(|source| StorageError::ObjectStore { source })
    }
}

#[async_trait::async_trait]
impl<P: Send + Sync + ?Sized + 'static> CdnCredentials for Viewer<P> {
    async fn credential(
        &self,
        key: &Path,
        rejected: Option<u64>,
    ) -> crab_storage::Result<Option<CdnCredential>> {
        let (generation, grant) = self
            .0
            .viewer(rejected)
            .await
            .map_err(|source| StorageError::ObjectStore { source })?;
        viewer_credential(&grant.ok_or_else(scope_changed)?, generation, key)
    }
}

fn attach_route(
    store: Store,
    grant: &CdnViewerGrant,
    credentials: Arc<dyn CdnCredentials>,
    timeout: Duration,
    consumer: CdnConsumer,
    profile: CdnProfile,
) -> Result<Store> {
    let target = store.target_identity().copied().ok_or_else(scope_changed)?;
    Ok(store.with_cdn_route(
        CdnRoute::new(
            target,
            grant.repo_prefix.clone(),
            grant.global_prefix.clone(),
            CdnAccess::Private(credentials),
            CdnTransport::new(&grant.base_url)?,
            timeout,
        )
        .for_consumer(consumer, profile),
    )?)
}

fn viewer_credential(
    grant: &CdnViewerGrant,
    generation: u64,
    key: &Path,
) -> crab_storage::Result<Option<CdnCredential>> {
    let now = SystemTime::now();
    let Some(token) = grant.viewer_for_object(key.as_ref(), now) else {
        if grant.expires_at
            <= now
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |age| age.as_secs())
        {
            return Err(StorageError::AuthExpired {
                path: key.to_string(),
            });
        }
        return Ok(None);
    };
    let token = token.expose_secret().to_owned();
    let credential = match grant.provider {
        CdnProvider::Cloudfront | CdnProvider::CloudCdn => CdnCredential::cookie(generation, token),
        CdnProvider::FrontDoor => CdnCredential::bearer(generation, token),
    };
    let expires_at = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(grant.expires_at))
        .ok_or_else(scope_changed)?;
    Ok(Some(credential.with_expiration(expires_at)))
}

pub(crate) fn scope_changed() -> StorageError {
    StorageError::Forbidden {
        path: "CDN refresh changed the authorized route; resolve a new operation".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_auth::credential_response::parse_credential_response;
    use crab_storage::StorageScope;
    use object_store::{ObjectStoreExt, memory::InMemory};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Provider {
        resolution: Option<CredentialResolution>,
        refreshes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl CredentialProvider for Provider {
        type Error = std::io::Error;

        async fn resolve(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> std::result::Result<CredentialResolution, Self::Error> {
            self.resolution
                .clone()
                .ok_or_else(|| std::io::Error::other("fixture unavailable"))
        }

        fn needs_refresh(&self) -> bool {
            false
        }

        async fn refresh(&self) -> std::result::Result<CredentialResolution, Self::Error> {
            self.refreshes.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            self.resolution
                .clone()
                .ok_or_else(|| std::io::Error::other("fixture unavailable"))
        }

        fn identity(&self) -> Option<&str> {
            None
        }
    }

    fn resolution(base: &str) -> CredentialResolution {
        let expires_at = SystemTime::now() + Duration::from_secs(120);
        let epoch = expires_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let response = parse_credential_response(&format!(r#"{{
            "provider": "gcp",
            "credentials": {{}},
            "expires_at": "2026-12-01T00:00:00Z",
            "permissions": ["read"],
            "cdn": {{
                "provider": "cloud_cdn",
                "origin": "gs://test-bucket",
                "base_url": "{base}",
                "repo_prefix": "org/repo",
                "global_prefix": null,
                "expires_at": {epoch},
                "grants": [{{
                    "prefix": "org/repo/v2/history/",
                    "credential": "Cloud-CDN-Cookie=URLPrefix=encoded:Expires=123:KeyName=test:Signature=opaque"
                }}]
            }}
        }}"#)).unwrap();
        CredentialResolution {
            credentials: CloudCredentials::Gcp {
                access_token: "fixture".into(),
                expires_at,
            },
            storage_scope: None,
            cdn: response.cdn,
        }
    }

    fn credentials(next: Option<CredentialResolution>) -> (Viewer<Provider>, Arc<Provider>) {
        let initial = resolution("https://downloads.example.com");
        direct_credentials(initial, next)
    }

    fn direct_credentials(
        initial: CredentialResolution,
        next: Option<CredentialResolution>,
    ) -> (Viewer<Provider>, Arc<Provider>) {
        let provider = Arc::new(Provider {
            resolution: next,
            refreshes: AtomicUsize::new(0),
        });
        let (built, authority) = direct_backend("test-bucket", initial).unwrap();
        let credentials = Viewer(Arc::new(RefreshingObjectStore::new_bound(
            Arc::clone(&provider),
            "test-bucket".into(),
            "org/repo".into(),
            "fetch".into(),
            (backend_parts(&built), authority),
            |provider| {
                async move {
                    let next = provider.refresh().await.map_err(to_object_store_error)?;
                    let (built, authority) =
                        direct_backend("test-bucket", next).map_err(to_object_store_error)?;
                    Ok((backend_parts(&built), authority))
                }
                .boxed()
            },
        )));
        (credentials, provider)
    }

    #[tokio::test]
    async fn concurrent_denials_refresh_the_rejected_generation_once() {
        let (credentials, provider) =
            credentials(Some(resolution("https://downloads.example.com")));
        let key = Path::from("org/repo/v2/history/object");
        let (a, b) = tokio::join!(
            credentials.credential(&key, Some(0)),
            credentials.credential(&key, Some(0))
        );
        assert!(a.unwrap().is_some());
        assert!(b.unwrap().is_some());
        assert_eq!(provider.refreshes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn concurrent_denials_share_a_short_lived_replacement_viewer() {
        let mut replacement = resolution("https://downloads.example.com");
        let expires_at = SystemTime::now() + Duration::from_secs(14);
        replacement.cdn.as_mut().unwrap().expires_at = expires_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (credentials, provider) = credentials(Some(replacement));
        let key = Path::from("org/repo/v2/history/object");
        let (a, b) = tokio::join!(
            credentials.credential(&key, Some(0)),
            credentials.credential(&key, Some(0))
        );
        assert!(a.unwrap().is_some());
        assert!(b.unwrap().is_some());
        assert_eq!(credentials.0.viewer(None).await.unwrap().0, 1);
        assert_eq!(provider.refreshes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn viewer_refresh_refuses_a_new_deployment() {
        let (credentials, _) = credentials(Some(resolution("https://other.example.com")));
        let key = Path::from("org/repo/v2/history/object");
        let outcome = credentials.credential(&key, Some(0)).await;
        assert!(matches!(
            outcome,
            Err(StorageError::ObjectStore {
                source: object_store::Error::PermissionDenied { .. },
            })
        ));
        // A viewer rejection also retires the captured native backend.
        let origin = credentials.0.get(&key).await.unwrap_err();
        assert!(matches!(
            origin,
            object_store::Error::PermissionDenied { .. }
        ));
    }

    #[tokio::test]
    async fn expiring_origin_and_rejected_viewer_share_one_refresh() {
        let mut initial = resolution("https://downloads.example.com");
        let expires_at = SystemTime::now() + Duration::from_secs(14);
        initial.cdn.as_mut().unwrap().expires_at = expires_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        initial.credentials = CloudCredentials::Gcp {
            access_token: "old".into(),
            expires_at,
        };
        let provider = Arc::new(Provider {
            resolution: Some(resolution("https://downloads.example.com")),
            refreshes: AtomicUsize::new(0),
        });
        let (built, authority) = direct_backend("test-bucket", initial).unwrap();
        let mut initial = backend_parts(&built);
        initial.inner = Arc::new(InMemory::new());
        let key = Path::from("org/repo/v2/history/object");
        let replacement = Arc::new(InMemory::new());
        replacement
            .put(&key, bytes::Bytes::from_static(b"fresh origin").into())
            .await
            .unwrap();
        let wrapper = Arc::new(RefreshingObjectStore::new_bound(
            Arc::clone(&provider),
            "test-bucket".into(),
            "org/repo".into(),
            "fetch".into(),
            (initial, authority),
            move |provider| {
                let replacement = replacement.clone();
                async move {
                    let next = provider.refresh().await.map_err(to_object_store_error)?;
                    let (built, authority) =
                        direct_backend("test-bucket", next).map_err(to_object_store_error)?;
                    let mut parts = backend_parts(&built);
                    parts.inner = replacement;
                    Ok((parts, authority))
                }
                .boxed()
            },
        ));
        let viewer = Viewer(Arc::clone(&wrapper));
        let (origin, edge) = tokio::join!(wrapper.get(&key), viewer.credential(&key, Some(0)));
        assert_eq!(origin.unwrap().bytes().await.unwrap(), "fresh origin");
        assert!(edge.unwrap().is_some());
        assert_eq!(wrapper.viewer(None).await.unwrap().0, 1);
        assert_eq!(provider.refreshes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn origin_only_view_refuses_a_new_physical_generation() {
        let mut initial = resolution("https://downloads.example.com");
        let expires_at = SystemTime::now() + Duration::from_secs(14);
        initial.credentials = CloudCredentials::Gcp {
            access_token: "old".into(),
            expires_at,
        };
        let view = |generation: &str| StorageScope {
            repo_prefix: format!("org/repo/acl-views/v1/{generation}"),
            global_prefix: format!("org/repo/acl-views/v1/{generation}/.crab"),
            source_repo: "org/repo".into(),
            scope_hash: "d".repeat(64),
        };
        initial.storage_scope = Some(view("old"));
        initial.cdn = None;
        let mut next = resolution("https://downloads.example.com");
        next.storage_scope = Some(view("new"));
        next.cdn = None;
        let provider = Arc::new(Provider {
            resolution: Some(next),
            refreshes: AtomicUsize::new(0),
        });
        let store = build_direct_cdn_store(
            Arc::clone(&provider),
            "test-bucket".into(),
            "org/repo".into(),
            "fetch".into(),
            initial,
            Duration::from_secs(30),
            CdnConsumer::Cli,
        )
        .unwrap();
        assert_eq!(store.storage_scope(), Some(&view("old")));
        let key = Path::from("org/repo/acl-views/v1/old/v2/root");
        for _ in 0..2 {
            let error = store.get_with_etag(&key).await.unwrap_err();
            assert!(matches!(error, StorageError::Forbidden { .. }));
        }
        assert_eq!(provider.refreshes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn failed_viewer_refresh_is_shared_and_keeps_rejected_authority_closed() {
        let (credentials, provider) = credentials(None);
        let cache_reader = Store::new(Arc::new(InMemory::new()))
            .with_target_identity([1; 32])
            .with_read_authorization(credentials.0.clone(), "org/repo")
            .unwrap();
        cache_reader.check_read_authority().await.unwrap();
        let key = Path::from("org/repo/v2/history/object");
        let (a, b) = tokio::join!(
            credentials.credential(&key, Some(0)),
            credentials.credential(&key, Some(0))
        );
        assert!(a.is_err());
        assert!(b.is_err());
        assert!(credentials.credential(&key, None).await.is_err());
        let error = cache_reader.check_read_authority().await.unwrap_err();
        assert!(crab_storage::read_rejection(&error).is_some());
        assert_eq!(provider.refreshes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn dropping_a_refresh_waiter_does_not_replay_the_issuer_request() {
        let (credentials, provider) =
            credentials(Some(resolution("https://downloads.example.com")));
        let key = Path::from("org/repo/v2/history/object");
        let mut first = credentials.credential(&key, Some(0));
        assert!(futures_util::poll!(&mut first).is_pending());
        assert_eq!(provider.refreshes.load(Ordering::Relaxed), 1);
        drop(first);
        assert!(
            credentials
                .credential(&key, Some(0))
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(provider.refreshes.load(Ordering::Relaxed), 1);
    }
}
