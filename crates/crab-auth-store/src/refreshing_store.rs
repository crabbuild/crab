use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, stream};
use object_store::multipart::{MultipartStore, PartId};
use object_store::path::Path;
use object_store::signer::Signer;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartId, MultipartUpload, ObjectMeta,
    ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, warn};

use crab_auth::{CredentialProvider, CredentialResolution};

use crate::Result;

#[derive(Clone)]
pub struct RefreshingStoreParts {
    pub inner: Arc<dyn ObjectStore>,
    pub signer: Option<Arc<dyn Signer>>,
    pub multipart: Option<Arc<dyn MultipartStore>>,
    pub multipart_identity: Option<crab_storage::BucketIdentity>,
    pub target_identity: [u8; 32],
}

type StoreBuilder = dyn Fn(CredentialResolution) -> Result<RefreshingStoreParts> + Send + Sync;

pub struct RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized,
{
    provider: Arc<P>,
    bucket: String,
    prefix: String,
    operation: String,
    state: Arc<RwLock<RefreshingStoreParts>>,
    refresh_lock: Arc<Mutex<()>>,
    build: Arc<StoreBuilder>,
}

impl<P> Clone for RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized,
{
    fn clone(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            bucket: self.bucket.clone(),
            prefix: self.prefix.clone(),
            operation: self.operation.clone(),
            state: Arc::clone(&self.state),
            refresh_lock: Arc::clone(&self.refresh_lock),
            build: Arc::clone(&self.build),
        }
    }
}

impl<P> RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized,
{
    pub fn new(
        provider: Arc<P>,
        bucket: String,
        prefix: String,
        operation: String,
        initial: RefreshingStoreParts,
        build: Arc<StoreBuilder>,
    ) -> Self {
        Self {
            provider,
            bucket,
            prefix,
            operation,
            state: Arc::new(RwLock::new(initial)),
            refresh_lock: Arc::new(Mutex::new(())),
            build,
        }
    }

    pub async fn has_signer(&self) -> bool {
        self.state.read().await.signer.is_some()
    }

    pub async fn has_multipart(&self) -> bool {
        let state = self.state.read().await;
        state.multipart.is_some() && state.multipart_identity.is_some()
    }

    async fn parts_for_operation(&self) -> object_store::Result<RefreshingStoreParts> {
        if self.provider.needs_refresh() {
            self.refresh_parts(false).await
        } else {
            Ok(self.state.read().await.clone())
        }
    }

    async fn refresh_parts(&self, force: bool) -> object_store::Result<RefreshingStoreParts> {
        let _guard = self.refresh_lock.lock().await;
        if !force && !self.provider.needs_refresh() {
            return Ok(self.state.read().await.clone());
        }

        debug!(
            bucket = %self.bucket,
            prefix = %self.prefix,
            operation = %self.operation,
            force,
            "refreshing object-store credentials"
        );

        let resolution = self
            .provider
            .refresh_for(&self.bucket, &self.prefix, &self.operation)
            .await
            .map_err(to_object_store_error)?;
        let parts = (self.build)(resolution).map_err(to_object_store_error)?;
        let mut state = self.state.write().await;
        if parts.multipart_identity != state.multipart_identity {
            return Err(object_store::Error::Generic {
                store: "refreshing",
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "refreshed credentials changed the multipart destination",
                )
                .into(),
            });
        }
        // A credential rotation may not redirect an operation whose snapshot and
        // publication proof were bound to the initial provider target.
        if parts.target_identity != state.target_identity {
            return Err(to_object_store_error(crate::AuthStoreError::AuthFailed {
                path: "credential refresh changed the storage target; resolve a new operation"
                    .to_owned(),
            }));
        }
        *state = parts.clone();
        Ok(parts)
    }

    async fn retryable_unary<T, F, Fut>(&self, op: F) -> object_store::Result<T>
    where
        F: Fn(Arc<dyn ObjectStore>) -> Fut,
        Fut: Future<Output = object_store::Result<T>>,
    {
        let parts = self.parts_for_operation().await?;
        match op(Arc::clone(&parts.inner)).await {
            Ok(value) => Ok(value),
            Err(err) if self.should_retry_auth_error(&err) => {
                warn!(
                    error = %err,
                    bucket = %self.bucket,
                    prefix = %self.prefix,
                    operation = %self.operation,
                    "object-store auth failed; refreshing credentials and retrying once"
                );
                let parts = self.refresh_parts(true).await?;
                op(Arc::clone(&parts.inner)).await
            }
            Err(err) => Err(err),
        }
    }

    fn should_retry_auth_error(&self, err: &object_store::Error) -> bool {
        match err {
            object_store::Error::Unauthenticated { .. } => true,
            object_store::Error::PermissionDenied { .. } => self.provider.needs_refresh(),
            object_store::Error::Generic { source, .. } => {
                let msg = source.to_string().to_lowercase();
                msg.contains("expired") && msg.contains("token")
            }
            _ => false,
        }
    }
}

impl<P> fmt::Debug for RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshingObjectStore")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("operation", &self.operation)
            .finish_non_exhaustive()
    }
}

impl<P> fmt::Display for RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RefreshingObjectStore({}/{})", self.bucket, self.prefix)
    }
}

#[async_trait]
impl<P> ObjectStore for RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized + 'static,
{
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.retryable_unary(|inner| {
            let location = location.clone();
            let payload = payload.clone();
            let opts = opts.clone();
            async move { inner.put_opts(&location, payload, opts).await }
        })
        .await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let parts = self.parts_for_operation().await?;
        parts.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.retryable_unary(|inner| {
            let location = location.clone();
            let options = options.clone();
            async move { inner.get_opts(&location, options).await }
        })
        .await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let this = self.clone();
        locations
            .map(move |location| {
                let this = this.clone();
                async move {
                    let location = location?;
                    this.retryable_unary(|inner| {
                        let location = location.clone();
                        async move { inner.delete(&location).await }
                    })
                    .await?;
                    Ok(location)
                }
            })
            .buffered(10)
            .boxed()
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let this = self.clone();
        let prefix = prefix.cloned();
        stream::once(async move {
            match this.parts_for_operation().await {
                Ok(parts) => parts.inner.list(prefix.as_ref()),
                Err(err) => stream::once(async move { Err(err) }).boxed(),
            }
        })
        .flatten()
        .boxed()
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.retryable_unary(|inner| {
            let prefix = prefix.cloned();
            async move { inner.list_with_delimiter(prefix.as_ref()).await }
        })
        .await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.retryable_unary(|inner| {
            let from = from.clone();
            let to = to.clone();
            let options = options.clone();
            async move { inner.copy_opts(&from, &to, options).await }
        })
        .await
    }
}

#[async_trait]
impl<P> Signer for RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized + 'static,
{
    async fn signed_url(
        &self,
        method: reqwest::Method,
        path: &Path,
        expires_in: Duration,
    ) -> object_store::Result<url::Url> {
        self.retryable_signer_unary(|signer| {
            let method = method.clone();
            let path = path.clone();
            async move { signer.signed_url(method, &path, expires_in).await }
        })
        .await
    }
}

#[async_trait]
impl<P> MultipartStore for RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized + 'static,
{
    async fn create_multipart(&self, path: &Path) -> object_store::Result<MultipartId> {
        self.retryable_multipart_unary(|multipart| {
            let path = path.clone();
            async move { multipart.create_multipart(&path).await }
        })
        .await
    }

    async fn create_multipart_opts(
        &self,
        path: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<MultipartId> {
        self.retryable_multipart_unary(|multipart| {
            let path = path.clone();
            let opts = opts.clone();
            async move { multipart.create_multipart_opts(&path, opts).await }
        })
        .await
    }

    async fn put_part(
        &self,
        path: &Path,
        id: &MultipartId,
        part_idx: usize,
        data: PutPayload,
    ) -> object_store::Result<PartId> {
        self.retryable_multipart_unary(|multipart| {
            let path = path.clone();
            let id = id.clone();
            let data = data.clone();
            async move { multipart.put_part(&path, &id, part_idx, data).await }
        })
        .await
    }

    async fn complete_multipart(
        &self,
        path: &Path,
        id: &MultipartId,
        parts: Vec<PartId>,
    ) -> object_store::Result<PutResult> {
        self.retryable_multipart_unary(|multipart| {
            let path = path.clone();
            let id = id.clone();
            let parts = parts.clone();
            async move { multipart.complete_multipart(&path, &id, parts).await }
        })
        .await
    }

    async fn abort_multipart(&self, path: &Path, id: &MultipartId) -> object_store::Result<()> {
        self.retryable_multipart_unary(|multipart| {
            let path = path.clone();
            let id = id.clone();
            async move { multipart.abort_multipart(&path, &id).await }
        })
        .await
    }
}

impl<P> RefreshingObjectStore<P>
where
    P: CredentialProvider + ?Sized,
{
    async fn retryable_signer_unary<T, F, Fut>(&self, op: F) -> object_store::Result<T>
    where
        F: Fn(Arc<dyn Signer>) -> Fut,
        Fut: Future<Output = object_store::Result<T>>,
    {
        let parts = self.parts_for_operation().await?;
        let signer = parts.signer.ok_or_else(signer_not_supported)?;
        match op(Arc::clone(&signer)).await {
            Ok(value) => Ok(value),
            Err(err) if self.should_retry_auth_error(&err) => {
                let parts = self.refresh_parts(true).await?;
                let signer = parts.signer.ok_or_else(signer_not_supported)?;
                op(Arc::clone(&signer)).await
            }
            Err(err) => Err(err),
        }
    }

    async fn retryable_multipart_unary<T, F, Fut>(&self, op: F) -> object_store::Result<T>
    where
        F: Fn(Arc<dyn MultipartStore>) -> Fut,
        Fut: Future<Output = object_store::Result<T>>,
    {
        let parts = self.parts_for_operation().await?;
        let multipart = parts.multipart.ok_or_else(multipart_not_supported)?;
        match op(Arc::clone(&multipart)).await {
            Ok(value) => Ok(value),
            Err(error) if self.should_retry_auth_error(&error) => {
                let parts = self.refresh_parts(true).await?;
                let multipart = parts.multipart.ok_or_else(multipart_not_supported)?;
                op(Arc::clone(&multipart)).await
            }
            Err(error) => Err(error),
        }
    }
}

fn signer_not_supported() -> object_store::Error {
    object_store::Error::NotSupported {
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "current refreshed backend cannot sign URLs",
        )
        .into(),
    }
}

fn multipart_not_supported() -> object_store::Error {
    object_store::Error::NotSupported {
        source: std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "current refreshed backend has no stable multipart upload IDs",
        )
        .into(),
    }
}

fn to_object_store_error<E>(err: E) -> object_store::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    object_store::Error::Generic {
        store: "refreshing",
        source: Box::new(err),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::SystemTime;

    use bytes::Bytes;
    use crab_auth::CloudCredentials;
    use futures_util::stream;
    use object_store::{GetResultPayload, PutResult};

    #[derive(Default)]
    struct MockProvider {
        needs_refresh: AtomicBool,
        resolve_count: AtomicUsize,
    }

    #[async_trait]
    impl CredentialProvider for MockProvider {
        type Error = crate::AuthStoreError;

        async fn resolve(
            &self,
            _bucket: &str,
            _prefix: &str,
            _operation: &str,
        ) -> std::result::Result<CredentialResolution, Self::Error> {
            self.resolve_count.fetch_add(1, Ordering::SeqCst);
            self.needs_refresh.store(false, Ordering::SeqCst);
            Ok(CredentialResolution::new(CloudCredentials::Gcp {
                access_token: "test".into(),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            }))
        }

        fn needs_refresh(&self) -> bool {
            self.needs_refresh.load(Ordering::SeqCst)
        }

        async fn refresh(&self) -> std::result::Result<CredentialResolution, Self::Error> {
            self.resolve("", "", "").await
        }

        async fn refresh_for(
            &self,
            bucket: &str,
            prefix: &str,
            operation: &str,
        ) -> std::result::Result<CredentialResolution, Self::Error> {
            assert_eq!(bucket, "bucket");
            assert_eq!(prefix, "repo");
            assert_eq!(operation, "fetch");
            self.resolve(bucket, prefix, operation).await
        }

        fn identity(&self) -> Option<&str> {
            None
        }
    }

    #[derive(Debug)]
    struct MockStore {
        generation: usize,
        unauth_once: bool,
        forbidden: bool,
    }

    struct MockMultipart {
        generation: usize,
        unauthenticated: bool,
    }

    #[async_trait]
    impl MultipartStore for MockMultipart {
        async fn create_multipart(&self, path: &Path) -> object_store::Result<MultipartId> {
            if self.unauthenticated {
                return Err(object_store::Error::Unauthenticated {
                    path: path.to_string(),
                    source: "expired".into(),
                });
            }
            Ok(format!("gen-{}", self.generation))
        }

        async fn put_part(
            &self,
            _path: &Path,
            _id: &MultipartId,
            _part_idx: usize,
            _data: PutPayload,
        ) -> object_store::Result<PartId> {
            Ok(PartId {
                content_id: format!("gen-{}", self.generation),
            })
        }

        async fn complete_multipart(
            &self,
            _path: &Path,
            _id: &MultipartId,
            _parts: Vec<PartId>,
        ) -> object_store::Result<PutResult> {
            Ok(PutResult {
                e_tag: Some(format!("gen-{}", self.generation)),
                version: None,
                extensions: Default::default(),
            })
        }

        async fn abort_multipart(
            &self,
            _path: &Path,
            _id: &MultipartId,
        ) -> object_store::Result<()> {
            Ok(())
        }
    }

    impl fmt::Display for MockStore {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "MockStore({})", self.generation)
        }
    }

    #[async_trait]
    impl ObjectStore for MockStore {
        async fn put_opts(
            &self,
            _location: &Path,
            _payload: PutPayload,
            _opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            Ok(PutResult {
                e_tag: Some(format!("gen-{}", self.generation)),
                version: None,
                extensions: Default::default(),
            })
        }

        async fn put_multipart_opts(
            &self,
            _location: &Path,
            _opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            Err(object_store::Error::NotImplemented {
                operation: "put_multipart_opts".to_owned(),
                implementer: "MockStore".to_owned(),
            })
        }

        async fn get_opts(
            &self,
            location: &Path,
            _options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if self.unauth_once {
                return Err(object_store::Error::Unauthenticated {
                    path: location.to_string(),
                    source: "expired".into(),
                });
            }
            if self.forbidden {
                return Err(object_store::Error::PermissionDenied {
                    path: location.to_string(),
                    source: "forbidden".into(),
                });
            }
            let bytes = Bytes::from(format!("gen-{}", self.generation));
            Ok(GetResult {
                payload: GetResultPayload::Stream(stream::once(async move { Ok(bytes) }).boxed()),
                meta: ObjectMeta {
                    location: location.clone(),
                    last_modified: SystemTime::now().into(),
                    size: 5,
                    e_tag: None,
                    version: None,
                },
                range: 0..5,
                attributes: Default::default(),
                extensions: Default::default(),
            })
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            locations
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            let location = prefix.cloned().unwrap_or_else(|| Path::from("root"));
            stream::once(async move {
                Ok(ObjectMeta {
                    location,
                    last_modified: SystemTime::now().into(),
                    size: 0,
                    e_tag: None,
                    version: None,
                })
            })
            .boxed()
        }

        async fn list_with_delimiter(
            &self,
            _prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            Ok(ListResult {
                common_prefixes: vec![],
                objects: vec![],
                extensions: Default::default(),
            })
        }

        async fn copy_opts(
            &self,
            _from: &Path,
            _to: &Path,
            _options: CopyOptions,
        ) -> object_store::Result<()> {
            Ok(())
        }
    }

    fn wrapper(
        provider: Arc<MockProvider>,
        build_count: Arc<AtomicUsize>,
        initial: RefreshingStoreParts,
    ) -> RefreshingObjectStore<MockProvider> {
        let multipart_identity = initial.multipart_identity.clone();
        let builder = Arc::new(move |_creds| {
            let generation = build_count.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation,
                    unauth_once: false,
                    forbidden: false,
                }),
                signer: None,
                multipart: Some(Arc::new(MockMultipart {
                    generation,
                    unauthenticated: false,
                })),
                multipart_identity: multipart_identity.clone(),
                target_identity: [0; 32],
            })
        });
        RefreshingObjectStore::new(
            provider,
            "bucket".into(),
            "repo".into(),
            "fetch".into(),
            initial,
            builder,
        )
    }

    #[tokio::test]
    async fn refresh_rejects_target_change_and_preserves_original_store() {
        for proactive in [false, true] {
            let provider = Arc::new(MockProvider::default());
            provider.needs_refresh.store(proactive, Ordering::SeqCst);
            let initial = RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 0,
                    unauth_once: !proactive,
                    forbidden: false,
                }),
                signer: None,
                target_identity: [1; 32],
                multipart: None,
                multipart_identity: None,
            };
            let store = wrapper(provider, Arc::new(AtomicUsize::new(0)), initial);
            let error = store.get(&Path::from("object")).await.unwrap_err();
            let object_store::Error::Generic { source, .. } = error else {
                panic!("target mismatch must retain its typed source");
            };
            assert!(matches!(
                source.downcast_ref::<crate::AuthStoreError>(),
                Some(crate::AuthStoreError::AuthFailed { .. })
            ));
            assert_eq!(store.state.read().await.target_identity, [1; 32]);
        }
    }

    #[tokio::test]
    async fn refreshes_before_operation_when_provider_needs_refresh() {
        let provider = Arc::new(MockProvider::default());
        provider.needs_refresh.store(true, Ordering::SeqCst);
        let build_count = Arc::new(AtomicUsize::new(0));
        let store = wrapper(
            Arc::clone(&provider),
            Arc::clone(&build_count),
            RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 0,
                    unauth_once: false,
                    forbidden: false,
                }),
                signer: None,
                multipart: None,
                multipart_identity: None,
                target_identity: [0; 32],
            },
        );

        let got = store
            .get(&Path::from("object"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, Bytes::from_static(b"gen-1"));
        assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 1);
        assert_eq!(build_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_once_after_unauthenticated() {
        let provider = Arc::new(MockProvider::default());
        let build_count = Arc::new(AtomicUsize::new(0));
        let store = wrapper(
            Arc::clone(&provider),
            Arc::clone(&build_count),
            RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 0,
                    unauth_once: true,
                    forbidden: false,
                }),
                signer: None,
                multipart: None,
                multipart_identity: None,
                target_identity: [0; 32],
            },
        );

        let got = store
            .get(&Path::from("object"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, Bytes::from_static(b"gen-1"));
        assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_retry_fresh_permission_denied() {
        let provider = Arc::new(MockProvider::default());
        let build_count = Arc::new(AtomicUsize::new(0));
        let store = wrapper(
            Arc::clone(&provider),
            Arc::clone(&build_count),
            RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 0,
                    unauth_once: false,
                    forbidden: true,
                }),
                signer: None,
                multipart: None,
                multipart_identity: None,
                target_identity: [0; 32],
            },
        );

        let err = store
            .get(&Path::from("object"))
            .await
            .expect_err("fresh 403 should not refresh");
        assert!(matches!(err, object_store::Error::PermissionDenied { .. }));
        assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 0);
        assert_eq!(build_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cloned_raw_inner_handle_refreshes_too() {
        let provider = Arc::new(MockProvider::default());
        provider.needs_refresh.store(true, Ordering::SeqCst);
        let build_count = Arc::new(AtomicUsize::new(0));
        let store = wrapper(
            Arc::clone(&provider),
            Arc::clone(&build_count),
            RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 0,
                    unauth_once: false,
                    forbidden: false,
                }),
                signer: None,
                multipart: None,
                multipart_identity: None,
                target_identity: [0; 32],
            },
        );
        let raw: Arc<dyn ObjectStore> = Arc::new(store);

        let got = raw
            .get(&Path::from("object"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, Bytes::from_static(b"gen-1"));
        assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multipart_handle_refreshes_after_unauthenticated_response() {
        let provider = Arc::new(MockProvider::default());
        let build_count = Arc::new(AtomicUsize::new(0));
        let store = wrapper(
            Arc::clone(&provider),
            Arc::clone(&build_count),
            RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 0,
                    unauth_once: false,
                    forbidden: false,
                }),
                signer: None,
                multipart: Some(Arc::new(MockMultipart {
                    generation: 0,
                    unauthenticated: true,
                })),
                multipart_identity: Some(crab_storage::BucketIdentity::new(
                    crab_storage::StorageProviderKind::S3,
                    "endpoint-a",
                    "bucket",
                )),
                target_identity: [0; 32],
            },
        );

        let upload_id = store.create_multipart(&Path::from("object")).await.unwrap();

        assert_eq!(upload_id, "gen-1");
        assert_eq!(provider.resolve_count.load(Ordering::SeqCst), 1);
        assert_eq!(build_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multipart_refresh_rejects_changed_destination() {
        let provider = Arc::new(MockProvider::default());
        provider.needs_refresh.store(true, Ordering::SeqCst);
        let initial_identity = crab_storage::BucketIdentity::new(
            crab_storage::StorageProviderKind::S3,
            "endpoint-a",
            "bucket",
        );
        let builder = Arc::new(move |_creds| {
            Ok(RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 1,
                    unauth_once: false,
                    forbidden: false,
                }),
                signer: None,
                multipart: Some(Arc::new(MockMultipart {
                    generation: 1,
                    unauthenticated: false,
                })),
                multipart_identity: Some(crab_storage::BucketIdentity::new(
                    crab_storage::StorageProviderKind::S3,
                    "endpoint-b",
                    "bucket",
                )),
                target_identity: [0; 32],
            })
        });
        let store = RefreshingObjectStore::new(
            provider,
            "bucket".into(),
            "repo".into(),
            "fetch".into(),
            RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation: 0,
                    unauth_once: false,
                    forbidden: false,
                }),
                signer: None,
                multipart: Some(Arc::new(MockMultipart {
                    generation: 0,
                    unauthenticated: false,
                })),
                multipart_identity: Some(initial_identity),
                target_identity: [0; 32],
            },
            builder,
        );

        let error = store
            .create_multipart(&Path::from("object"))
            .await
            .expect_err("destination changes must fail closed");

        assert!(
            error
                .to_string()
                .contains("changed the multipart destination")
        );
    }
}
