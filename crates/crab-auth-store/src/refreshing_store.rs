use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::{StreamExt, stream};
use object_store::path::Path;
use object_store::signer::Signer;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, warn};

use crab_auth::{CredentialProvider, CredentialResolution};

use crate::Result;

pub struct RefreshingStoreParts {
    pub inner: Arc<dyn ObjectStore>,
    pub signer: Option<Arc<dyn Signer>>,
}

impl Clone for RefreshingStoreParts {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            signer: self.signer.as_ref().map(Arc::clone),
        }
    }
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
        *self.state.write().await = parts.clone();
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
        let builder = Arc::new(move |_creds| {
            let generation = build_count.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(RefreshingStoreParts {
                inner: Arc::new(MockStore {
                    generation,
                    unauth_once: false,
                    forbidden: false,
                }),
                signer: None,
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
}
