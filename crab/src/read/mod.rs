//! Crab-owned read primitives for rev-pinned repository snapshots.
//!
//! This module powers selective materialization commands.

pub mod selection;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix_hash::ObjectId;
use gix_object::{Find, FindExt, tree::EntryKind as GixEntryKind};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::cache::{LocalCache, default_cache_root};
use crate::cmd::hydrate::HydrationRuntime;
use crate::core::config::Config;
use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::url::{Cloud, CrabUrl, ObjectUrl, UrlForm};
use crate::metadata::manifest::{Manifest, PackManifestEntry};
use crate::storage::{StoreLayout, resolve_object_url_store};
use crab_cache_store::CachingStore;
use crab_git::lfs_pointer::LfsPointer;
use crab_lfs::LfsObjectStore;
use crab_types::pointer::Pointer;

const DEFAULT_REV: &str = "HEAD";
const POINTER_PEEK_THRESHOLD: usize = 4096;
const MAX_BLOB_PEEK: usize = 1024;
const WRITE_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Build the canonical CLI hydration runtime for a selected read store.
///
/// Cache initialization is an optimization: failure leaves the verified
/// origin-backed hydrator available and emits a structured diagnostic.
pub fn build_cli_hydrator(
    caching_store: CachingStore,
    router: StoreLayout,
    config: &Config,
) -> Result<HydrationRuntime> {
    HydrationRuntime::with_config_from_cli_layout(caching_store, router, config)
}

/// Resolve the configured remote and build the product hydration adapter.
pub async fn build_configured_cli_hydrator(
    config: &Config,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<Option<HydrationRuntime>> {
    let Some(url) = config.remote_url.as_deref() else {
        return Ok(None);
    };
    if url.trim().is_empty() {
        return Err(CrabError::Configuration {
            key: "remote.url".into(),
            origin: "crab.toml contains an empty [remote].url".into(),
        });
    }
    let parsed = CrabUrl::parse(url)?;
    let selection =
        crate::replication::select_read_store(config, &parsed, operation, cancel).await?;
    let caching_store = CachingStore::new(selection.store, &config.cache)?;
    build_cli_hydrator(caching_store, selection.router, config).map(Some)
}

/// Build the canonical shared-crate hydration runtime for VFS/server callers.
///
/// Product configuration stays in this adapter while reconstruction remains
/// owned by `crab-read`.
pub fn build_shared_hydrator(
    caching_store: CachingStore,
    router: StoreLayout,
    config: &Config,
) -> Result<crab_read::ShardHydrator> {
    let read_layout = crab_read::ReadStoreLayout::with_global_prefix(
        caching_store.origin().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    Ok(crab_read::ReadRuntimeBuilder::new(
        caching_store,
        read_layout,
        config.hydrate.download_concurrency,
    )
    .with_buffer_budget(config.hydrate.prefetch_budget)
    .build()?)
}

/// Options used when opening a repository for reads.
#[derive(Debug, Clone)]
pub struct RepositoryOpenOptions {
    /// Cache root override. When absent, uses Crab's default cache root.
    pub cache_dir: Option<PathBuf>,
    /// Resolved Crab configuration.
    pub config: Config,
    /// Cancellation token honored during store selection and remote reads.
    pub cancel: CancellationToken,
}

/// Read handle for a local or remote Crab repository.
#[derive(Clone)]
pub struct RepositoryReader {
    inner: Arc<Inner>,
}

struct Inner {
    url: String,
    config: Config,
    cache: Arc<LocalCache>,
    git_dir: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    cancel: CancellationToken,
    remote: OnceCell<Arc<RemoteContext>>,
    remote_git_dir: OnceCell<Arc<PathBuf>>,
}

struct RemoteContext {
    caching_store: CachingStore,
    router: StoreLayout,
    hydrator: Arc<HydrationRuntime>,
}

/// A repository snapshot pinned to a resolved commit.
#[derive(Clone)]
pub struct SnapshotReader {
    repo: RepositoryReader,
    requested_revision: String,
    resolved_revision: String,
    git_dir: PathBuf,
}

/// Materialization strategy for an entry selected from a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DownloadEntryKind {
    /// Plain git blob stored directly in the tree.
    Git,
    /// Crab pointer blob reconstructed from shards/xorbs.
    CrabPointer,
    /// Git LFS pointer blob fetched from Crab's LFS object layout.
    LfsPointer,
}

/// Metadata for a materializable file at a resolved revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct DownloadEntry {
    /// Repo-relative forward-slash path.
    pub path: String,
    /// Logical file size after materialization.
    pub size: u64,
    /// Backing blob kind.
    pub kind: DownloadEntryKind,
    /// Pointer content hash when the entry is Crab or LFS-backed.
    pub content_hash: Option<String>,
}

/// A set of entries selected for download from a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct DownloadPlan {
    /// Selected entries, deduplicated by repo-relative path.
    pub entries: Vec<DownloadEntry>,
}

impl RepositoryReader {
    /// Open a local path or supported Crab object URL for read access.
    pub async fn open(repo: &str, options: RepositoryOpenOptions) -> Result<Self> {
        if repo.contains("://") {
            let object_url = ObjectUrl::parse(repo)?;
            return match object_url.cloud {
                Cloud::Local => Self::finish_local_url(repo, object_url, options).await,
                _ => Self::finish_remote(repo, object_url, options),
            };
        }

        Self::open_local_path(Path::new(repo), options).await
    }

    /// Original repository string after local-path normalization.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// Cache root used by this reader.
    #[must_use]
    pub fn cache_dir(&self) -> &Path {
        self.inner.cache.root()
    }

    /// Stable destination root for implicit `crab download` cache writes.
    #[must_use]
    pub fn download_cache_root(&self, resolved_revision: &str) -> PathBuf {
        self.cache_dir()
            .join("downloads")
            .join(cache_key_for(self.url()))
            .join(resolved_revision)
    }

    /// Pin the repository to a resolved revision.
    pub async fn snapshot(&self, revision: Option<&str>) -> Result<SnapshotReader> {
        check_cancelled(&self.inner.cancel)?;
        let requested = revision.unwrap_or(DEFAULT_REV).to_owned();
        let (resolved_revision, git_dir) = match self.inner.git_dir.as_ref() {
            Some(git_dir) => {
                let git_dir_for_task = git_dir.clone();
                let requested_for_task = requested.clone();
                let resolved = tokio::task::spawn_blocking(move || {
                    crab_git::ref_resolve::resolve_ref_at(&git_dir_for_task, &requested_for_task)
                })
                .await
                .map_err(|join_err| {
                    CrabError::Internal(format!("resolve revision task failed: {join_err}"))
                })??;
                (resolved, git_dir.clone())
            }
            None => self.inner.remote_snapshot_git_dir(&requested).await?,
        };

        Ok(SnapshotReader {
            repo: self.clone(),
            requested_revision: requested,
            resolved_revision,
            git_dir,
        })
    }

    async fn open_local_path(path: &Path, options: RepositoryOpenOptions) -> Result<Self> {
        let absolute = tokio::fs::canonicalize(path).await?;
        let url = local_path_to_file_url(&absolute);
        let object_url = ObjectUrl::parse(&url)?;
        Self::finish_local_url(&url, object_url, options).await
    }

    async fn finish_local_url(
        url: &str,
        object_url: ObjectUrl,
        options: RepositoryOpenOptions,
    ) -> Result<Self> {
        let start = PathBuf::from(&object_url.prefix);
        let cache_root = options.cache_dir.unwrap_or_else(default_cache_root);
        let repo_path = match gix_discover::upwards(&start) {
            Ok((repo_path, _trust)) => repo_path,
            Err(_err) if start.exists() => {
                return Ok(Self {
                    inner: Arc::new(Inner {
                        url: url.to_owned(),
                        config: options.config,
                        cache: Arc::new(LocalCache::new(cache_root)),
                        git_dir: None,
                        work_dir: None,
                        cancel: options.cancel,
                        remote: OnceCell::new(),
                        remote_git_dir: OnceCell::new(),
                    }),
                });
            }
            Err(err) => {
                return Err(CrabError::Configuration {
                    key: format!("git discovery failed at {}: {err}", start.display()),
                    origin: start.display().to_string(),
                });
            }
        };
        let (git_dir, work_dir) = repo_path.into_repository_and_work_tree_directories();

        Ok(Self {
            inner: Arc::new(Inner {
                url: url.to_owned(),
                config: options.config,
                cache: Arc::new(LocalCache::new(cache_root)),
                git_dir: Some(git_dir),
                work_dir,
                cancel: options.cancel,
                remote: OnceCell::new(),
                remote_git_dir: OnceCell::new(),
            }),
        })
    }

    fn finish_remote(
        url: &str,
        _object_url: ObjectUrl,
        options: RepositoryOpenOptions,
    ) -> Result<Self> {
        let cache_root = options.cache_dir.unwrap_or_else(default_cache_root);
        Ok(Self {
            inner: Arc::new(Inner {
                url: url.to_owned(),
                config: options.config,
                cache: Arc::new(LocalCache::new(cache_root)),
                git_dir: None,
                work_dir: None,
                cancel: options.cancel,
                remote: OnceCell::new(),
                remote_git_dir: OnceCell::new(),
            }),
        })
    }
}

impl SnapshotReader {
    /// Revision string requested by the caller.
    #[must_use]
    pub fn requested_revision(&self) -> &str {
        &self.requested_revision
    }

    /// Resolved 40-character commit SHA.
    #[must_use]
    pub fn resolved_revision(&self) -> &str {
        &self.resolved_revision
    }

    /// Return metadata for one exact file path.
    pub async fn entry_for_path(&self, path: &str) -> Result<DownloadEntry> {
        let git_dir = self.git_dir.clone();
        let commit = self.resolved_revision.clone();
        let path_owned = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let blob = read_blob_at_commit(&git_dir, &commit, &path_owned)?;
            classify_blob_bytes(&path_owned, &blob)
        })
        .await
        .map_err(|join_err| CrabError::Internal(format!("entry stat task failed: {join_err}")))?
    }

    /// List all materializable file entries in the snapshot.
    pub async fn list_entries(&self) -> Result<Vec<DownloadEntry>> {
        let git_dir = self.git_dir.clone();
        let commit = self.resolved_revision.clone();
        tokio::task::spawn_blocking(move || list_entries_blocking(&git_dir, &commit))
            .await
            .map_err(|join_err| CrabError::Internal(format!("tree walk task failed: {join_err}")))?
    }

    /// Materialize one repo-relative file into `dest`.
    pub async fn download_to_path(&self, path: &str, dest: &Path) -> Result<u64> {
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let git_dir = self.git_dir.clone();
        let commit = self.resolved_revision.clone();
        let path_owned = path.to_owned();
        let blob_bytes = tokio::task::spawn_blocking(move || {
            read_blob_at_commit(&git_dir, &commit, &path_owned)
        })
        .await
        .map_err(|join_err| CrabError::Internal(format!("blob-load task failed: {join_err}")))??;

        if let Ok(ptr) = Pointer::parse(&blob_bytes) {
            let remote = self.repo.inner.remote().await?;
            return remote.hydrator.reconstruct_to_path(&ptr, dest).await;
        }

        if !blob_bytes.is_empty()
            && let Ok(lfs_ptr) = LfsPointer::parse(&blob_bytes)
        {
            let remote = self.repo.inner.remote().await?;
            let lfs_store = LfsObjectStore::new(
                remote.caching_store.origin().clone(),
                remote.router.repo_prefix(),
            );
            let content = lfs_store.get(&lfs_ptr.oid).await?;
            verify_lfs_content(path, &lfs_ptr, &content)?;
            tokio::fs::write(dest, &content).await?;
            return Ok(content.len() as u64);
        }

        tokio::fs::write(dest, &blob_bytes).await?;
        Ok(blob_bytes.len() as u64)
    }

    /// Materialize one repo-relative file into a blocking writer.
    pub async fn write_to_writer<W>(&self, path: &str, writer: W) -> Result<u64>
    where
        W: std::io::Write + Send + 'static,
    {
        let git_dir = self.git_dir.clone();
        let commit = self.resolved_revision.clone();
        let path_owned = path.to_owned();
        let blob_bytes = tokio::task::spawn_blocking(move || {
            read_blob_at_commit(&git_dir, &commit, &path_owned)
        })
        .await
        .map_err(|join_err| CrabError::Internal(format!("blob-load task failed: {join_err}")))??;

        if let Ok(ptr) = Pointer::parse(&blob_bytes) {
            let remote = self.repo.inner.remote().await?;
            return remote.hydrator.reconstruct_to_writer(&ptr, writer).await;
        }

        if !blob_bytes.is_empty()
            && let Ok(lfs_ptr) = LfsPointer::parse(&blob_bytes)
        {
            let remote = self.repo.inner.remote().await?;
            let lfs_store = LfsObjectStore::new(
                remote.caching_store.origin().clone(),
                remote.router.repo_prefix(),
            );
            let object_path = lfs_store.object_path_for(&lfs_ptr.oid);
            let mut verifier = Sha256Tap::new(writer);
            let bytes = remote
                .caching_store
                .origin()
                .stream_to_writer(&object_path, &mut verifier)
                .await?;
            verifier.verify(path, &lfs_ptr, bytes)?;
            return Ok(bytes);
        }

        write_blob_to_writer(writer, &blob_bytes)
    }
}

impl Inner {
    async fn remote(&self) -> Result<Arc<RemoteContext>> {
        let ctx = self
            .remote
            .get_or_try_init(|| async {
                check_cancelled(&self.cancel)?;
                let remote_url = match self.work_dir.as_deref() {
                    Some(work_dir) => read_workspace_remote(work_dir, &self.config)?,
                    None => self.url.clone(),
                };
                let object_url = ObjectUrl::parse(&remote_url)?;
                let (store, router) = if object_url.form == UrlForm::Crab {
                    let parsed = CrabUrl::parse(&remote_url)?;
                    let selection = crate::replication::select_read_store(
                        &self.config,
                        &parsed,
                        "download",
                        &self.cancel,
                    )
                    .await?;
                    (selection.store, selection.router)
                } else {
                    let resolved = resolve_object_url_store(
                        &object_url,
                        &self.config,
                        "download",
                        &self.cancel,
                    )
                    .await?;
                    let router = StoreLayout::new(resolved.store.clone(), resolved.prefix);
                    (resolved.store, router)
                };

                let caching_store = CachingStore::new_with_local_cache(
                    store,
                    &self.config.cache,
                    Arc::clone(&self.cache),
                )?;
                let hydrator =
                    build_cli_hydrator(caching_store.clone(), router.clone(), &self.config)?;

                Ok::<_, CrabError>(Arc::new(RemoteContext {
                    caching_store,
                    router,
                    hydrator: Arc::new(hydrator),
                }))
            })
            .await?;
        Ok(Arc::clone(ctx))
    }

    async fn remote_snapshot_git_dir(&self, rev: &str) -> Result<(String, PathBuf)> {
        let snapshot = self.read_remote_snapshot().await?;
        let manifest = snapshot.materialized_manifest();
        let resolved = resolve_manifest_rev(&manifest, rev).ok_or_else(|| CrabError::NotFound {
            path: format!("revision:{rev}"),
        })?;
        let git_dir = self
            .remote_git_dir_for_packs(&snapshot.journal.packs)
            .await?;
        Ok((resolved, git_dir))
    }

    async fn remote_git_dir_for_packs(&self, packs: &[PackManifestEntry]) -> Result<PathBuf> {
        let git_dir = self
            .remote_git_dir
            .get_or_try_init(|| async {
                let git_dir = self.cache.root().join("git");
                let pack_dir = git_dir.join("objects").join("pack");
                tokio::fs::create_dir_all(&pack_dir).await?;
                Ok::<_, CrabError>(Arc::new(git_dir))
            })
            .await?;

        let remote = self.remote().await?;
        if !packs.is_empty() {
            let pack_dir = git_dir.join("objects").join("pack");
            install_remote_git_packs(&remote, &pack_dir, packs).await?;
        }

        Ok((**git_dir).clone())
    }

    async fn read_remote_snapshot(&self) -> Result<crate::metadata::manifest::RepositorySnapshot> {
        let remote = self.remote().await?;
        let origin = crate::storage::Store::from_storage(remote.caching_store.origin().clone());
        crate::metadata::manifest::read_repository_snapshot(&origin, &remote.router).await
    }
}

async fn install_remote_git_packs(
    remote: &RemoteContext,
    pack_dir: &Path,
    packs: &[PackManifestEntry],
) -> Result<()> {
    for pack in packs {
        let final_pack = pack_dir.join(format!("pack-{}.pack", pack.pack_id));
        let final_idx = pack_dir.join(format!("pack-{}.idx", pack.pack_id));
        if final_pack.exists() && final_idx.exists() {
            continue;
        }

        let tmp_pack = tempfile::Builder::new()
            .prefix(".crab-download-pack-")
            .suffix(".pack")
            .tempfile_in(pack_dir)?
            .into_temp_path();
        let tmp_pack_path = tmp_pack.to_path_buf();
        let remote_path = remote.router.pack_path(&pack.pack_id);
        remote
            .caching_store
            .origin()
            .download_to_path_bounded(&remote_path, &tmp_pack_path, pack.size)
            .await?;

        let install_result = crate::git::pack::install_pack_file_locally(
            pack_dir,
            &tmp_pack_path,
            &pack.pack_id,
            0,
            true,
        )
        .await;
        let _ = tokio::fs::remove_file(&tmp_pack_path).await;
        drop(tmp_pack);
        install_result?;
    }

    Ok(())
}

fn read_blob_at_commit(git_dir: &Path, commit_sha: &str, path: &str) -> Result<Vec<u8>> {
    if path.is_empty() {
        return Err(CrabError::NotFound {
            path: path.to_owned(),
        });
    }

    let odb = open_odb(git_dir)?;
    let commit_oid = ObjectId::from_hex(commit_sha.as_bytes())
        .map_err(|e| CrabError::Internal(format!("invalid commit sha {commit_sha:?}: {e}")))?;
    let tree_id = {
        let mut buf = Vec::new();
        let mut iter = odb
            .find_commit_iter(&commit_oid, &mut buf)
            .map_err(|e| CrabError::Internal(format!("failed to read commit {commit_sha}: {e}")))?;
        iter.tree_id().map_err(|e| {
            CrabError::Internal(format!(
                "failed to parse tree id from commit {commit_sha}: {e}"
            ))
        })?
    };

    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    if components.is_empty() {
        return Err(CrabError::NotFound {
            path: path.to_owned(),
        });
    }

    let mut current_tree = tree_id;
    for (index, component) in components.iter().enumerate() {
        let is_last = index == components.len() - 1;
        let mut buf = Vec::new();
        let tree_iter = odb
            .find_tree_iter(&current_tree, &mut buf)
            .map_err(|e| CrabError::Internal(format!("failed to read tree {current_tree}: {e}")))?;

        let mut matched: Option<(ObjectId, GixEntryKind)> = None;
        for entry_result in tree_iter {
            let entry = entry_result
                .map_err(|e| CrabError::Internal(format!("corrupt tree {current_tree}: {e}")))?;
            if AsRef::<[u8]>::as_ref(entry.filename) == component.as_bytes() {
                matched = Some((entry.oid.to_owned(), entry.mode.kind()));
                break;
            }
        }

        let Some((oid, kind)) = matched else {
            return Err(CrabError::NotFound {
                path: path.to_owned(),
            });
        };

        if is_last {
            if !matches!(kind, GixEntryKind::Blob | GixEntryKind::BlobExecutable) {
                return Err(CrabError::NotFound {
                    path: path.to_owned(),
                });
            }
            let mut blob_buf = Vec::new();
            let data = odb
                .try_find(&oid, &mut blob_buf)
                .map_err(|e| CrabError::Internal(format!("failed to read blob {oid}: {e}")))?
                .ok_or_else(|| {
                    CrabError::Internal(format!("blob {oid} missing from object database"))
                })?;
            if data.kind != gix_object::Kind::Blob {
                return Err(CrabError::Internal(format!(
                    "oid {oid} is not a blob (kind = {:?})",
                    data.kind
                )));
            }
            return Ok(data.data.to_vec());
        }

        if !matches!(kind, GixEntryKind::Tree) {
            return Err(CrabError::NotFound {
                path: path.to_owned(),
            });
        }
        current_tree = oid;
    }

    Err(CrabError::NotFound {
        path: path.to_owned(),
    })
}

fn list_entries_blocking(git_dir: &Path, commit_sha: &str) -> Result<Vec<DownloadEntry>> {
    let odb = open_odb(git_dir)?;
    let commit_oid = ObjectId::from_hex(commit_sha.as_bytes())
        .map_err(|e| CrabError::Internal(format!("invalid commit sha {commit_sha:?}: {e}")))?;
    let tree_id = {
        let mut buf = Vec::new();
        let mut iter = odb
            .find_commit_iter(&commit_oid, &mut buf)
            .map_err(|e| CrabError::Internal(format!("failed to read commit {commit_sha}: {e}")))?;
        iter.tree_id().map_err(|e| {
            CrabError::Internal(format!(
                "failed to parse tree id from commit {commit_sha}: {e}"
            ))
        })?
    };

    let mut entries = Vec::new();
    walk_tree_blocking(&odb, &tree_id, "", &mut entries)?;
    Ok(entries)
}

fn walk_tree_blocking<F>(
    odb: &F,
    tree_id: &gix_hash::oid,
    prefix: &str,
    entries: &mut Vec<DownloadEntry>,
) -> Result<()>
where
    F: Find,
{
    let mut buf = Vec::new();
    let tree_iter = odb
        .find_tree_iter(tree_id, &mut buf)
        .map_err(|e| CrabError::Internal(format!("failed to read tree {tree_id}: {e}")))?;
    let tree_entries = tree_iter
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| CrabError::Internal(format!("corrupt tree {tree_id}: {e}")))?;

    for entry in tree_entries {
        let name = std::str::from_utf8(entry.filename.as_ref()).map_err(|e| {
            CrabError::Internal(format!("non-UTF-8 filename in tree {tree_id}: {e}"))
        })?;
        let full_path = if prefix.is_empty() {
            name.to_owned()
        } else {
            format!("{prefix}/{name}")
        };
        let oid = entry.oid.to_owned();

        match entry.mode.kind() {
            GixEntryKind::Blob | GixEntryKind::BlobExecutable => {
                entries.push(classify_blob_object(odb, &oid, &full_path)?);
            }
            GixEntryKind::Tree => {
                walk_tree_blocking(odb, &oid, &full_path, entries)?;
            }
            GixEntryKind::Link | GixEntryKind::Commit => {}
        }
    }

    Ok(())
}

fn classify_blob_object<F>(odb: &F, oid: &ObjectId, path: &str) -> Result<DownloadEntry>
where
    F: Find,
{
    let mut buf = Vec::new();
    let data = odb
        .try_find(oid, &mut buf)
        .map_err(|e| CrabError::Internal(format!("failed to read blob {oid}: {e}")))?
        .ok_or_else(|| CrabError::Internal(format!("blob {oid} missing from object database")))?;
    if data.kind != gix_object::Kind::Blob {
        return Err(CrabError::Internal(format!(
            "oid {oid} is not a blob (kind = {:?})",
            data.kind
        )));
    }
    classify_blob_bytes(path, data.data)
}

fn classify_blob_bytes(path: &str, blob: &[u8]) -> Result<DownloadEntry> {
    if blob.len() <= POINTER_PEEK_THRESHOLD {
        let crab_peek_len = blob.len().min(crab_types::pointer::MAX_POINTER_SIZE);
        if let Ok(ptr) = Pointer::parse(&blob[..crab_peek_len]) {
            return Ok(DownloadEntry {
                path: path.to_owned(),
                size: ptr.size,
                kind: DownloadEntryKind::CrabPointer,
                content_hash: Some(hex_encode(&ptr.file_hash)),
            });
        }

        if !blob.is_empty() {
            let lfs_peek_len = blob.len().min(MAX_BLOB_PEEK);
            if let Ok(lfs_ptr) = LfsPointer::parse(&blob[..lfs_peek_len]) {
                return Ok(DownloadEntry {
                    path: path.to_owned(),
                    size: lfs_ptr.size,
                    kind: DownloadEntryKind::LfsPointer,
                    content_hash: Some(hex_encode(&lfs_ptr.oid)),
                });
            }
        }
    }

    Ok(DownloadEntry {
        path: path.to_owned(),
        size: blob.len() as u64,
        kind: DownloadEntryKind::Git,
        content_hash: None,
    })
}

fn open_odb(git_dir: &Path) -> Result<gix_odb::Handle> {
    let effective_git_dir = resolve_common_dir(git_dir);
    let objects_dir = effective_git_dir.join("objects");
    if !objects_dir.is_dir() {
        return Err(CrabError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("git objects directory not found: {}", objects_dir.display()),
        )));
    }
    gix_odb::at(&objects_dir).map_err(|e| {
        CrabError::Internal(format!(
            "failed to open git object database at {}: {e}",
            objects_dir.display()
        ))
    })
}

fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    let commondir_file = git_dir.join("commondir");
    if let Ok(content) = std::fs::read_to_string(&commondir_file) {
        let relative = content.trim();
        if !relative.is_empty() {
            let resolved = git_dir.join(relative);
            if let Ok(canonical) = resolved.canonicalize() {
                return canonical;
            }
            return resolved;
        }
    }
    git_dir.to_path_buf()
}

fn read_workspace_remote(work_dir: &Path, config: &Config) -> Result<String> {
    if let Some(url) = config.remote_url.as_deref()
        && !url.is_empty()
    {
        return Ok(url.to_owned());
    }

    Err(CrabError::Configuration {
        key: format!(
            "no Crab or raw object-store remote configured under {}",
            work_dir.display()
        ),
        origin: "run `crab configure <url>` to create crab.toml before downloading pointer content"
            .to_owned(),
    })
}

fn resolve_manifest_rev(manifest: &Manifest, rev: &str) -> Option<String> {
    if is_full_hex_sha(rev) {
        return Some(rev.to_ascii_lowercase());
    }

    let ref_name = if rev == "HEAD" || rev == "head" {
        manifest_head_target(manifest)?
    } else if manifest.refs.contains_key(rev) {
        rev.to_owned()
    } else if !rev.starts_with("refs/") {
        [
            format!("refs/{rev}"),
            format!("refs/heads/{rev}"),
            format!("refs/tags/{rev}"),
        ]
        .into_iter()
        .find(|candidate| manifest.refs.contains_key(candidate))?
    } else {
        return None;
    };

    manifest.refs.get(&ref_name).cloned()
}

fn manifest_head_target(manifest: &Manifest) -> Option<String> {
    if manifest.refs.contains_key(&manifest.head) {
        return Some(manifest.head.clone());
    }
    manifest.refs.keys().next().cloned()
}

fn is_full_hex_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit())
}

fn local_path_to_file_url(path: &Path) -> String {
    let s = path.to_string_lossy();
    #[cfg(windows)]
    let s = s.replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

fn cache_key_for(input: &str) -> String {
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

fn verify_lfs_content(path: &str, pointer: &LfsPointer, content: &[u8]) -> Result<()> {
    use sha2::Digest as _;

    let actual: [u8; 32] = sha2::Sha256::digest(content).into();
    if actual != pointer.oid {
        return Err(CrabError::CorruptObject {
            path: path.to_owned(),
            reason: format!(
                "LFS object sha256 mismatch: pointer declares {}, got {}",
                hex_encode(&pointer.oid),
                hex_encode(&actual),
            ),
        });
    }
    Ok(())
}

fn write_blob_to_writer<W>(mut writer: W, blob: &[u8]) -> Result<u64>
where
    W: std::io::Write,
{
    for chunk in blob.chunks(WRITE_CHUNK_BYTES) {
        writer.write_all(chunk)?;
    }
    writer.flush()?;
    Ok(blob.len() as u64)
}

struct Sha256Tap<W> {
    writer: W,
    hasher: sha2::Sha256,
}

impl<W> Sha256Tap<W> {
    fn new(writer: W) -> Self {
        use sha2::Digest as _;

        Self {
            writer,
            hasher: sha2::Sha256::new(),
        }
    }
}

impl<W> Sha256Tap<W>
where
    W: std::io::Write,
{
    fn verify(mut self, path: &str, pointer: &LfsPointer, bytes: u64) -> Result<()> {
        use sha2::Digest as _;

        self.writer.flush()?;
        if bytes != pointer.size {
            return Err(CrabError::CorruptObject {
                path: path.to_owned(),
                reason: format!(
                    "LFS object size mismatch: pointer declares {}, got {bytes}",
                    pointer.size,
                ),
            });
        }

        let actual: [u8; 32] = self.hasher.finalize().into();
        if actual != pointer.oid {
            return Err(CrabError::CorruptObject {
                path: path.to_owned(),
                reason: format!(
                    "LFS object sha256 mismatch: pointer declares {}, got {}",
                    hex_encode(&pointer.oid),
                    hex_encode(&actual),
                ),
            });
        }
        Ok(())
    }
}

impl<W> std::io::Write for Sha256Tap<W>
where
    W: std::io::Write,
{
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.write_all(buf)?;
        use sha2::Digest as _;
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_manifest_rev_accepts_common_names() {
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest
            .refs
            .insert("refs/heads/main".to_owned(), "a".repeat(40));
        manifest
            .refs
            .insert("refs/tags/v1".to_owned(), "b".repeat(40));

        assert_eq!(
            resolve_manifest_rev(&manifest, "HEAD"),
            Some("a".repeat(40))
        );
        assert_eq!(
            resolve_manifest_rev(&manifest, "main"),
            Some("a".repeat(40))
        );
        assert_eq!(resolve_manifest_rev(&manifest, "v1"), Some("b".repeat(40)));
    }

    #[test]
    fn classify_empty_blob_as_git_file() {
        let entry = classify_blob_bytes("empty.txt", b"").expect("classify");
        assert_eq!(entry.kind, DownloadEntryKind::Git);
        assert_eq!(entry.size, 0);
    }
}
