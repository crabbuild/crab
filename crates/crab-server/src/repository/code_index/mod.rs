//! Optional search acceleration for one verified default-branch tree.

mod format;
pub(crate) mod symbols;
pub(crate) use format::{Index, Mask, QueryMask, Row};

use bytes::Bytes;
use crab_remote_git::{
    BudgetDimension, OperationContext, OperationKind, RemoteGitRepository, RemoteGitSnapshot,
    Revision, TreeListingItem, TreeListingRequest,
};
use crab_storage::{ETag, StorageError};
use gix_hash::ObjectId;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::Repository;
use crate::server::Server;

const PATH: &str = "v2/derived/code-search-v3";
const MAX_FILE_BYTES: u64 = 256 * 1024;
const BUILD_PAGE: usize = 96;
const BUILD_CONCURRENCY: usize = 8;
const BUILD_PARSE_JOBS: usize = 4;
const MAX_INDEX_BYTES: usize = 32 * 1024 * 1024;
const CACHE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("opening the code search source failed")]
    Repository(#[from] crate::Error),
    #[error("reading the code search source failed")]
    Git(#[from] crab_remote_git::Error),
    #[error("code index storage failed")]
    Storage(#[from] StorageError),
    #[error("code index worker stopped")]
    Worker(#[from] tokio::task::JoinError),
    #[error("code index has an invalid format or checksum")]
    Invalid,
}

#[derive(Default)]
pub(crate) struct Cache(Mutex<VecDeque<(Uuid, Arc<Index>)>>);

impl Cache {
    async fn get(&self, id: Uuid, tree: ObjectId) -> Option<Arc<Index>> {
        let mut entries = self.0.lock().await;
        let index = entries
            .iter()
            .position(|(repo, value)| *repo == id && value.tree == tree)?;
        let entry = entries.remove(index)?;
        let result = Arc::clone(&entry.1);
        entries.push_front(entry);
        Some(result)
    }

    async fn insert(&self, id: Uuid, index: Arc<Index>) {
        let mut entries = self.0.lock().await;
        entries.retain(|(repo, existing)| *repo != id || existing.tree != index.tree);
        entries.push_front((id, index));
        let mut size: usize = entries.iter().map(|(_, entry)| entry.weight).sum();
        while size > CACHE_BYTES {
            if let Some((_, oldest)) = entries.pop_back() {
                size -= oldest.weight;
            }
        }
    }
}

pub(crate) async fn text(
    oid: ObjectId,
    operation: &OperationContext,
) -> crab_remote_git::Result<Option<String>> {
    // The caller already proved the blob and path through the pinned Git tree.
    let Some(blob) = operation.read_object_at_most(oid, MAX_FILE_BYTES).await? else {
        return Ok(None);
    };
    if blob.kind != gix_object::Kind::Blob {
        return Err(crab_remote_git::Error::ObjectKind {
            oid,
            expected: gix_object::Kind::Blob,
            actual: blob.kind,
        });
    }
    operation
        .charge(BudgetDimension::ResponseBytes, blob.data.len() as u64)
        .await?;
    if blob.data.contains(&0)
        || !matches!(
            crab_git::classify(&blob.data),
            crab_git::PointerKind::NotAPointer
        )
    {
        return Ok(None);
    }
    Ok(std::str::from_utf8(&blob.data).ok().map(str::to_owned))
}

pub(crate) async fn load(
    server: &Server,
    repo: &Repository,
    tree: ObjectId,
) -> Result<Option<Arc<Index>>, Error> {
    if let Some(value) = server.code_indexes.get(repo.id, tree).await {
        return Ok(Some(value));
    }
    let Some((bytes, _)) = stored(repo).await? else {
        return Ok(None);
    };
    if format::tree(&bytes)? != tree {
        return Ok(None);
    }
    let index = Arc::new(tokio::task::spawn_blocking(move || Index::decode(bytes)).await??);
    server
        .code_indexes
        .insert(repo.id, Arc::clone(&index))
        .await;
    Ok(Some(index))
}

async fn stored(repo: &Repository) -> Result<Option<(Bytes, ETag)>, Error> {
    match repo
        .store
        .get_with_etag_bounded(&repo.layout.repo_path(PATH), MAX_INDEX_BYTES as u64)
        .await
    {
        Ok(value) => Ok(Some(value)),
        Err(StorageError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn default_snapshot(
    server: &Server,
    git: &RemoteGitRepository,
    cancel: &CancellationToken,
) -> Result<Option<RemoteGitSnapshot>, Error> {
    let Some(head) = git.refs().head.as_ref() else {
        return Ok(None);
    };
    let op = git
        .operation_with_limits(
            OperationKind::Tree,
            cancel,
            server.options.operation_limits(),
        )
        .await?;
    let snapshot = git
        .snapshot(&Revision::Reference(head.name.clone()), &op)
        .await;
    Ok(Some(op.finish(snapshot).await?))
}

pub(crate) async fn reconcile(
    server: &Server,
    repo: &Repository,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let git = repo.revalidate(server, cancel).await?.git;
    let Some(snapshot) = default_snapshot(server, &git, cancel).await? else {
        return Ok(());
    };
    let tree = snapshot.root_tree_oid();
    if server.code_indexes.get(repo.id, tree).await.is_some() {
        return Ok(());
    }
    let previous = stored(repo).await?;
    let (old, etag) = match previous {
        Some((bytes, etag)) => {
            // A corrupt derived file is repairable. Keep the observed CAS token
            // so rebuilding it cannot replace a newer replica's publication.
            let value = tokio::task::spawn_blocking(move || Index::decode(bytes))
                .await?
                .ok();
            (value, Some(etag))
        }
        None => (None, None),
    };
    let old = match old {
        Some(old) if old.tree == tree => {
            server.code_indexes.insert(repo.id, Arc::new(old)).await;
            return Ok(());
        }
        old => old,
    };

    let index = build(server, &git, &snapshot, old.as_ref(), cancel).await?;
    let (index, bytes) = tokio::task::spawn_blocking(move || {
        let bytes = index.encode()?;
        Ok::<_, Error>((index, bytes))
    })
    .await??;
    let live = repo.revalidate(server, cancel).await?.git;
    if cancel.is_cancelled()
        || default_snapshot(server, &live, cancel)
            .await?
            .is_none_or(|live| live.root_tree_oid() != tree)
    {
        return Ok(());
    }
    let path = repo.layout.repo_path(PATH);
    // Readers require the exact tree, and an older builder can never
    // overwrite a successor that wrote the slot after this builder opened it.
    let publication = match etag {
        Some(etag) => repo.store.update(&path, bytes, etag).await,
        None => repo.store.create_strict_with_etag(&path, bytes).await,
    };
    match publication {
        Ok(_) => server.code_indexes.insert(repo.id, Arc::new(index)).await,
        Err(StorageError::StateConflict { .. }) => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

async fn build(
    server: &Server,
    git: &RemoteGitRepository,
    snapshot: &RemoteGitSnapshot,
    previous: Option<&Index>,
    cancel: &CancellationToken,
) -> Result<Index, Error> {
    let previous: HashMap<_, _> = previous
        .and_then(|index| index.rows.as_ref())
        .into_iter()
        .flatten()
        // The parser depends on the extension even when a rename keeps the same blob.
        .map(|row| {
            (
                (row.oid, symbols::Syntax::for_path(row.path.as_bytes())),
                row,
            )
        })
        .collect();
    let tree = snapshot.root_tree_oid();
    let mut size = format::OVERHEAD;
    let mut row_weight = 0;
    let mut after = None;
    let mut rows = Vec::new();
    loop {
        let request = TreeListingRequest::new(Bytes::new(), after, None, BUILD_PAGE)?;
        let operation = git
            .operation_with_limits(
                OperationKind::Content,
                cancel,
                server.options.operation_limits(),
            )
            .await?;
        let page = async {
            let listing = snapshot.list_tree_blobs(&request, &operation).await?;
            let mut batch = Vec::with_capacity(listing.items.len());
            for entries in listing.items.chunks(BUILD_CONCURRENCY) {
                let found = futures_util::future::try_join_all(entries.iter().map(|item| async {
                    let TreeListingItem::Blob(entry) = item else {
                        return Err(crab_remote_git::Error::InternalInvariant {
                            invariant: "ungrouped code index received a grouped path",
                        });
                    };
                    let syntax = symbols::Syntax::for_path(entry.path.as_bytes());
                    let data = match previous.get(&(entry.oid, syntax)) {
                        Some(row) => BuildData::Reused(row.filter.clone(), row.symbols.clone()),
                        None => BuildData::Text(text(entry.oid, &operation).await?),
                    };
                    Ok((entry.path.clone(), entry.oid, syntax, data))
                }))
                .await?;
                batch.extend(found);
            }
            Ok((batch, listing.has_more))
        }
        .await;
        let (batch, more) = operation.finish(page).await?;
        let partition_size = batch.len().div_ceil(BUILD_PARSE_JOBS).max(1);
        let mut batch = batch.into_iter();
        let partitions = std::iter::from_fn(|| {
            let partition: Vec<_> = batch.by_ref().take(partition_size).collect();
            (!partition.is_empty()).then_some(partition)
        });
        // Run independently admitted partitions in parallel, then assemble
        // them in tree order so the published index keeps its path cursor.
        let parsed = futures_util::future::try_join_all(partitions.map(|partition| {
            let parse_cancel = cancel.clone();
            server.runtime.run_cpu(cancel, move || {
                partition
                    .into_iter()
                    .map(|(path, oid, syntax, data)| {
                        if parse_cancel.is_cancelled() {
                            return Err(crab_remote_git::Error::Cancelled);
                        }
                        let (filter, symbols) = match data {
                            BuildData::Reused(filter, symbols) => (filter, symbols),
                            BuildData::Text(text) => {
                                let filter = text.as_deref().map(Mask::from_text);
                                let symbols =
                                    syntax.zip(text.as_deref()).and_then(|(syntax, text)| {
                                        symbols::definitions(syntax, text, &parse_cancel).map(
                                            |defs| {
                                                Mask::from_symbols(
                                                    defs.into_iter()
                                                        .map(|definition| &text[definition.bytes]),
                                                )
                                            },
                                        )
                                    });
                                (filter, symbols)
                            }
                        };
                        Ok(Row {
                            path,
                            oid,
                            filter,
                            symbols,
                        })
                    })
                    .collect::<crab_remote_git::Result<Vec<_>>>()
            })
        }))
        .await?;
        let page = parsed.into_iter().flatten().collect::<Vec<_>>();
        size += page.iter().map(Row::encoded_size).sum::<usize>();
        row_weight += page.len() * size_of::<Row>();
        if size > MAX_INDEX_BYTES || size + row_weight > CACHE_BYTES {
            return Ok(Index::new(tree, None, format::OVERHEAD));
        }
        after = page
            .last()
            .map(|row| Bytes::copy_from_slice(row.path.as_bytes()));
        rows.extend(page);
        if !more {
            return Ok(Index::new(tree, Some(rows), size));
        }
        if after.is_none() {
            return Err(Error::Invalid);
        }
    }
}

enum BuildData {
    Reused(Option<Mask>, Option<Mask>),
    Text(Option<String>),
}

pub(crate) async fn run(server: Arc<Server>) -> crate::Result<()> {
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(10)) => {}
        }
        for repo in server.repositories.values() {
            let cancel = server.cancellation.child_token();
            let work = async {
                let permit = server.maintenance_admission.clone().acquire_owned().await;
                if let Ok(_permit) = permit {
                    reconcile(&server, &repo, &cancel).await?;
                }
                Ok::<_, Error>(())
            };
            tokio::select! {
                () = server.cancellation.cancelled() => return Ok(()),
                result = work => if let Err(error) = result {
                    tracing::warn!(repository_id = %repo.id, ?error, "Code search index reconciliation was deferred");
                }
            }
        }
    }
}
