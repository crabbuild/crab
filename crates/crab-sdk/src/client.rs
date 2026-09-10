use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "write")]
pub(crate) mod mutation;

use bytes::Bytes;
use crab_remote_git::{
    NoopMetrics, OperationKind, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity,
    RepositoryOptions, RuntimeOptions,
};
use crab_storage::{Store, StoreLayout};

use crate::remote_error::{consumer_error, remote_error};
use crate::runtime::Operations;
use crate::{
    Blame, ClientBuilder, Commit, Diff, Error, ErrorKind, GitPath, HistoryTraversal, ObjectId,
    OperationOptions, Page, PageRequest, RepositoryLocator, Result, Revision, TreeEntry,
};

impl ClientBuilder {
    /// Build the selected store without initializing or modifying a repository.
    pub fn build(self) -> Result<Client> {
        let direct_store_configured = self.store.is_some();
        #[cfg(feature = "local")]
        let local_store = self.store.clone();
        let (store, namespace) = match self.store {
            Some(options) => options.build()?,
            #[cfg(feature = "managed")]
            None if self.managed.is_some() => (
                Store::new(Arc::new(object_store::memory::InMemory::new())),
                "managed-only".to_owned(),
            ),
            None => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "direct storage or managed service options are required",
                ));
            }
        };
        #[cfg(feature = "managed")]
        let managed = self
            .managed
            .map(crate::managed_impl::ManagedState::new)
            .transpose()?;
        let git = RemoteGitRuntime::new(RuntimeOptions::default(), Arc::new(NoopMetrics))
            .map_err(remote_error)?;
        Ok(Client(Arc::new(ClientState {
            store,
            namespace,
            #[cfg(feature = "content")]
            cache: self.cache,
            operations: Operations::default(),
            git: Arc::new(git),
            direct_store_configured,
            #[cfg(feature = "local")]
            local_store,
            #[cfg(feature = "local")]
            local_tools: self.local_tools,
            #[cfg(feature = "local")]
            local_execution_policy: self.local_execution_policy,
            #[cfg(feature = "managed")]
            managed,
        })))
    }
}

pub(crate) struct ClientState {
    #[cfg(feature = "content")]
    cache: Option<crate::ContentCache>,
    pub(crate) store: Store,
    pub(crate) direct_store_configured: bool,
    namespace: String,
    pub(crate) operations: Operations,
    git: Arc<RemoteGitRuntime>,
    #[cfg(feature = "local")]
    pub(crate) local_store: Option<crate::DirectStoreOptions>,
    #[cfg(feature = "local")]
    pub(crate) local_tools: Option<crate::LocalTools>,
    #[cfg(feature = "local")]
    pub(crate) local_execution_policy: crate::LocalExecutionPolicy,
    #[cfg(feature = "managed")]
    pub(crate) managed: Option<crate::managed_impl::ManagedState>,
}

pub(crate) struct ResolvedRepository {
    pub(crate) store: Store,
    pub(crate) prefix: String,
    pub(crate) namespace: String,
    pub(crate) placement: u64,
    pub(crate) managed: bool,
}

impl ClientState {
    pub(crate) async fn resolve_repository(
        &self,
        locator: &crate::RepositoryLocator,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ResolvedRepository> {
        #[cfg(feature = "managed")]
        if let Some(repository) = locator.managed_owner() {
            let managed = self.managed.as_ref().ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "managed service options are required",
                )
            })?;
            let resolved = managed
                .resolver
                .resolve(
                    &repository,
                    crab_auth::managed::TransferOperation::Fetch,
                    _cancel,
                )
                .await
                .map_err(crate::managed_impl::managed_repository_error)?;
            let identity = resolved.store.target_identity().ok_or_else(|| {
                Error::new(
                    ErrorKind::Corruption,
                    "managed transfer grant has no placement identity",
                )
            })?;
            let placement = u64::from_le_bytes(identity[..8].try_into().map_err(|source| {
                Error::with_source(
                    ErrorKind::Corruption,
                    "managed placement identity is invalid",
                    source,
                )
            })?);
            return Ok(ResolvedRepository {
                store: resolved.store,
                prefix: resolved.repository_prefix,
                namespace: format!(
                    "managed:{}:{}",
                    managed.cache_scope,
                    repository.canonical_url()
                ),
                placement,
                managed: true,
            });
        }
        if !self.direct_store_configured {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "direct storage is required for remote repository access",
            ));
        }
        Ok(ResolvedRepository {
            store: self.store.clone(),
            prefix: locator.direct_prefix()?.to_owned(),
            namespace: self.namespace.clone(),
            placement: 1,
            managed: false,
        })
    }
}

#[cfg(all(test, feature = "local"))]
pub(crate) fn memory_local_client(local_tools: crate::LocalTools) -> Client {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_target_identity([43; 32])
        .with_bucket_identity(crab_storage::BucketIdentity::new(
            crab_storage::StorageProviderKind::S3,
            "memory-local-test",
            "bucket",
        ));
    Client(Arc::new(ClientState {
        store,
        namespace: "memory-local-test".to_owned(),
        #[cfg(feature = "content")]
        cache: None,
        operations: Operations::default(),
        git: Arc::new(RemoteGitRuntime::default()),
        direct_store_configured: true,
        local_store: None,
        local_tools: Some(local_tools),
        local_execution_policy: crate::LocalExecutionPolicy::Untrusted,
        #[cfg(feature = "managed")]
        managed: None,
    }))
}

#[cfg(all(test, feature = "local", feature = "content"))]
pub(crate) fn memory_local_client_with_cache(
    local_tools: crate::LocalTools,
    cache: crate::ContentCache,
) -> Client {
    let mut client = memory_local_client(local_tools);
    if let Some(state) = Arc::get_mut(&mut client.0) {
        state.cache = Some(cache);
    }
    client
}

#[cfg(feature = "local")]
impl ClientState {
    pub(crate) fn local_store(&self) -> Result<&crate::DirectStoreOptions> {
        self.local_store.as_ref().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "direct storage is required for Crab local transport",
            )
        })
    }
}

/// Shared repository client whose operations must be drained before runtime shutdown.
#[derive(Clone)]
pub struct Client(pub(crate) Arc<ClientState>);

impl Client {
    /// Start explicit client configuration.
    #[must_use]
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    pub(crate) async fn open_remote_with_options(
        &self,
        locator: RepositoryLocator,
        options: OperationOptions,
    ) -> Result<RemoteRepository> {
        let state = self.0.clone();
        let worker_locator = locator.clone();
        let (owner, resolved) = self
            .0
            .operations
            .run(options.clone(), move |cancel| async move {
                let resolved = state.resolve_repository(&worker_locator, &cancel).await?;
                let layout = StoreLayout::new(resolved.store.clone(), resolved.prefix.clone());
                let identity = RepositoryIdentity::new(
                    resolved.namespace.clone(),
                    resolved.prefix.clone(),
                    resolved.placement,
                )
                .map_err(remote_error)?;
                let owner = RemoteGitRepository::open(
                    resolved.store.clone(),
                    layout,
                    identity,
                    state.git.clone(),
                    RepositoryOptions::new(Default::default(), options.owner_limits())
                        .map_err(remote_error)?,
                    &cancel,
                )
                .await
                .map_err(remote_error)?;
                Ok((owner, resolved))
            })
            .await?;
        #[cfg(feature = "content")]
        let content = Arc::new(crate::content::crab::ContentRuntime::new(
            self.0.cache.clone(),
            StoreLayout::new(resolved.store.clone(), resolved.prefix.clone()),
            &resolved.namespace,
            owner.shard_index_hash().to_owned(),
            owner.generation(),
        ));
        Ok(RemoteRepository {
            client: self.clone(),
            locator,
            owner: Arc::new(owner),
            managed: resolved.managed,
            #[cfg(feature = "content")]
            content,
        })
    }

    /// Stop admission, cancel and drain SDK workers, then drain owner cleanup.
    ///
    /// Returns the first unobserved worker failure since the previous close.
    /// Ordinary drop cancellation is ignored unless cleanup also failed.
    pub async fn close(&self) -> Result<()> {
        self.0.operations.drain().await;
        self.0.git.shutdown().await;
        // Consume the diagnostic only after every asynchronous cleanup boundary,
        // so dropping this close future leaves it available to the next close.
        self.0.operations.take_failure()
    }
}

/// Options applied independently to one read.
#[derive(Clone, Debug, Default)]
pub struct ReadOptions {
    pub(crate) operation: OperationOptions,
    range: Option<(u64, u64)>,
}

impl ReadOptions {
    fn limits(&self) -> Result<crab_remote_git::OperationLimits> {
        if self.range.is_some() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "byte ranges apply only to file reads",
            ));
        }
        Ok(self.content_limits())
    }

    pub(crate) fn content_limits(&self) -> crab_remote_git::OperationLimits {
        self.operation.owner_limits()
    }

    /// Apply cancellation, deadline and limits to this read.
    #[must_use]
    pub fn with_operation(mut self, operation: OperationOptions) -> Self {
        self.operation = operation;
        self
    }

    /// Set nonzero aggregate limits for this operation, including cache-hit work.
    pub fn with_limits(mut self, limits: crate::ReadLimits) -> Result<Self> {
        self.operation = self.operation.with_limits(limits)?;
        Ok(self)
    }

    /// Return the selected aggregate limits.
    #[must_use]
    pub fn read_limits(&self) -> crate::ReadLimits {
        self.operation.read_limits()
    }

    /// Select an exact half-open range for raw blob or logical file reads.
    ///
    /// Reversed bounds are rejected immediately; bounds past the file size fail
    /// when the file is resolved. Empty ranges at or before EOF are valid.
    pub fn with_range(mut self, range: std::ops::Range<u64>) -> Result<Self> {
        if range.start > range.end {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "range start exceeds its end",
            ));
        }
        self.range = Some((range.start, range.end));
        Ok(self)
    }

    pub(crate) fn byte_range(&self, size: u64) -> Result<std::ops::Range<u64>> {
        let (start, end) = self.range.unwrap_or((0, size));
        if end > size {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "range exceeds the file size",
            ));
        }
        Ok(start..end)
    }

    /// Set a nonzero operation duration; owner defaults apply when omitted.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.operation = self.operation.with_timeout(timeout)?;
        Ok(self)
    }
}

/// Immutable repository generation; refresh returns a separate handle.
#[derive(Clone)]
pub struct RemoteRepository {
    #[cfg(feature = "content")]
    content: Arc<crate::content::crab::ContentRuntime>,
    client: Client,
    locator: RepositoryLocator,
    owner: Arc<RemoteGitRepository>,
    managed: bool,
}

/// An operation family implemented for this repository handle.
///
/// Capability discovery describes available mechanisms, not authorization or
/// backend qualification. Individual operations still validate their inputs
/// and can fail because credentials or repository data are unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RepositoryCapability {
    /// Read pinned Git objects, trees, history, diffs, blame and raw archives.
    ReadGit,
    /// Reconstruct Crab and LFS content, including ranges and hydrated archives.
    ReadContent,
    /// Prepare and execute atomic ref batches with durable direct reconciliation.
    UpdateRefs,
    /// Create commits from streamed Git or hydrated file edits.
    CreateCommits,
}

/// One reference from a pinned repository generation.
#[derive(Clone, Debug)]
pub struct Reference {
    name: String,
    target: ObjectId,
    peeled: Option<ObjectId>,
}

impl Reference {
    /// Return the full reference name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the object directly named by the reference.
    #[must_use]
    pub fn target(&self) -> ObjectId {
        self.target
    }

    /// Return the recorded peeled commit of an annotated tag, when present.
    #[must_use]
    pub fn peeled(&self) -> Option<ObjectId> {
        self.peeled
    }
}

/// Complete sorted refs and symbolic HEAD from one repository generation.
#[derive(Clone, Debug)]
pub struct References {
    head: Option<String>,
    entries: Vec<Reference>,
}

impl References {
    /// Return the symbolic HEAD name, including an unborn branch.
    #[must_use]
    pub fn head(&self) -> Option<&str> {
        self.head.as_deref()
    }

    /// Return references in bytewise full-name order.
    #[must_use]
    pub fn entries(&self) -> &[Reference] {
        &self.entries
    }
}

impl RemoteRepository {
    /// Inspect implemented operation families without I/O or refreshing state.
    ///
    /// This metadata remains available after client close. ReadContent requires
    /// the `content` feature. UpdateRefs and CreateCommits require `write` and a
    /// conditional-write provider or managed protected-push service; the
    /// filesystem backend cannot publish refs.
    #[must_use]
    pub fn capabilities(&self) -> &[RepositoryCapability] {
        if self.managed {
            return &[
                RepositoryCapability::ReadGit,
                #[cfg(feature = "content")]
                RepositoryCapability::ReadContent,
                #[cfg(feature = "write")]
                RepositoryCapability::UpdateRefs,
                #[cfg(feature = "write")]
                RepositoryCapability::CreateCommits,
            ];
        }
        #[cfg(feature = "write")]
        if self.client.0.store.bucket_identity().cloud != crab_storage::StorageProviderKind::Local {
            return &[
                RepositoryCapability::ReadGit,
                #[cfg(feature = "content")]
                RepositoryCapability::ReadContent,
                RepositoryCapability::UpdateRefs,
                RepositoryCapability::CreateCommits,
            ];
        }
        &[
            RepositoryCapability::ReadGit,
            #[cfg(feature = "content")]
            RepositoryCapability::ReadContent,
        ]
    }

    /// Return this handle's pinned refs without refreshing their generation.
    pub fn refs(&self) -> crate::Request<'_, References, OperationOptions> {
        crate::Request::new(move |options| Box::pin(self.refs_with_options(options)))
    }

    async fn refs_with_options(&self, options: OperationOptions) -> Result<References> {
        let owner = self.owner.clone();
        self.client
            .0
            .operations
            .run(options.clone(), move |_| async move {
                let refs = owner.refs();
                let limits = options.read_limits();
                if refs.entries.len() as u64 > limits.max_entries {
                    return Err(Error::new(
                        ErrorKind::LimitExceeded,
                        "reference entry limit exceeded",
                    ));
                }
                let head = refs
                    .head
                    .as_ref()
                    .map(|head| head.name.clone())
                    .or_else(|| refs.unborn_head.clone());
                let mut remaining = limits.max_response_bytes;
                // Charge returned names and binary object IDs before cloning the
                // reference vector; cached refs still consume response capacity.
                for bytes in
                    head.iter()
                        .map(|name| name.len() as u64)
                        .chain(refs.entries.iter().map(|entry| {
                            entry.name.len() as u64
                                + 20
                                + if entry.peeled.is_some() { 20 } else { 0 }
                        }))
                {
                    remaining = remaining.checked_sub(bytes).ok_or_else(|| {
                        Error::new(
                            ErrorKind::LimitExceeded,
                            "reference response limit exceeded",
                        )
                    })?;
                }
                let entries = refs
                    .entries
                    .iter()
                    .map(|entry| {
                        Ok(Reference {
                            name: entry.name.clone(),
                            target: ObjectId::from_owner(entry.target)?,
                            peeled: entry.peeled.map(ObjectId::from_owner).transpose()?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Ok(References { head, entries })
            })
            .await
    }

    /// Capture the current generation without changing this handle or its snapshots.
    pub fn refresh(&self) -> crate::Request<'_, Self, OperationOptions> {
        crate::Request::new(move |options| Box::pin(self.refresh_with_options(options)))
    }

    async fn refresh_with_options(&self, options: OperationOptions) -> Result<Self> {
        self.client
            .open_remote_with_options(self.locator.clone(), options)
            .await
    }

    /// Resolve a reachable commit and finish the read session before returning it.
    pub fn snapshot(&self, revision: Revision) -> crate::Request<'_, Snapshot, OperationOptions> {
        crate::Request::new(move |options| Box::pin(self.snapshot_with_options(revision, options)))
    }

    async fn snapshot_with_options(
        &self,
        revision: Revision,
        options: OperationOptions,
    ) -> Result<Snapshot> {
        let owner = self.owner.clone();
        let snapshot = self
            .client
            .0
            .operations
            .run(options.clone(), move |cancel| async move {
                let operation = owner
                    .operation_with_limits(OperationKind::Snapshot, &cancel, options.owner_limits())
                    .await
                    .map_err(remote_error)?;
                let result = owner.snapshot(&revision.into_owner(), &operation).await;
                operation.finish(result).await.map_err(remote_error)
            })
            .await?;
        Ok(Snapshot {
            repository: self.clone(),
            owner: snapshot,
        })
    }
}

/// A commit snapshot whose reads share the pinned repository generation.
#[derive(Clone)]
pub struct Snapshot {
    repository: RemoteRepository,
    owner: crab_remote_git::RemoteGitSnapshot,
}

impl Snapshot {
    /// Return the verified commit identity captured by this snapshot.
    pub fn commit_id(&self) -> Result<ObjectId> {
        ObjectId::from_owner(self.owner.commit_oid())
    }

    /// Read verified commit metadata with exact identity and message bytes.
    pub fn commit(&self) -> crate::Request<'_, Commit, ReadOptions> {
        crate::Request::new(move |options| Box::pin(self.commit_with_options(options)))
    }

    async fn commit_with_options(&self, options: ReadOptions) -> Result<Commit> {
        let repository = self.repository.owner.clone();
        let snapshot = self.owner.clone();
        self.repository
            .client
            .0
            .operations
            .run(options.operation.clone(), move |cancel| async move {
                let operation = repository
                    .operation_with_limits(OperationKind::Commit, &cancel, options.limits()?)
                    .await
                    .map_err(remote_error)?;
                let result = snapshot.commit(&operation).await;
                let value = operation.finish(result).await.map_err(remote_error)?;
                Commit::from_owner(value)
            })
            .await
    }

    /// List one bounded page of immediate children, preserving Git modes and byte paths.
    pub fn tree(
        &self,
        path: GitPath,
        page: PageRequest,
    ) -> crate::Request<'_, Page<TreeEntry>, ReadOptions> {
        crate::Request::new(move |options| Box::pin(self.tree_with_options(path, page, options)))
    }

    async fn tree_with_options(
        &self,
        path: GitPath,
        page: PageRequest,
        options: ReadOptions,
    ) -> Result<Page<TreeEntry>> {
        let repository = self.repository.owner.clone();
        let snapshot = self.owner.clone();
        let identity = repository.identity().clone();
        let generation = snapshot.generation();
        let commit = self.commit_id()?;
        let page = page.into_owner(&identity, generation, commit)?;
        self.repository
            .client
            .0
            .operations
            .run(options.operation.clone(), move |cancel| async move {
                let path = crab_remote_git::GitPath::new(path.as_bytes().to_vec())
                    .map_err(remote_error)?;
                let operation = repository
                    .operation_with_limits(OperationKind::Tree, &cancel, options.limits()?)
                    .await
                    .map_err(remote_error)?;
                let result = snapshot.list_directory(&path, &page, &operation).await;
                let value = operation.finish(result).await.map_err(remote_error)?;
                Page::from_owner(value, identity, generation, commit, TreeEntry::from_owner)
            })
            .await
    }

    /// Traverse a bounded commit-history page under an explicit parent policy.
    pub fn history(
        &self,
        traversal: HistoryTraversal,
        page: PageRequest,
    ) -> crate::Request<'_, Page<Commit>, ReadOptions> {
        crate::Request::new(move |options| {
            Box::pin(self.history_with_options(traversal, page, options))
        })
    }

    async fn history_with_options(
        &self,
        traversal: HistoryTraversal,
        page: PageRequest,
        options: ReadOptions,
    ) -> Result<Page<Commit>> {
        let repository = self.repository.owner.clone();
        let snapshot = self.owner.clone();
        let identity = repository.identity().clone();
        let generation = snapshot.generation();
        let commit = self.commit_id()?;
        let page = page.into_owner(&identity, generation, commit)?;
        let traversal = match traversal {
            HistoryTraversal::AllParents => crab_remote_git::HistoryTraversal::AllParents,
            HistoryTraversal::FirstParent => crab_remote_git::HistoryTraversal::FirstParent,
        };
        self.repository
            .client
            .0
            .operations
            .run(options.operation.clone(), move |cancel| async move {
                let operation = repository
                    .operation_with_limits(OperationKind::History, &cancel, options.limits()?)
                    .await
                    .map_err(remote_error)?;
                let result = snapshot.history(traversal, &page, &operation).await;
                let value = operation.finish(result).await.map_err(remote_error)?;
                Page::from_owner(value, identity, generation, commit, Commit::from_owner)
            })
            .await
    }

    /// Open logical file content; pointer targets require hydration support.
    pub fn open_file(
        &self,
        path: GitPath,
    ) -> crate::Request<'_, crate::ContentStream, ReadOptions> {
        crate::Request::new(move |options| Box::pin(self.open_file_with_options(path, options)))
    }

    async fn open_file_with_options(
        &self,
        path: GitPath,
        options: ReadOptions,
    ) -> Result<crate::ContentStream> {
        crate::content::open(
            &self.repository.client.0.operations,
            self.repository.owner.clone(),
            self.owner.clone(),
            path,
            options,
            #[cfg(feature = "content")]
            self.repository.content.clone(),
        )
        .await
    }

    /// Stream verified archive entries in the explicitly selected representation.
    pub fn archive(
        &self,
        mode: crate::ContentMode,
    ) -> crate::Request<'_, crate::ArchiveStream, ReadOptions> {
        crate::Request::new(move |options| Box::pin(self.archive_with_options(mode, options)))
    }

    async fn archive_with_options(
        &self,
        mode: crate::ContentMode,
        options: ReadOptions,
    ) -> Result<crate::ArchiveStream> {
        if mode == crate::ContentMode::Hydrated && !cfg!(feature = "content") {
            return Err(Error::new(
                ErrorKind::UnsupportedCapability,
                "hydrated archives require content support",
            ));
        }
        crate::archive::open(
            &self.repository.client.0.operations,
            self.repository.owner.clone(),
            self.owner.clone(),
            options.clone(),
            options.limits()?,
            mode,
            #[cfg(feature = "content")]
            self.repository.content.clone(),
        )
        .await
    }

    /// Diff this snapshot against a reachable base revision at one exact path.
    ///
    /// Text results use bounded complete replacement hunks, without rename detection.
    pub fn diff(&self, base: Revision, path: GitPath) -> crate::Request<'_, Diff, ReadOptions> {
        crate::Request::new(move |options| Box::pin(self.diff_with_options(base, path, options)))
    }

    async fn diff_with_options(
        &self,
        base: Revision,
        path: GitPath,
        options: ReadOptions,
    ) -> Result<Diff> {
        let repository = self.repository.owner.clone();
        let snapshot = self.owner.clone();
        let value = self
            .repository
            .client
            .0
            .operations
            .run(options.operation.clone(), move |cancel| async move {
                let path = crab_remote_git::GitPath::new(path.as_bytes().to_vec())
                    .map_err(remote_error)?;
                let operation = repository
                    .operation_with_limits(OperationKind::Diff, &cancel, options.limits()?)
                    .await
                    .map_err(remote_error)?;
                let result = async {
                    let base = repository.snapshot(&base.into_owner(), &operation).await?;
                    snapshot.diff(&base, &path, &operation).await
                }
                .await;
                operation.finish(result).await.map_err(remote_error)
            })
            .await?;
        Ok(Diff::from_owner(value))
    }

    /// Attribute all lines of one bounded text blob without hydrating pointer targets.
    pub fn blame(&self, path: GitPath) -> crate::Request<'_, Blame, ReadOptions> {
        crate::Request::new(move |options| Box::pin(self.blame_with_options(path, options)))
    }

    async fn blame_with_options(&self, path: GitPath, options: ReadOptions) -> Result<Blame> {
        let repository = self.repository.owner.clone();
        let snapshot = self.owner.clone();
        self.repository
            .client
            .0
            .operations
            .run(options.operation.clone(), move |cancel| async move {
                let path = crab_remote_git::GitPath::new(path.as_bytes().to_vec())
                    .map_err(remote_error)?;
                let operation = repository
                    .operation_with_limits(OperationKind::Blame, &cancel, options.limits()?)
                    .await
                    .map_err(remote_error)?;
                let result = snapshot.blame(&path, &operation).await;
                let value = operation.finish(result).await.map_err(remote_error)?;
                Blame::from_owner(value)
            })
            .await
    }

    /// Read exact Git bytes, including pointer text, within owner limits.
    pub fn read_blob(&self, path: GitPath) -> crate::Request<'_, Bytes, ReadOptions> {
        crate::Request::new(move |options| Box::pin(self.read_blob_with_options(path, options)))
    }

    async fn read_blob_with_options(&self, path: GitPath, options: ReadOptions) -> Result<Bytes> {
        let repository = self.repository.owner.clone();
        let snapshot = self.owner.clone();
        self.repository
            .client
            .0
            .operations
            .run(options.operation.clone(), move |cancel| async move {
                let limits = options.content_limits();
                let path = crab_remote_git::GitPath::new(path.as_bytes().to_vec())
                    .map_err(remote_error)?;
                let operation = repository
                    .operation_with_limits(OperationKind::Content, &cancel, limits)
                    .await
                    .map_err(remote_error)?;
                let result = async {
                    let blob = snapshot.read_blob(&path, &operation).await?;
                    let range = options
                        .byte_range(blob.bytes.len() as u64)
                        .map_err(consumer_error)?;
                    Ok(blob.bytes.slice(range.start as usize..range.end as usize))
                }
                .await;
                operation.finish(result).await.map_err(remote_error)
            })
            .await
    }
}
