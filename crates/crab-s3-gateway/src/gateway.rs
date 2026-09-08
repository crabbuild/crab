use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use bytes::Bytes;
use crab_cache_store::{CacheConfig, CachingStore};
use crab_git::pointer_detect::PointerKind;
use crab_remote_git::{
    ContentClassification, EntryKind, OperationKind, RemoteGitRepository, RemoteGitRuntime,
    RepositoryIdentity, RepositoryOptions, Revision,
};
use crab_storage::{StorageProviderKind, Store, StoreLayout, build_static_env_store};
use futures_util::StreamExt as _;
use s3s::{S3, S3Request, S3Response, S3Result, dto::*, s3_error};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{Config, RepositoryAccess, RepositoryConfig, auth::GatewayAuth, mutation, namespace};

pub(crate) struct Repository {
    pub(crate) config: RepositoryConfig,
    pub(crate) store: Store,
    pub(crate) layout: StoreLayout<Store>,
    pub(crate) identity: RepositoryIdentity,
    hydrator: crab_read::ShardHydrator,
    lfs: crab_lfs::LfsObjectStore,
}

impl Repository {
    pub(crate) fn new(config: RepositoryConfig, store: Store) -> crate::Result<Self> {
        let layout = StoreLayout::new(store.clone(), config.prefix.clone());
        let caching = CachingStore::new(store.clone(), CacheConfig::default())?;
        let read_layout = crab_read::ReadStoreLayout::with_global_prefix(
            store.clone(),
            layout.repo_prefix().to_owned(),
            layout.global_prefix().to_owned(),
        );
        Ok(Self {
            hydrator: crab_read::ReadRuntimeBuilder::new(caching, read_layout, 16).build()?,
            lfs: crab_lfs::LfsObjectStore::new(store.clone(), layout.repo_prefix()),
            identity: RepositoryIdentity::new(
                format!("{}:{}", provider_name(config.provider), config.bucket),
                config.prefix.clone(),
                1,
            )?,
            config,
            store,
            layout,
        })
    }

    fn access(&self, principal: &str) -> Option<RepositoryAccess> {
        self.config
            .members
            .iter()
            .find(|member| member.principal == principal)
            .map(|member| member.access)
    }
}

#[derive(Clone)]
pub(crate) struct Gateway {
    repositories: Arc<BTreeMap<String, Repository>>,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    auth: GatewayAuth,
    region: Arc<str>,
    admission: Arc<Semaphore>,
    cancellation: CancellationToken,
}

struct ReadObject {
    content: ReadContent,
    size: u64,
    etag: String,
    modified: Timestamp,
    attributes: Option<crate::attributes::ObjectAttributes>,
}

#[derive(Clone)]
enum ReadContent {
    Ordinary(Bytes),
    CrabPointer(Bytes),
    LfsPointer(crab_git::LfsPointer),
}

type ContentStream =
    Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, s3s::StdError>> + Send + 'static>>;

impl ReadContent {
    async fn stream(
        self,
        repository: &Repository,
        range: std::ops::Range<u64>,
    ) -> S3Result<ContentStream> {
        use futures_util::{StreamExt as _, TryStreamExt as _};
        use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

        match self {
            Self::Ordinary(bytes) => {
                let start = usize::try_from(range.start).map_err(|_| s3_error!(InvalidRange))?;
                let end = usize::try_from(range.end).map_err(|_| s3_error!(InvalidRange))?;
                Ok(Box::pin(futures_util::stream::once(async move {
                    Ok(bytes.slice(start..end))
                })))
            }
            Self::LfsPointer(pointer) => {
                let (_, actual, stream) = repository
                    .lfs
                    .get_stream(&pointer.oid, pointer.size, Some(range.clone()))
                    .await
                    .map_err(|error| gateway_error(error.into()))?;
                if actual != range {
                    return Err(s3_error!(InvalidObjectState));
                }
                Ok(Box::pin(
                    stream.map_err(|error| Box::new(error) as s3s::StdError),
                ))
            }
            Self::CrabPointer(pointer_bytes) => {
                let PointerKind::Crab(pointer) = crab_git::classify(&pointer_bytes) else {
                    return Err(s3_error!(InvalidObjectState));
                };
                let directory = tempfile::tempdir().map_err(|error| gateway_error(error.into()))?;
                let path = directory.path().join("content");
                repository
                    .hydrator
                    .reconstruct_to_path(&pointer, &path)
                    .await
                    .map_err(|error| gateway_error(error.into()))?;
                let mut file = tokio::fs::File::open(path)
                    .await
                    .map_err(|error| gateway_error(error.into()))?;
                file.seek(std::io::SeekFrom::Start(range.start))
                    .await
                    .map_err(|error| gateway_error(error.into()))?;
                let reader = tokio_util::io::ReaderStream::new(file.take(range.end - range.start));
                let stream = futures_util::stream::try_unfold(
                    (reader, directory),
                    |(mut reader, directory)| async move {
                        match reader.next().await {
                            Some(Ok(bytes)) => Ok(Some((bytes, (reader, directory)))),
                            Some(Err(error)) => Err(error),
                            None => Ok(None),
                        }
                    },
                )
                .map_err(|error| Box::new(error) as s3s::StdError);
                Ok(Box::pin(stream))
            }
        }
    }

    async fn spool(
        self,
        repository: &Repository,
        range: std::ops::Range<u64>,
        max_bytes: u64,
    ) -> S3Result<crate::content::Spool> {
        use futures_util::StreamExt as _;

        let mut stream = self.stream(repository, range).await?;
        let mut writer = crate::content::SpoolWriter::new()
            .await
            .map_err(content_error)?;
        while let Some(chunk) = stream.next().await {
            writer
                .write(
                    &chunk.map_err(|error| {
                        tracing::warn!(%error, "S3 source object stream failed");
                        s3_error!(InternalError)
                    })?,
                    max_bytes,
                )
                .await
                .map_err(content_error)?;
        }
        writer.finish().await.map_err(content_error)
    }
}

impl Gateway {
    pub(crate) fn new(config: Config, cancellation: CancellationToken) -> crate::Result<Self> {
        let auth = GatewayAuth::load(&config)?;
        let region = Arc::from(config.region.clone());
        let mut stores: BTreeMap<String, Store> = BTreeMap::new();
        let mut repositories = BTreeMap::new();
        for entry in config.repositories {
            let store_key = format!("{}:{}", provider_name(entry.provider), entry.bucket);
            let store = match stores.get(&store_key) {
                Some(store) => store.clone(),
                None => {
                    let store = build_store(&entry)?;
                    stores.insert(store_key, store.clone());
                    store
                }
            };
            let repository = Repository::new(entry.clone(), store)?;
            repositories.insert(entry.name.clone(), repository);
        }
        Ok(Self {
            repositories: Arc::new(repositories),
            runtime: Arc::new(RemoteGitRuntime::default()),
            options: RepositoryOptions::default(),
            auth,
            region,
            admission: Arc::new(Semaphore::new(32)),
            cancellation,
        })
    }

    pub(crate) async fn initialize_repositories(&self) -> crate::Result<()> {
        for repository in self.repositories.values() {
            let head = format!("refs/heads/{}", repository.config.default_branch);
            crab_write::initialize::initialize_repository(
                &repository.store,
                &repository.layout,
                &head,
            )
            .await?;
        }
        Ok(())
    }

    pub(crate) fn auth(&self) -> GatewayAuth {
        self.auth.clone()
    }

    pub(crate) async fn shutdown(&self) {
        self.cancellation.cancel();
        self.runtime.shutdown().await;
    }

    fn principal<'a, T>(&'a self, req: &S3Request<T>) -> S3Result<&'a str> {
        if req
            .region
            .as_ref()
            .is_some_and(|region| region.as_str() != self.region.as_ref())
        {
            return Err(s3_error!(
                AuthorizationHeaderMalformed,
                "The authorization region does not match this endpoint"
            ));
        }
        if req
            .service
            .as_deref()
            .is_some_and(|service| service != "s3")
        {
            return Err(s3_error!(SignatureDoesNotMatch));
        }
        let access_key = req
            .credentials
            .as_ref()
            .map(|credentials| credentials.access_key.as_str())
            .ok_or_else(|| s3_error!(AccessDenied, "Signature is required"))?;
        self.auth
            .principal(access_key)
            .ok_or_else(|| s3_error!(AccessDenied))
    }

    fn repository<'a, T>(
        &'a self,
        req: &S3Request<T>,
        bucket: &str,
        required: RepositoryAccess,
    ) -> S3Result<&'a Repository> {
        let principal = self.principal(req)?;
        let repository = self
            .repositories
            .get(bucket)
            .ok_or_else(|| s3_error!(NoSuchBucket))?;
        if repository
            .access(principal)
            .is_some_and(|access| access >= required)
        {
            Ok(repository)
        } else {
            Err(s3_error!(AccessDenied))
        }
    }

    async fn open(&self, repository: &Repository) -> S3Result<RemoteGitRepository> {
        crate::repository::open_current(
            repository,
            Arc::clone(&self.runtime),
            self.options,
            &self.cancellation,
        )
        .await
        .map_err(gateway_error)
    }

    async fn read_object(&self, repository: &Repository, key: &str) -> S3Result<ReadObject> {
        let address = namespace::object_address(key).map_err(namespace_error)?;
        let repo = self.open(repository).await?;
        let operation = repo
            .operation(OperationKind::Repository, &self.cancellation)
            .await
            .map_err(remote_error)?;
        let result = async {
            let snapshot = repo
                .snapshot(
                    &Revision::parse(&address.reference).map_err(remote_error)?,
                    &operation,
                )
                .await
                .map_err(remote_error)?;
            let commit = snapshot.commit(&operation).await.map_err(remote_error)?;
            let blob = snapshot
                .read_blob(&address.path, &operation)
                .await
                .map_err(remote_error)?;
            if blob.metadata.kind != EntryKind::Blob {
                return Err(s3_error!(InvalidObjectState));
            }
            let manifest = crate::attributes::load(repository, snapshot.commit_oid())
                .await
                .map_err(gateway_error)?;
            let path = std::str::from_utf8(address.path.as_bytes())
                .map_err(|_| s3_error!(InvalidObjectState))?;
            let attributes = manifest.object(path, blob.metadata.oid).cloned();
            let modified_seconds = attributes
                .as_ref()
                .and_then(|value| i64::try_from(value.modified_seconds).ok())
                .unwrap_or(commit.committer.seconds);
            let modified = timestamp(modified_seconds)?;
            let (content, size) = classify_blob(blob)?;
            let etag = match attributes.as_ref() {
                Some(value) => value.etag.clone(),
                None => match &content {
                    ReadContent::Ordinary(bytes) => md5_hex(bytes),
                    ReadContent::CrabPointer(_) | ReadContent::LfsPointer(_) => {
                        let spool = content.clone().spool(repository, 0..size, u64::MAX).await?;
                        crate::content::md5_hex(&spool.digests.md5)
                    }
                },
            };
            Ok(ReadObject {
                content,
                size,
                etag,
                modified,
                attributes,
            })
        }
        .await;
        finish(operation, result).await
    }

    fn writable_address<T>(
        &self,
        req: &S3Request<T>,
        bucket: &str,
        key: &str,
    ) -> S3Result<(&Repository, namespace::ObjectAddress, String)> {
        let principal = self.principal(req)?.to_owned();
        let repository = self.repository(req, bucket, RepositoryAccess::Write)?;
        let address = namespace::object_address(key).map_err(namespace_error)?;
        let branch = address
            .branch
            .as_deref()
            .and_then(|name| name.strip_prefix("refs/heads/"))
            .ok_or_else(|| s3_error!(MethodNotAllowed, "Writes require a branch key"))?;
        if repository
            .config
            .protected_branches
            .iter()
            .any(|protected| protected == branch)
        {
            return Err(s3_error!(
                AccessDenied,
                "The destination branch is protected"
            ));
        }
        Ok((repository, address, principal))
    }
}

fn build_store(entry: &RepositoryConfig) -> crate::Result<Store> {
    Ok(build_static_env_store(&entry.bucket, entry.provider)?)
}

fn classify_blob(blob: crab_remote_git::Blob) -> S3Result<(ReadContent, u64)> {
    let physical_size = u64::try_from(blob.bytes.len()).map_err(|_| s3_error!(EntityTooLarge))?;
    let logical_size = blob.metadata.logical_size.unwrap_or(physical_size);
    if logical_size > crate::content::MAX_MULTIPART_OBJECT_BYTES {
        return Err(s3_error!(EntityTooLarge));
    }
    let content = match blob.metadata.classification {
        ContentClassification::OrdinaryGit => ReadContent::Ordinary(blob.bytes),
        ContentClassification::CrabPointer => ReadContent::CrabPointer(blob.bytes),
        ContentClassification::LfsPointer => {
            let PointerKind::Lfs(pointer) = crab_git::classify(&blob.bytes) else {
                return Err(s3_error!(InvalidObjectState));
            };
            if pointer.size != logical_size {
                return Err(s3_error!(InvalidObjectState));
            }
            ReadContent::LfsPointer(pointer)
        }
    };
    Ok((content, logical_size))
}

fn provider_name(provider: StorageProviderKind) -> &'static str {
    match provider {
        StorageProviderKind::S3 => "s3",
        StorageProviderKind::Gcs => "gcs",
        StorageProviderKind::Azure => "azure",
        StorageProviderKind::Local => "local",
    }
}

async fn mutation_bytes(repository: &Repository, spool: &crate::content::Spool) -> S3Result<Bytes> {
    mutation_bytes_with_inline_limit(repository, spool, crate::content::INLINE_GIT_BLOB_BYTES).await
}

async fn mutation_bytes_with_inline_limit(
    repository: &Repository,
    spool: &crate::content::Spool,
    inline_limit: u64,
) -> S3Result<Bytes> {
    if spool.size <= inline_limit {
        return spool.bytes().await.map_err(content_error);
    }
    // The LFS object is content-addressed and uploaded before its pointer commit.
    // Crab's GC grace period protects this brief publication window and cleans an
    // orphan if the later ref mutation fails.
    repository
        .lfs
        .put_stream_with_size(&spool.digests.sha256, Some(spool.size), spool.path())
        .await
        .map_err(|error| gateway_error(error.into()))?;
    Ok(Bytes::from(
        crab_git::LfsPointer {
            oid: spool.digests.sha256,
            size: spool.size,
            extensions: Vec::new(),
        }
        .serialize(),
    ))
}

#[async_trait::async_trait]
impl S3 for Gateway {
    async fn list_buckets(
        &self,
        req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        if req
            .input
            .bucket_region
            .as_deref()
            .is_some_and(|region| region != self.region.as_ref())
        {
            return Err(s3_error!(InvalidRequest));
        }
        let principal = self.principal(&req)?;
        let mut buckets = self
            .repositories
            .values()
            .filter(|repository| repository.access(principal).is_some())
            .filter(|repository| {
                req.input
                    .prefix
                    .as_deref()
                    .is_none_or(|prefix| repository.config.name.starts_with(prefix))
            })
            .filter(|repository| {
                req.input
                    .continuation_token
                    .as_deref()
                    .is_none_or(|token| repository.config.name.as_str() > token)
            })
            .map(|repository| Bucket {
                name: Some(repository.config.name.clone()),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let limit = match req.input.max_buckets {
            Some(value) if (1..=10_000).contains(&value) => {
                usize::try_from(value).map_err(|_| s3_error!(InvalidArgument))?
            }
            Some(_) => return Err(s3_error!(InvalidArgument)),
            None => usize::MAX,
        };
        let truncated = buckets.len() > limit;
        buckets.truncate(limit);
        let continuation_token = truncated
            .then(|| buckets.last().and_then(|bucket| bucket.name.clone()))
            .flatten();
        Ok(S3Response::new(ListBucketsOutput {
            buckets: Some(buckets),
            continuation_token,
            prefix: req.input.prefix,
            ..Default::default()
        }))
    }

    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let mut response = S3Response::new(HeadBucketOutput::default());
        response.headers.insert(
            "x-amz-bucket-region",
            http::HeaderValue::from_str(&self.region).map_err(|_| s3_error!(InternalError))?,
        );
        Ok(response)
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_get_extensions(&req.input)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let object = self.read_object(repository, &req.input.key).await?;
        evaluate_conditions(
            req.input.if_match.as_ref(),
            req.input.if_none_match.as_ref(),
            req.input.if_modified_since.as_ref(),
            req.input.if_unmodified_since.as_ref(),
            &object.etag,
            &object.modified,
        )?;
        let checked = req
            .input
            .range
            .as_ref()
            .map(|range| range.check(object.size))
            .transpose()?;
        let (range, content_range) = match checked {
            Some(range) => {
                let header = format!("bytes {}-{}/{}", range.start, range.end - 1, object.size);
                (range.start..range.end, Some(header))
            }
            None => (0..object.size, None),
        };
        let content_length =
            i64::try_from(range.end - range.start).map_err(|_| s3_error!(InternalError))?;
        let body = object.content.stream(repository, range).await?;
        let body =
            http_body_util::StreamBody::new(body.map(|result| result.map(http_body::Frame::data)));
        let output = GetObjectOutput {
            accept_ranges: Some("bytes".to_owned()),
            body: Some(StreamingBlob::from(s3s::Body::http_body_unsync(body))),
            content_length: Some(content_length),
            content_range,
            content_type: req
                .input
                .response_content_type
                .or_else(|| {
                    object
                        .attributes
                        .as_ref()
                        .and_then(|value| value.content_type.clone())
                })
                .or_else(|| Some("application/octet-stream".to_owned())),
            cache_control: req.input.response_cache_control.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.cache_control.clone())
            }),
            content_disposition: req.input.response_content_disposition.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_disposition.clone())
            }),
            content_encoding: req.input.response_content_encoding.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_encoding.clone())
            }),
            content_language: req.input.response_content_language.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_language.clone())
            }),
            expires: req.input.response_expires.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.expires.clone())
            }),
            e_tag: Some(ETag::Strong(object.etag)),
            last_modified: Some(object.modified),
            metadata: object
                .attributes
                .map(|value| value.metadata.into_iter().collect()),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_head_extensions(&req.input)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let object = self.read_object(repository, &req.input.key).await?;
        evaluate_conditions(
            req.input.if_match.as_ref(),
            req.input.if_none_match.as_ref(),
            req.input.if_modified_since.as_ref(),
            req.input.if_unmodified_since.as_ref(),
            &object.etag,
            &object.modified,
        )?;
        let checked = req
            .input
            .range
            .as_ref()
            .map(|range| range.check(object.size))
            .transpose()?;
        let (content_length, content_range) = match checked {
            Some(range) => (
                range.end - range.start,
                Some(format!(
                    "bytes {}-{}/{}",
                    range.start,
                    range.end - 1,
                    object.size
                )),
            ),
            None => (object.size, None),
        };
        Ok(S3Response::new(HeadObjectOutput {
            accept_ranges: Some("bytes".to_owned()),
            content_length: Some(
                i64::try_from(content_length).map_err(|_| s3_error!(InternalError))?,
            ),
            content_range,
            cache_control: object
                .attributes
                .as_ref()
                .and_then(|value| value.cache_control.clone()),
            content_disposition: object
                .attributes
                .as_ref()
                .and_then(|value| value.content_disposition.clone()),
            content_encoding: object
                .attributes
                .as_ref()
                .and_then(|value| value.content_encoding.clone()),
            content_language: object
                .attributes
                .as_ref()
                .and_then(|value| value.content_language.clone()),
            content_type: object
                .attributes
                .as_ref()
                .and_then(|value| value.content_type.clone())
                .or_else(|| Some("application/octet-stream".to_owned())),
            e_tag: Some(ETag::Strong(object.etag)),
            expires: object
                .attributes
                .as_ref()
                .and_then(|value| value.expires.clone()),
            last_modified: Some(object.modified),
            metadata: object
                .attributes
                .map(|value| value.metadata.into_iter().collect()),
            ..Default::default()
        }))
    }

    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_put_extensions(&req.input)?;
        let condition = put_condition(&req.input)?;
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        let content_length = req.input.content_length;
        let content_md5 = req.input.content_md5.clone();
        let mut checksums = RequestChecksums::from(&req.input);
        let trailing_headers = req.trailing_headers.clone();
        let spool = crate::content::spool_body(
            req.input.body,
            content_length,
            crate::content::MAX_PUT_OBJECT_BYTES,
        )
        .await
        .map_err(content_error)?;
        checksums.merge_trailers(trailing_headers.as_ref())?;
        verify_content_md5(&spool.digests.md5, content_md5.as_deref())?;
        checksums.verify(&spool.digests)?;
        let etag = crate::content::md5_hex(&spool.digests.md5);
        let bytes = mutation_bytes(repository, &spool).await?;
        let outcome = mutation::apply(
            repository,
            Arc::clone(&self.runtime),
            self.options,
            address
                .branch
                .as_deref()
                .ok_or_else(|| s3_error!(MethodNotAllowed))?,
            &address.path,
            mutation::Change::Put {
                bytes,
                attributes: Box::new(crate::attributes::PutAttributes {
                    etag_override: Some(etag),
                    completion_upload_id: None,
                    logical_size: Some(spool.size),
                    cache_control: req.input.cache_control,
                    content_disposition: req.input.content_disposition,
                    content_encoding: req.input.content_encoding,
                    content_language: req.input.content_language,
                    content_type: req.input.content_type,
                    expires: req.input.expires,
                    metadata: req.input.metadata.unwrap_or_default().into_iter().collect(),
                }),
                condition,
            },
            &principal,
            &self.cancellation,
        )
        .await
        .map_err(mutation_error)?;
        Ok(S3Response::new(PutObjectOutput {
            e_tag: outcome.etag.map(ETag::Strong),
            checksum_crc32: checksums.crc32,
            checksum_crc32c: checksums.crc32c,
            checksum_crc64nvme: checksums.crc64nvme,
            checksum_sha1: checksums.sha1,
            checksum_sha256: checksums.sha256,
            checksum_type: checksums
                .algorithm
                .as_ref()
                .map(|_| ChecksumType::from_static(ChecksumType::FULL_OBJECT)),
            ..Default::default()
        }))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_delete_extensions(&req.input)?;
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        mutation::apply(
            repository,
            Arc::clone(&self.runtime),
            self.options,
            address
                .branch
                .as_deref()
                .ok_or_else(|| s3_error!(MethodNotAllowed))?,
            &address.path,
            mutation::Change::Delete,
            &principal,
            &self.cancellation,
        )
        .await
        .map_err(mutation_error)?;
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        if req.input.bypass_governance_retention.is_some()
            || req.input.expected_bucket_owner.is_some()
            || req.input.mfa.is_some()
            || req.input.request_payer.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        if req.input.delete.objects.len() > 1000 {
            return Err(s3_error!(
                MalformedXML,
                "DeleteObjects accepts at most 1000 keys"
            ));
        }
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let quiet = req.input.delete.quiet.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for object in req.input.delete.objects {
            if object.version_id.is_some()
                || object.e_tag.is_some()
                || object.last_modified_time.is_some()
                || object.size.is_some()
            {
                errors.push(s3s::dto::Error {
                    key: Some(object.key),
                    code: Some("NotImplemented".to_owned()),
                    message: Some(
                        "Per-object version and condition fields are unsupported".to_owned(),
                    ),
                    ..Default::default()
                });
                continue;
            }
            let key = object.key;
            let result = async {
                let address = namespace::object_address(&key).map_err(namespace_error)?;
                let branch = writable_branch(repository, &address)?;
                mutation::apply(
                    repository,
                    Arc::clone(&self.runtime),
                    self.options,
                    branch,
                    &address.path,
                    mutation::Change::Delete,
                    &principal,
                    &self.cancellation,
                )
                .await
                .map_err(mutation_error)
            }
            .await;
            match result {
                Ok(_) if !quiet => deleted.push(DeletedObject {
                    key: Some(key),
                    ..Default::default()
                }),
                Ok(_) => {}
                Err(error) => errors.push(s3s::dto::Error {
                    key: Some(key),
                    code: Some(error.code().as_str().to_owned()),
                    message: Some(
                        error
                            .message()
                            .unwrap_or("Object deletion failed")
                            .to_owned(),
                    ),
                    ..Default::default()
                }),
            }
        }
        Ok(S3Response::new(DeleteObjectsOutput {
            deleted: (!deleted.is_empty()).then_some(deleted),
            errors: (!errors.is_empty()).then_some(errors),
            ..Default::default()
        }))
    }

    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_copy_extensions(&req.input)?;
        let (source_bucket, source_key) = match &req.input.copy_source {
            CopySource::Bucket {
                bucket,
                key,
                version_id: None,
            } => (bucket.to_string(), key.to_string()),
            CopySource::Bucket {
                version_id: Some(_),
                ..
            } => {
                return Err(s3_error!(
                    NotImplemented,
                    "Copying an object version is unsupported"
                ));
            }
            CopySource::AccessPoint { .. } | CopySource::Outpost { .. } => {
                return Err(s3_error!(
                    NotImplemented,
                    "Copy access points are unsupported"
                ));
            }
        };
        let source_repository = self.repository(&req, &source_bucket, RepositoryAccess::Read)?;
        let source_object = self.read_object(source_repository, &source_key).await?;
        evaluate_conditions(
            req.input.copy_source_if_match.as_ref(),
            req.input.copy_source_if_none_match.as_ref(),
            req.input.copy_source_if_modified_since.as_ref(),
            req.input.copy_source_if_unmodified_since.as_ref(),
            &source_object.etag,
            &source_object.modified,
        )?;
        let source_size = source_object.size;
        if source_size > crate::content::MAX_PUT_OBJECT_BYTES {
            return Err(s3_error!(EntityTooLarge));
        }
        let spool = source_object
            .content
            .spool(
                source_repository,
                0..source_size,
                crate::content::MAX_PUT_OBJECT_BYTES,
            )
            .await?;
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        let mut attributes = if req
            .input
            .metadata_directive
            .as_ref()
            .is_some_and(|value| value.as_str() == MetadataDirective::REPLACE)
        {
            crate::attributes::PutAttributes {
                etag_override: None,
                completion_upload_id: None,
                logical_size: None,
                cache_control: req.input.cache_control,
                content_disposition: req.input.content_disposition,
                content_encoding: req.input.content_encoding,
                content_language: req.input.content_language,
                content_type: req.input.content_type,
                expires: req.input.expires,
                metadata: req.input.metadata.unwrap_or_default().into_iter().collect(),
            }
        } else {
            source_object
                .attributes
                .as_ref()
                .map(stored_to_pending)
                .unwrap_or_default()
        };
        attributes.etag_override = Some(crate::content::md5_hex(&spool.digests.md5));
        attributes.logical_size = Some(spool.size);
        let bytes = mutation_bytes(repository, &spool).await?;
        let outcome = mutation::apply(
            repository,
            Arc::clone(&self.runtime),
            self.options,
            address
                .branch
                .as_deref()
                .ok_or_else(|| s3_error!(MethodNotAllowed))?,
            &address.path,
            mutation::Change::Put {
                bytes,
                attributes: Box::new(attributes),
                condition: mutation::PutCondition::None,
            },
            &principal,
            &self.cancellation,
        )
        .await
        .map_err(mutation_error)?;
        Ok(S3Response::new(CopyObjectOutput {
            copy_object_result: Some(CopyObjectResult {
                e_tag: outcome.etag.map(ETag::Strong),
                last_modified: Some(timestamp(
                    i64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| s3_error!(InternalError))?
                            .as_secs(),
                    )
                    .map_err(|_| s3_error!(InternalError))?,
                )?),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_create_multipart_extensions(&req.input)?;
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        let branch = address
            .branch
            .as_deref()
            .ok_or_else(|| s3_error!(MethodNotAllowed))?;
        let path =
            std::str::from_utf8(address.path.as_bytes()).map_err(|_| s3_error!(InvalidArgument))?;
        let session = crate::multipart::create(
            repository,
            &req.input.bucket,
            &req.input.key,
            branch,
            path,
            &principal,
            crate::attributes::PutAttributes {
                etag_override: None,
                completion_upload_id: None,
                logical_size: None,
                cache_control: req.input.cache_control,
                content_disposition: req.input.content_disposition,
                content_encoding: req.input.content_encoding,
                content_language: req.input.content_language,
                content_type: req.input.content_type,
                expires: req.input.expires,
                metadata: req.input.metadata.unwrap_or_default().into_iter().collect(),
            },
            now_seconds()?,
        )
        .await
        .map_err(multipart_error)?;
        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(req.input.bucket),
            key: Some(req.input.key),
            upload_id: Some(session.id),
            ..Default::default()
        }))
    }

    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_upload_part_extensions(&req.input)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let loaded = crate::multipart::load(repository, &req.input.upload_id)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        let content_length = req.input.content_length;
        let content_md5 = req.input.content_md5.clone();
        let spool = crate::content::spool_body(
            req.input.body,
            content_length,
            crate::content::MAX_MULTIPART_PART_BYTES,
        )
        .await
        .map_err(content_error)?;
        verify_content_md5(&spool.digests.md5, content_md5.as_deref())?;
        let etag = crate::content::md5_hex(&spool.digests.md5);
        crate::multipart::register_part(
            repository,
            loaded,
            req.input.part_number,
            &spool,
            etag.clone(),
            now_seconds()?,
            &self.cancellation,
        )
        .await
        .map_err(multipart_error)?;
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(ETag::Strong(etag)),
            ..Default::default()
        }))
    }

    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        if req.input.copy_source_sse_customer_algorithm.is_some()
            || req.input.copy_source_sse_customer_key.is_some()
            || req.input.copy_source_sse_customer_key_md5.is_some()
            || req.input.expected_bucket_owner.is_some()
            || req.input.expected_source_bucket_owner.is_some()
            || req.input.request_payer.is_some()
            || req.input.sse_customer_algorithm.is_some()
            || req.input.sse_customer_key.is_some()
            || req.input.sse_customer_key_md5.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let (source_bucket, source_key) = copy_source_bucket_key(&req.input.copy_source)?;
        let source_repository = self.repository(&req, &source_bucket, RepositoryAccess::Read)?;
        let source = self.read_object(source_repository, &source_key).await?;
        evaluate_conditions(
            req.input.copy_source_if_match.as_ref(),
            req.input.copy_source_if_none_match.as_ref(),
            req.input.copy_source_if_modified_since.as_ref(),
            req.input.copy_source_if_unmodified_since.as_ref(),
            &source.etag,
            &source.modified,
        )?;
        let range = match req.input.copy_source_range.as_deref() {
            Some(value) => {
                let range = Range::parse(value).map_err(|_| s3_error!(InvalidArgument))?;
                let range = range.check(source.size)?;
                range.start..range.end
            }
            None => 0..source.size,
        };
        if range.end - range.start > crate::content::MAX_MULTIPART_PART_BYTES {
            return Err(s3_error!(EntityTooLarge));
        }
        let spool = source
            .content
            .spool(
                source_repository,
                range,
                crate::content::MAX_MULTIPART_PART_BYTES,
            )
            .await?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let loaded = crate::multipart::load(repository, &req.input.upload_id)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        let etag = crate::content::md5_hex(&spool.digests.md5);
        crate::multipart::register_part(
            repository,
            loaded,
            req.input.part_number,
            &spool,
            etag.clone(),
            now_seconds()?,
            &self.cancellation,
        )
        .await
        .map_err(multipart_error)?;
        Ok(S3Response::new(UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(ETag::Strong(etag)),
                last_modified: Some(timestamp(
                    i64::try_from(now_seconds()?).map_err(|_| s3_error!(InternalError))?,
                )?),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        reject_complete_multipart_extensions(&req.input)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let loaded = crate::multipart::load(repository, &req.input.upload_id)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        let selected = req
            .input
            .multipart_upload
            .and_then(|upload| upload.parts)
            .ok_or_else(|| s3_error!(InvalidPart))?
            .into_iter()
            .map(|part| {
                let number = part.part_number.ok_or_else(|| s3_error!(InvalidPart))?;
                let etag = match part.e_tag.ok_or_else(|| s3_error!(InvalidPart))? {
                    ETag::Strong(value) => value,
                    ETag::Weak(_) => return Err(s3_error!(InvalidPart)),
                };
                Ok((number, etag))
            })
            .collect::<S3Result<Vec<_>>>()?;
        if let Some(etag) =
            crate::multipart::completed_etag(&loaded.session, &selected).map_err(multipart_error)?
        {
            return Ok(S3Response::new(CompleteMultipartUploadOutput {
                bucket: Some(req.input.bucket),
                key: Some(req.input.key),
                e_tag: Some(ETag::Strong(etag.to_owned())),
                ..Default::default()
            }));
        }
        let (session, parts) = crate::multipart::freeze(
            repository,
            loaded,
            &selected,
            crate::content::MAX_MULTIPART_OBJECT_BYTES,
        )
        .await
        .map_err(multipart_error)?;
        let mut writer = crate::content::SpoolWriter::new()
            .await
            .map_err(content_error)?;
        for part in &parts {
            use md5::Digest as _;

            let mut stream = crate::multipart::part_stream(repository, part)
                .await
                .map_err(multipart_error)?;
            let mut digest = md5::Md5::new();
            let mut size = 0_u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|error| multipart_error(crate::multipart::Error::Storage(error)))?;
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| s3_error!(EntityTooLarge))?;
                digest.update(&chunk);
                writer
                    .write(&chunk, crate::content::MAX_MULTIPART_OBJECT_BYTES)
                    .await
                    .map_err(content_error)?;
            }
            let actual = digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if size != part.size || actual != part.etag {
                return Err(s3_error!(InvalidPart));
            }
        }
        let spool = writer.finish().await.map_err(content_error)?;
        let etag = multipart_etag(&parts)?;
        let mut attributes = session.attributes.clone();
        attributes.etag_override = Some(etag.clone());
        attributes.completion_upload_id = Some(session.id.clone());
        attributes.logical_size = Some(spool.size);
        let address = namespace::object_address(&session.key).map_err(namespace_error)?;
        let bytes = mutation_bytes(repository, &spool).await?;
        mutation::apply(
            repository,
            Arc::clone(&self.runtime),
            self.options,
            &session.branch,
            &address.path,
            mutation::Change::Put {
                bytes,
                attributes: Box::new(attributes),
                condition: mutation::PutCondition::None,
            },
            &principal,
            &self.cancellation,
        )
        .await
        .map_err(mutation_error)?;
        let loaded = crate::multipart::load(repository, &session.id)
            .await
            .map_err(multipart_error)?;
        crate::multipart::complete(repository, loaded, etag.clone())
            .await
            .map_err(multipart_error)?;
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(req.input.bucket),
            key: Some(req.input.key),
            e_tag: Some(ETag::Strong(etag)),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        if req.input.expected_bucket_owner.is_some()
            || req.input.if_match_initiated_time.is_some()
            || req.input.request_payer.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let loaded = crate::multipart::load(repository, &req.input.upload_id)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        crate::multipart::abort(repository, loaded)
            .await
            .map_err(multipart_error)?;
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }

    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        if req.input.expected_bucket_owner.is_some()
            || req.input.request_payer.is_some()
            || req.input.sse_customer_algorithm.is_some()
            || req.input.sse_customer_key.is_some()
            || req.input.sse_customer_key_md5.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?;
        let loaded = crate::multipart::load(repository, &req.input.upload_id)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            principal,
        )
        .map_err(multipart_error)?;
        let marker = req.input.part_number_marker.unwrap_or(0);
        let max = req.input.max_parts.unwrap_or(1000);
        if !(1..=1000).contains(&max) {
            return Err(s3_error!(InvalidArgument));
        }
        let mut parts = loaded
            .session
            .parts
            .values()
            .filter(|part| part.number > marker)
            .cloned()
            .collect::<Vec<_>>();
        let truncated = parts.len() > max as usize;
        parts.truncate(max as usize);
        let next = truncated
            .then(|| parts.last().map(|part| part.number))
            .flatten();
        Ok(S3Response::new(ListPartsOutput {
            bucket: Some(req.input.bucket),
            key: Some(req.input.key),
            upload_id: Some(req.input.upload_id),
            max_parts: Some(max),
            part_number_marker: Some(marker),
            next_part_number_marker: next,
            is_truncated: Some(truncated),
            parts: Some(
                parts
                    .into_iter()
                    .map(|part| {
                        Ok(Part {
                            e_tag: Some(ETag::Strong(part.etag)),
                            last_modified: Some(timestamp(
                                i64::try_from(part.modified_seconds)
                                    .map_err(|_| s3_error!(InternalError))?,
                            )?),
                            part_number: Some(part.number),
                            size: Some(
                                i64::try_from(part.size).map_err(|_| s3_error!(InternalError))?,
                            ),
                            ..Default::default()
                        })
                    })
                    .collect::<S3Result<Vec<_>>>()?,
            ),
            ..Default::default()
        }))
    }

    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        if req
            .input
            .delimiter
            .as_deref()
            .is_some_and(|value| value != "/")
            || req.input.expected_bucket_owner.is_some()
            || req.input.request_payer.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let url_encode = list_url_encoding(req.input.encoding_type.as_ref())?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?;
        let prefix = req.input.prefix.as_deref().unwrap_or("");
        let sessions = crate::multipart::list(repository)
            .await
            .map_err(multipart_error)?
            .into_iter()
            .filter(|session| session.principal == principal && session.key.starts_with(prefix))
            .collect::<Vec<_>>();
        let mut groups = std::collections::BTreeSet::new();
        let mut projected = Vec::new();
        for session in sessions {
            if req.input.delimiter.as_deref() == Some("/")
                && let Some(relative) = session.key.strip_prefix(prefix)
                && let Some(position) = relative.find('/')
            {
                groups.insert(format!("{}{}", prefix, &relative[..=position]));
                continue;
            }
            projected.push((session.key.clone(), session.id.clone(), Some(session), None));
        }
        projected.extend(groups.into_iter().map(|prefix| {
            (
                prefix.clone(),
                String::new(),
                None,
                Some(CommonPrefix {
                    prefix: Some(prefix),
                }),
            )
        }));
        projected.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        if let Some(key_marker) = req.input.key_marker.as_deref() {
            projected.retain(|item| match req.input.upload_id_marker.as_deref() {
                Some(upload_id_marker) if item.0 == key_marker => {
                    item.1.as_str() > upload_id_marker
                }
                _ => item.0.as_str() > key_marker,
            });
        }
        let max = req.input.max_uploads.unwrap_or(1000);
        if !(1..=1000).contains(&max) {
            return Err(s3_error!(InvalidArgument));
        }
        let truncated = projected.len() > max as usize;
        projected.truncate(max as usize);
        let (next_key, next_id) = if truncated {
            projected
                .last()
                .map(|item| {
                    (
                        Some(item.0.clone()),
                        (!item.1.is_empty()).then(|| item.1.clone()),
                    )
                })
                .unwrap_or_default()
        } else {
            (None, None)
        };
        let mut sessions = projected
            .iter_mut()
            .filter_map(|item| item.2.take())
            .collect::<Vec<_>>();
        let mut common_prefixes = projected
            .iter_mut()
            .filter_map(|item| item.3.take())
            .collect::<Vec<_>>();
        if url_encode {
            for session in &mut sessions {
                session.key = encode_list_value(&session.key, true);
            }
            for group in &mut common_prefixes {
                group.prefix = encode_list_option(group.prefix.take(), true);
            }
        }
        Ok(S3Response::new(ListMultipartUploadsOutput {
            bucket: Some(req.input.bucket),
            prefix: encode_list_option(req.input.prefix, url_encode),
            delimiter: encode_list_option(req.input.delimiter, url_encode),
            encoding_type: req.input.encoding_type,
            key_marker: encode_list_option(req.input.key_marker, url_encode),
            upload_id_marker: req.input.upload_id_marker,
            max_uploads: Some(max),
            is_truncated: Some(truncated),
            next_key_marker: encode_list_option(next_key, url_encode),
            next_upload_id_marker: next_id,
            common_prefixes: (!common_prefixes.is_empty()).then_some(common_prefixes),
            uploads: Some(
                sessions
                    .into_iter()
                    .map(|session| {
                        Ok(MultipartUpload {
                            key: Some(session.key),
                            upload_id: Some(session.id),
                            initiated: Some(timestamp(
                                i64::try_from(session.created_seconds)
                                    .map_err(|_| s3_error!(InternalError))?,
                            )?),
                            storage_class: Some(StorageClass::from_static("STANDARD")),
                            ..Default::default()
                        })
                    })
                    .collect::<S3Result<Vec<_>>>()?,
            ),
            ..Default::default()
        }))
    }

    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        let marker = req.input.marker.clone();
        let response = self.list_objects_v2(req.map_input(Into::into)).await?;
        Ok(response.map_output(|output| {
            let url_encode = output.encoding_type.is_some();
            ListObjectsOutput {
                name: output.name,
                prefix: output.prefix,
                marker: encode_list_option(marker, url_encode),
                max_keys: output.max_keys,
                is_truncated: output.is_truncated,
                contents: output.contents,
                common_prefixes: output.common_prefixes,
                delimiter: output.delimiter,
                next_marker: encode_list_option(output.next_continuation_token, url_encode),
                encoding_type: output.encoding_type,
                ..Default::default()
            }
        }))
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let _permit = self
            .admission
            .try_acquire()
            .map_err(|_| s3_error!(SlowDown))?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let url_encode = list_url_encoding(req.input.encoding_type.as_ref())?;
        if req
            .input
            .delimiter
            .as_deref()
            .is_some_and(|value| value != "/")
            || req.input.optional_object_attributes.is_some()
            || req.input.request_payer.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let prefix = req.input.prefix.as_deref().unwrap_or("");
        let Some((reference, _path_prefix)) =
            namespace::listing_reference(prefix).map_err(namespace_error)?
        else {
            return self.list_refs(&req, repository).await;
        };
        let repo = self.open(repository).await?;
        let operation = repo
            .operation(OperationKind::Repository, &self.cancellation)
            .await
            .map_err(remote_error)?;
        let result = async {
            let snapshot = repo
                .snapshot(
                    &Revision::parse(&reference).map_err(remote_error)?,
                    &operation,
                )
                .await
                .map_err(remote_error)?;
            let commit = snapshot.commit(&operation).await.map_err(remote_error)?;
            let commit_modified = timestamp(commit.committer.seconds)?;
            let attribute_manifest = crate::attributes::load(repository, snapshot.commit_oid())
                .await
                .map_err(gateway_error)?;
            let encoded_ref = prefix
                .split_once('/')
                .map(|(value, _)| value)
                .unwrap_or_default();
            let mut keys = Vec::new();
            for entry in snapshot
                .list_tree_recursive(&operation)
                .await
                .map_err(remote_error)?
            {
                if entry.kind != EntryKind::Blob {
                    continue;
                }
                let path = std::str::from_utf8(entry.path.as_bytes())
                    .map_err(|_| s3_error!(InvalidObjectState))?;
                let key = format!("{encoded_ref}/{path}");
                if !key.starts_with(prefix) {
                    continue;
                }
                let blob = snapshot
                    .read_blob(&entry.path, &operation)
                    .await
                    .map_err(remote_error)?;
                let attributes = attribute_manifest.object(path, entry.oid);
                let modified = match attributes {
                    Some(attributes) => timestamp(
                        i64::try_from(attributes.modified_seconds)
                            .map_err(|_| s3_error!(InternalError))?,
                    )?,
                    None => commit_modified.clone(),
                };
                let (content, logical_size) = classify_blob(blob)?;
                let etag = match attributes {
                    Some(attributes) => attributes.etag.clone(),
                    None => {
                        let spool = content.spool(repository, 0..logical_size, u64::MAX).await?;
                        crate::content::md5_hex(&spool.digests.md5)
                    }
                };
                keys.push((key, etag, Some(logical_size), modified));
            }
            keys.sort_by(|left, right| left.0.cmp(&right.0));
            let max_keys = req.input.max_keys.unwrap_or(1000);
            if max_keys < 0 {
                return Err(s3_error!(InvalidArgument));
            }
            let max_keys = max_keys.min(1000);
            let limit = usize::try_from(max_keys).map_err(|_| s3_error!(InvalidArgument))?;
            let delimiter = req.input.delimiter.as_deref();
            let mut values = Vec::new();
            let mut groups = std::collections::BTreeSet::new();
            for (key, etag, size, modified) in keys {
                if let Some(delimiter) = delimiter
                    && let Some(relative) = key.strip_prefix(prefix)
                    && let Some(position) = relative.find(delimiter)
                {
                    groups.insert(format!("{}{}", prefix, &relative[..=position]));
                    continue;
                }
                values.push((key, etag, size, modified));
            }
            let mut projected = values
                .into_iter()
                .map(|(key, etag, size, modified)| {
                    (
                        key.clone(),
                        Some(Object {
                            key: Some(key),
                            e_tag: Some(ETag::Strong(etag)),
                            last_modified: Some(modified),
                            size: size.and_then(|size| i64::try_from(size).ok()),
                            storage_class: Some(ObjectStorageClass::from_static("STANDARD")),
                            ..Default::default()
                        }),
                        None,
                    )
                })
                .chain(groups.into_iter().map(|prefix| {
                    (
                        prefix.clone(),
                        None,
                        Some(CommonPrefix {
                            prefix: Some(prefix),
                        }),
                    )
                }))
                .collect::<Vec<_>>();
            projected.sort_by(|left, right| left.0.cmp(&right.0));
            let after = req
                .input
                .continuation_token
                .as_deref()
                .or(req.input.start_after.as_deref());
            if let Some(after) = after {
                projected.retain(|(key, _, _)| key.as_str() > after);
            }
            let truncated = limit != 0 && projected.len() > limit;
            projected.truncate(limit);
            let next = truncated
                .then(|| projected.last().map(|item| item.0.clone()))
                .flatten();
            let contents = projected
                .iter_mut()
                .filter_map(|item| item.1.take())
                .collect::<Vec<_>>();
            let common_prefixes = projected
                .iter_mut()
                .filter_map(|item| item.2.take())
                .collect::<Vec<_>>();
            let mut contents = contents;
            let mut common_prefixes = common_prefixes;
            if url_encode {
                for object in &mut contents {
                    object.key = encode_list_option(object.key.take(), true);
                }
                for group in &mut common_prefixes {
                    group.prefix = encode_list_option(group.prefix.take(), true);
                }
            }
            Ok(S3Response::new(ListObjectsV2Output {
                name: Some(req.input.bucket.clone()),
                prefix: encode_list_option(req.input.prefix.clone(), url_encode),
                max_keys: Some(max_keys),
                key_count: Some(
                    i32::try_from(projected.len()).map_err(|_| s3_error!(InternalError))?,
                ),
                continuation_token: req.input.continuation_token.clone(),
                next_continuation_token: next,
                is_truncated: Some(truncated),
                contents: (!contents.is_empty()).then_some(contents),
                common_prefixes: (!common_prefixes.is_empty()).then_some(common_prefixes),
                delimiter: encode_list_option(req.input.delimiter.clone(), url_encode),
                encoding_type: req.input.encoding_type.clone(),
                start_after: encode_list_option(req.input.start_after.clone(), url_encode),
                ..Default::default()
            }))
        }
        .await;
        finish(operation, result).await
    }
}

fn verify_content_md5(actual: &[u8; 16], expected: Option<&str>) -> S3Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected)
        .map_err(|_| s3_error!(InvalidDigest))?;
    if expected.as_slice() != actual {
        return Err(s3_error!(BadDigest));
    }
    Ok(())
}

fn reject_put_extensions(input: &PutObjectInput) -> S3Result<()> {
    if input.acl.is_some()
        || input.bucket_key_enabled.is_some()
        || input.expected_bucket_owner.is_some()
        || input.grant_full_control.is_some()
        || input.grant_read.is_some()
        || input.grant_read_acp.is_some()
        || input.grant_write_acp.is_some()
        || input.if_match.is_some()
        || input.object_lock_legal_hold_status.is_some()
        || input.object_lock_mode.is_some()
        || input.object_lock_retain_until_date.is_some()
        || input.request_payer.is_some()
        || input.server_side_encryption.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
        || input.ssekms_encryption_context.is_some()
        || input.ssekms_key_id.is_some()
        || input.storage_class.is_some()
        || input.tagging.is_some()
        || input.website_redirect_location.is_some()
        || input.write_offset_bytes.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn put_condition(input: &PutObjectInput) -> S3Result<mutation::PutCondition> {
    match input.if_none_match.as_ref() {
        None => Ok(mutation::PutCondition::None),
        Some(ETagCondition::Any) => Ok(mutation::PutCondition::IfNoneMatchAny),
        Some(ETagCondition::ETag(_)) => Err(s3_error!(NotImplemented)),
    }
}

#[derive(Clone)]
struct RequestChecksums {
    algorithm: Option<ChecksumAlgorithm>,
    crc32: Option<String>,
    crc32c: Option<String>,
    crc64nvme: Option<String>,
    sha1: Option<String>,
    sha256: Option<String>,
}

impl From<&PutObjectInput> for RequestChecksums {
    fn from(input: &PutObjectInput) -> Self {
        Self {
            algorithm: input.checksum_algorithm.clone(),
            crc32: input.checksum_crc32.clone(),
            crc32c: input.checksum_crc32c.clone(),
            crc64nvme: input.checksum_crc64nvme.clone(),
            sha1: input.checksum_sha1.clone(),
            sha256: input.checksum_sha256.clone(),
        }
    }
}

impl RequestChecksums {
    fn merge_trailers(&mut self, trailers: Option<&s3s::TrailingHeaders>) -> S3Result<()> {
        let Some(headers) = trailers.and_then(s3s::TrailingHeaders::take) else {
            return Ok(());
        };
        merge_checksum_header(&mut self.crc32, &headers, "x-amz-checksum-crc32")?;
        merge_checksum_header(&mut self.crc32c, &headers, "x-amz-checksum-crc32c")?;
        merge_checksum_header(&mut self.crc64nvme, &headers, "x-amz-checksum-crc64nvme")?;
        merge_checksum_header(&mut self.sha1, &headers, "x-amz-checksum-sha1")?;
        merge_checksum_header(&mut self.sha256, &headers, "x-amz-checksum-sha256")?;
        Ok(())
    }

    fn verify(&self, digests: &crate::content::Digests) -> S3Result<()> {
        verify_base64_checksum(self.crc32.as_deref(), &digests.crc32.to_be_bytes())?;
        verify_base64_checksum(self.crc32c.as_deref(), &digests.crc32c.to_be_bytes())?;
        verify_base64_checksum(self.crc64nvme.as_deref(), &digests.crc64nvme.to_be_bytes())?;
        verify_base64_checksum(self.sha1.as_deref(), &digests.sha1)?;
        verify_base64_checksum(self.sha256.as_deref(), &digests.sha256)?;
        if let Some(algorithm) = &self.algorithm {
            let supplied = match algorithm.as_str() {
                ChecksumAlgorithm::CRC32 => self.crc32.is_some(),
                ChecksumAlgorithm::CRC32C => self.crc32c.is_some(),
                ChecksumAlgorithm::CRC64NVME => self.crc64nvme.is_some(),
                ChecksumAlgorithm::SHA1 => self.sha1.is_some(),
                ChecksumAlgorithm::SHA256 => self.sha256.is_some(),
                _ => return Err(s3_error!(InvalidRequest, "Unsupported checksum algorithm")),
            };
            if !supplied {
                return Err(s3_error!(InvalidRequest, "Checksum value is missing"));
            }
        }
        Ok(())
    }
}

fn merge_checksum_header(
    current: &mut Option<String>,
    headers: &http::HeaderMap,
    name: &'static str,
) -> S3Result<()> {
    let Some(value) = headers.get(name) else {
        return Ok(());
    };
    let value = value
        .to_str()
        .map_err(|_| s3_error!(InvalidDigest))?
        .to_owned();
    if current.as_ref().is_some_and(|current| current != &value) {
        return Err(s3_error!(BadDigest));
    }
    *current = Some(value);
    Ok(())
}

fn verify_base64_checksum(expected: Option<&str>, actual: &[u8]) -> S3Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected)
        .map_err(|_| s3_error!(InvalidDigest))?;
    if expected.as_slice() != actual {
        return Err(s3_error!(BadDigest));
    }
    Ok(())
}

fn reject_delete_extensions(input: &DeleteObjectInput) -> S3Result<()> {
    if input.bypass_governance_retention.is_some()
        || input.expected_bucket_owner.is_some()
        || input.if_match.is_some()
        || input.if_match_last_modified_time.is_some()
        || input.if_match_size.is_some()
        || input.mfa.is_some()
        || input.request_payer.is_some()
        || input.version_id.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_copy_extensions(input: &CopyObjectInput) -> S3Result<()> {
    let directive_supported = input.metadata_directive.as_ref().is_none_or(|value| {
        matches!(
            value.as_str(),
            MetadataDirective::COPY | MetadataDirective::REPLACE
        )
    });
    if !directive_supported
        || input.acl.is_some()
        || input.bucket_key_enabled.is_some()
        || input.checksum_algorithm.is_some()
        || input.copy_source_sse_customer_algorithm.is_some()
        || input.copy_source_sse_customer_key.is_some()
        || input.copy_source_sse_customer_key_md5.is_some()
        || input.expected_bucket_owner.is_some()
        || input.expected_source_bucket_owner.is_some()
        || input.grant_full_control.is_some()
        || input.grant_read.is_some()
        || input.grant_read_acp.is_some()
        || input.grant_write_acp.is_some()
        || input.object_lock_legal_hold_status.is_some()
        || input.object_lock_mode.is_some()
        || input.object_lock_retain_until_date.is_some()
        || input.request_payer.is_some()
        || input.server_side_encryption.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
        || input.ssekms_encryption_context.is_some()
        || input.ssekms_key_id.is_some()
        || input.storage_class.is_some()
        || input.tagging.is_some()
        || input.tagging_directive.is_some()
        || input.website_redirect_location.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_create_multipart_extensions(input: &CreateMultipartUploadInput) -> S3Result<()> {
    if input.acl.is_some()
        || input.bucket_key_enabled.is_some()
        || input.checksum_algorithm.is_some()
        || input.checksum_type.is_some()
        || input.expected_bucket_owner.is_some()
        || input.grant_full_control.is_some()
        || input.grant_read.is_some()
        || input.grant_read_acp.is_some()
        || input.grant_write_acp.is_some()
        || input.object_lock_legal_hold_status.is_some()
        || input.object_lock_mode.is_some()
        || input.object_lock_retain_until_date.is_some()
        || input.request_payer.is_some()
        || input.server_side_encryption.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
        || input.ssekms_encryption_context.is_some()
        || input.ssekms_key_id.is_some()
        || input.storage_class.is_some()
        || input.tagging.is_some()
        || input.website_redirect_location.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_upload_part_extensions(input: &UploadPartInput) -> S3Result<()> {
    if input.checksum_algorithm.is_some()
        || input.checksum_crc32.is_some()
        || input.checksum_crc32c.is_some()
        || input.checksum_crc64nvme.is_some()
        || input.checksum_sha1.is_some()
        || input.checksum_sha256.is_some()
        || input.expected_bucket_owner.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_complete_multipart_extensions(input: &CompleteMultipartUploadInput) -> S3Result<()> {
    if input.checksum_crc32.is_some()
        || input.checksum_crc32c.is_some()
        || input.checksum_crc64nvme.is_some()
        || input.checksum_sha1.is_some()
        || input.checksum_sha256.is_some()
        || input.checksum_type.is_some()
        || input.expected_bucket_owner.is_some()
        || input.if_match.is_some()
        || input.if_none_match.is_some()
        || input.mpu_object_size.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn writable_branch<'a>(
    repository: &Repository,
    address: &'a namespace::ObjectAddress,
) -> S3Result<&'a str> {
    let branch = address
        .branch
        .as_deref()
        .ok_or_else(|| s3_error!(MethodNotAllowed, "Writes require a branch key"))?;
    let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    if repository
        .config
        .protected_branches
        .iter()
        .any(|protected| protected == short)
    {
        return Err(s3_error!(
            AccessDenied,
            "The destination branch is protected"
        ));
    }
    Ok(branch)
}

fn copy_source_bucket_key(source: &CopySource) -> S3Result<(String, String)> {
    match source {
        CopySource::Bucket {
            bucket,
            key,
            version_id: None,
        } => Ok((bucket.to_string(), key.to_string())),
        CopySource::Bucket {
            version_id: Some(_),
            ..
        } => Err(s3_error!(
            NotImplemented,
            "Copying an object version is unsupported"
        )),
        CopySource::AccessPoint { .. } | CopySource::Outpost { .. } => Err(s3_error!(
            NotImplemented,
            "Copy access points are unsupported"
        )),
    }
}

fn stored_to_pending(
    value: &crate::attributes::ObjectAttributes,
) -> crate::attributes::PutAttributes {
    crate::attributes::PutAttributes {
        etag_override: None,
        completion_upload_id: None,
        logical_size: Some(value.size),
        cache_control: value.cache_control.clone(),
        content_disposition: value.content_disposition.clone(),
        content_encoding: value.content_encoding.clone(),
        content_language: value.content_language.clone(),
        content_type: value.content_type.clone(),
        expires: value.expires.clone(),
        metadata: value.metadata.clone(),
    }
}

impl Gateway {
    async fn list_refs(
        &self,
        req: &S3Request<ListObjectsV2Input>,
        repository: &Repository,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let url_encode = list_url_encoding(req.input.encoding_type.as_ref())?;
        let repo = self.open(repository).await?;
        let mut keys = repo
            .refs()
            .entries
            .iter()
            .filter_map(|reference| reference.name.strip_prefix("refs/heads/"))
            .map(|branch| {
                format!(
                    "{}/",
                    percent_encoding::utf8_percent_encode(
                        branch,
                        percent_encoding::NON_ALPHANUMERIC
                    )
                )
            })
            .collect::<Vec<_>>();
        keys.sort();
        let after = req
            .input
            .continuation_token
            .as_deref()
            .or(req.input.start_after.as_deref());
        if let Some(after) = after {
            keys.retain(|key| key.as_str() > after);
        }
        let max_keys = req.input.max_keys.unwrap_or(1000);
        if max_keys < 0 {
            return Err(s3_error!(InvalidArgument));
        }
        let max_keys = max_keys.min(1000);
        let limit = usize::try_from(max_keys).map_err(|_| s3_error!(InvalidArgument))?;
        let truncated = limit != 0 && keys.len() > limit;
        keys.truncate(limit);
        let next = truncated.then(|| keys.last().cloned()).flatten();
        let count = i32::try_from(keys.len()).map_err(|_| s3_error!(InternalError))?;
        Ok(S3Response::new(ListObjectsV2Output {
            name: Some(req.input.bucket.clone()),
            prefix: encode_list_option(req.input.prefix.clone(), url_encode),
            max_keys: Some(max_keys),
            key_count: Some(count),
            continuation_token: req.input.continuation_token.clone(),
            next_continuation_token: next,
            is_truncated: Some(truncated),
            common_prefixes: Some(
                keys.into_iter()
                    .map(|prefix| CommonPrefix {
                        prefix: Some(encode_list_value(&prefix, url_encode)),
                    })
                    .collect(),
            ),
            delimiter: encode_list_option(req.input.delimiter.clone(), url_encode),
            encoding_type: req.input.encoding_type.clone(),
            start_after: encode_list_option(req.input.start_after.clone(), url_encode),
            ..Default::default()
        }))
    }
}

fn reject_get_extensions(input: &GetObjectInput) -> S3Result<()> {
    if input.version_id.is_some()
        || input.part_number.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_head_extensions(input: &HeadObjectInput) -> S3Result<()> {
    if input.version_id.is_some()
        || input.part_number.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn evaluate_conditions(
    if_match: Option<&ETagCondition>,
    if_none_match: Option<&ETagCondition>,
    if_modified_since: Option<&Timestamp>,
    if_unmodified_since: Option<&Timestamp>,
    etag: &str,
    modified: &Timestamp,
) -> S3Result<()> {
    let actual = ETag::Strong(etag.to_owned());
    if let Some(condition) = if_match {
        let matches = match condition {
            ETagCondition::Any => true,
            ETagCondition::ETag(expected) => actual.strong_cmp(expected),
        };
        if !matches {
            return Err(s3_error!(PreconditionFailed));
        }
    } else if if_unmodified_since.is_some_and(|expected| modified > expected) {
        return Err(s3_error!(PreconditionFailed));
    }
    if let Some(condition) = if_none_match {
        let matches = match condition {
            ETagCondition::Any => true,
            ETagCondition::ETag(expected) => actual.weak_cmp(expected),
        };
        if matches {
            return Err(s3_error!(NotModified));
        }
    } else if if_modified_since.is_some_and(|expected| modified <= expected) {
        return Err(s3_error!(NotModified));
    }
    Ok(())
}

async fn finish<T>(
    operation: crab_remote_git::OperationContext,
    result: S3Result<T>,
) -> S3Result<T> {
    match operation.finish(Ok(())).await {
        Ok(()) => result,
        Err(error) => Err(remote_error(error)),
    }
}

fn timestamp(seconds: i64) -> S3Result<Timestamp> {
    let seconds = u64::try_from(seconds).map_err(|_| s3_error!(InternalError))?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .map(Timestamp::from)
        .ok_or_else(|| s3_error!(InternalError))
}

fn list_url_encoding(encoding: Option<&EncodingType>) -> S3Result<bool> {
    match encoding {
        None => Ok(false),
        Some(value) if value.as_str() == EncodingType::URL => Ok(true),
        Some(_) => Err(s3_error!(InvalidArgument, "Unsupported list encoding type")),
    }
}

fn encode_list_option(value: Option<String>, enabled: bool) -> Option<String> {
    value.map(|value| encode_list_value(&value, enabled))
}

fn encode_list_value(value: &str, enabled: bool) -> String {
    if enabled {
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
    } else {
        value.to_owned()
    }
}

fn namespace_error(error: namespace::NamespaceError) -> s3s::S3Error {
    match error {
        namespace::NamespaceError::MissingObject => s3_error!(NoSuchKey),
        _ => s3_error!(InvalidArgument),
    }
}

fn remote_error(error: crab_remote_git::Error) -> s3s::S3Error {
    match error {
        crab_remote_git::Error::PathNotFound => s3_error!(NoSuchKey),
        crab_remote_git::Error::Revision { .. } => s3_error!(NoSuchKey),
        crab_remote_git::Error::Cancelled => s3_error!(RequestTimeout),
        crab_remote_git::Error::LimitExceeded { .. } => s3_error!(SlowDown),
        error => {
            tracing::error!(error = ?error, "S3 repository read failed");
            s3_error!(InternalError)
        }
    }
}

fn mutation_error(error: mutation::Error) -> s3s::S3Error {
    match error {
        mutation::Error::NotDirectory | mutation::Error::IsDirectory => {
            s3_error!(InvalidObjectState)
        }
        mutation::Error::Cancelled => s3_error!(RequestTimeout),
        mutation::Error::PreconditionFailed => s3_error!(PreconditionFailed),
        mutation::Error::Write(crab_write::WriteError::RefChanged { .. }) => {
            s3_error!(
                OperationAborted,
                "A conflicting branch write won; retry the request"
            )
        }
        mutation::Error::Coordination(crab_coordination::CoordinationError::PushLockHeld {
            ..
        }) => s3_error!(
            OperationAborted,
            "The destination branch is busy; retry the request"
        ),
        error => {
            tracing::error!(error = ?error, "S3 repository mutation failed");
            s3_error!(InternalError)
        }
    }
}

fn gateway_error(error: crate::Error) -> s3s::S3Error {
    tracing::error!(error = ?error, "S3 gateway persistence failed");
    s3_error!(InternalError)
}

fn content_error(error: crate::content::Error) -> s3s::S3Error {
    match error {
        crate::content::Error::TooLarge => s3_error!(EntityTooLarge),
        crate::content::Error::Incomplete | crate::content::Error::Body(_) => {
            tracing::warn!(%error, "S3 request body failed");
            s3_error!(IncompleteBody)
        }
        crate::content::Error::Io(_) => {
            tracing::error!(error = ?error, "S3 content spool failed");
            s3_error!(InternalError)
        }
    }
}

pub(crate) fn md5_hex(bytes: &[u8]) -> String {
    use md5::Digest as _;

    md5::Md5::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn multipart_etag(parts: &[crate::multipart::Part]) -> S3Result<String> {
    use md5::Digest as _;

    let mut binary = Vec::with_capacity(parts.len().saturating_mul(16));
    for part in parts {
        if part.etag.len() != 32 {
            return Err(s3_error!(InvalidPart));
        }
        for index in (0..part.etag.len()).step_by(2) {
            binary.push(
                u8::from_str_radix(&part.etag[index..index + 2], 16)
                    .map_err(|_| s3_error!(InvalidPart))?,
            );
        }
    }
    let digest = md5::Md5::digest(binary);
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{hash}-{}", parts.len()))
}

fn now_seconds() -> S3Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| s3_error!(InternalError))
}

fn multipart_error(error: crate::multipart::Error) -> s3s::S3Error {
    match error {
        crate::multipart::Error::NoSuchUpload
        | crate::multipart::Error::NotOpen
        | crate::multipart::Error::Identity => s3_error!(NoSuchUpload),
        crate::multipart::Error::PartNumber | crate::multipart::Error::InvalidPart => {
            s3_error!(InvalidPart)
        }
        crate::multipart::Error::InvalidPartOrder => s3_error!(InvalidPartOrder),
        crate::multipart::Error::EntityTooSmall => s3_error!(EntityTooSmall),
        crate::multipart::Error::EntityTooLarge => s3_error!(EntityTooLarge),
        crate::multipart::Error::Conflict => s3_error!(OperationAborted),
        error => {
            tracing::error!(error = ?error, "S3 multipart state failed");
            s3_error!(InternalError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encoded_list_values_round_trip_reserved_and_unicode_bytes() {
        let value = "main/special name(1)-雪.txt";
        let encoded = encode_list_value(value, true);
        assert_eq!(
            percent_encoding::percent_decode_str(&encoded)
                .decode_utf8()
                .unwrap(),
            value
        );
        assert!(encoded.contains("%2F"));
        assert!(encoded.contains("%28"));
    }

    #[tokio::test]
    async fn content_above_inline_threshold_is_stored_as_streamable_lfs() {
        use futures_util::TryStreamExt as _;

        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let repository = Repository::new(
            RepositoryConfig {
                name: "repo".to_owned(),
                provider: StorageProviderKind::Local,
                bucket: "memory".to_owned(),
                prefix: "large-content-test".to_owned(),
                default_branch: "main".to_owned(),
                members: Vec::new(),
                protected_branches: Vec::new(),
            },
            store,
        )
        .unwrap();
        let content = b"content larger than the test inline limit";
        let mut writer = crate::content::SpoolWriter::new().await.unwrap();
        writer.write(content, u64::MAX).await.unwrap();
        let spool = writer.finish().await.unwrap();

        let pointer_bytes = mutation_bytes_with_inline_limit(&repository, &spool, 8)
            .await
            .unwrap();
        let PointerKind::Lfs(pointer) = crab_git::classify(&pointer_bytes) else {
            panic!("expected an LFS pointer");
        };
        let (_, _, stream) = repository
            .lfs
            .get_stream(&pointer.oid, pointer.size, None)
            .await
            .unwrap();
        let actual = stream
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .unwrap();

        assert_eq!(actual, content);
    }

    #[tokio::test]
    async fn put_checksums_accept_all_supported_algorithms_and_reject_mismatch() {
        use sha1::Digest as _;

        let body = b"123456789";
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let checksums = RequestChecksums {
            algorithm: Some(ChecksumAlgorithm::from_static(ChecksumAlgorithm::CRC32)),
            crc32: Some(encode(
                &u32::try_from(crc_fast::checksum(
                    crc_fast::CrcAlgorithm::Crc32IsoHdlc,
                    body,
                ))
                .unwrap()
                .to_be_bytes(),
            )),
            crc32c: Some(encode(
                &u32::try_from(crc_fast::checksum(crc_fast::CrcAlgorithm::Crc32Iscsi, body))
                    .unwrap()
                    .to_be_bytes(),
            )),
            crc64nvme: Some(encode(
                &crc_fast::checksum(crc_fast::CrcAlgorithm::Crc64Nvme, body).to_be_bytes(),
            )),
            sha1: Some(encode(&sha1::Sha1::digest(body))),
            sha256: Some(encode(&sha2::Sha256::digest(body))),
        };
        let mut writer = crate::content::SpoolWriter::new().await.unwrap();
        writer.write(body, u64::MAX).await.unwrap();
        let spool = writer.finish().await.unwrap();
        checksums.verify(&spool.digests).unwrap();

        let mut invalid = checksums;
        invalid.sha256 = Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned());
        let error = invalid.verify(&spool.digests).unwrap_err();
        assert_eq!(error.code().as_str(), "BadDigest");
    }
}
