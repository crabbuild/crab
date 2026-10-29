//! Fenced ownership and captured Git mechanics for native browse-index publication.
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use crab_coordination::{
    CoordinationError, GIT_GENERATION_OWNER_RESOURCE, GcFenceHeartbeat, GcFenceLease,
    PushLockAcquireContext,
};
use crab_metadata::{
    error::MetadataError,
    manifest_store,
    manifests::Manifest,
    path_state::{
        PathStateIndex, PathStateInput, PathStateMutation, append_path_state, load_path_state,
        load_path_state_checkpoint, load_path_state_checkpoint_record,
        publish_path_state_checkpoint, upload_path_state,
    },
    split_commit_graph::{
        CommitGraphInput, SplitCommitGraph, load_split_commit_graph, upload_split_commit_graph,
    },
};
use crab_remote_git::{
    ChangeKind, EntryKind, OperationContext, OperationKind, RemoteGitObject, RemoteGitRepository,
    RemoteGitSnapshot, RepositoryOptions, Revision, TreeChange,
};
use crab_storage::{StorageError, Store, StoreLayout};
use tokio_util::sync::CancellationToken;

use crate::{Result, WriteError};

pub mod browse;

const COMMIT_GRAPH_BATCH_SIZE: usize = 512;
const PATH_STATE_CHECKPOINT_COMMITS: u32 = 32;

struct WriterFence {
    lease: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

impl WriterFence {
    async fn acquire(
        store: &Store,
        domain: &str,
        ttl: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check_cancelled(cancel)?;
        let lease = GcFenceLease::acquire_writer(store.inner(), domain, ttl).await?;
        let heartbeat = GcFenceHeartbeat::spawn(&lease, cancel.clone(), ttl / 3);
        Ok(Self { lease, heartbeat })
    }

    async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.lease.release().await.map_err(Into::into)
    }
}

/// Run derived-index maintenance under one renewed owner and both GC writer fences.
///
/// Losing owner election is benign. The operation must observe `cancel`; all
/// acquired leases are released before returning, including after cancellation.
pub async fn with_generation_owner<F, E>(
    store: &Store,
    layout: &StoreLayout<Store>,
    ttl: Duration,
    cancel: &CancellationToken,
    operation: F,
) -> std::result::Result<(), E>
where
    F: std::future::Future<Output = std::result::Result<(), E>>,
    E: From<WriteError> + From<CoordinationError>,
{
    let mut context = PushLockAcquireContext::new(Arc::clone(store.inner()));
    let mut owner = match context
        .try_acquire_internal(layout.repo_prefix(), GIT_GENERATION_OWNER_RESOURCE, ttl)
        .await
    {
        Ok(owner) => owner,
        Err(CoordinationError::PushLockHeld { .. }) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    // Keep the full catalog and graph operation behind one heap indirection.
    // Large committed bursts otherwise exhaust a default Tokio worker stack.
    let maintenance = Box::pin(async {
        let global = WriterFence::acquire(store, layout.global_prefix(), ttl, cancel).await?;
        let repo = match WriterFence::acquire(store, layout.repo_prefix(), ttl, cancel).await {
            Ok(repo) => repo,
            Err(error) => {
                let _ = global.release().await;
                return Err(E::from(error));
            }
        };
        let mut result = operation.await;
        for fence in [repo, global] {
            result = result.and(fence.release().await.map_err(E::from));
        }
        result
    });
    let result = crab_coordination::while_renewing(&mut owner, Some(cancel), maintenance).await;
    result.and(owner.release().await.map_err(Into::into))
}

async fn path_state_progress(
    store: &Store,
    layout: &StoreLayout<Store>,
    graph: &SplitCommitGraph,
    max_bytes: u64,
) -> Result<(Option<PathStateIndex>, Option<String>)> {
    match load_path_state_checkpoint(store, layout, graph, max_bytes).await {
        Ok(progress) => Ok((progress, None)),
        Err(error) if immutable_metadata_unavailable(&error) => {
            let record = match load_path_state_checkpoint_record(
                store,
                layout,
                &graph.descriptor.git_validation_digest,
            )
            .await
            {
                Ok(record) => record,
                Err(error) if immutable_metadata_unavailable(&error) => None,
                Err(error) => return Err(error.into()),
            };
            Ok((None, record.map(|record| record.descriptor_hash)))
        }
        Err(error) => Err(error.into()),
    }
}

fn path_state_is_prefix(index: &PathStateIndex, graph: &SplitCommitGraph) -> bool {
    index.descriptor().commit_count <= graph.descriptor.commit_count
        && (0..index.descriptor().commit_count).all(|ordinal| {
            index
                .record(ordinal)
                .zip(graph.record(ordinal))
                .is_some_and(|(path_record, graph_record)| {
                    path_record.oid == graph_record.oid
                        && path_record.first_parent == graph_record.parents.first().copied()
                })
        })
}

fn immutable_metadata_unavailable(error: &MetadataError) -> bool {
    matches!(
        error,
        MetadataError::CorruptObject { .. }
            | MetadataError::Storage {
                source: StorageError::CorruptObject { .. } | StorageError::NotFound { .. },
            }
    )
}

async fn collect_path_state_inputs(
    repository: &RemoteGitRepository,
    graph: &SplitCommitGraph,
    first: u32,
    end: u32,
    cancel: &CancellationToken,
) -> Result<Vec<PathStateInput>> {
    let mut inputs = Vec::new();
    for ordinal in first..end {
        check_cancelled(cancel)?;
        let operation = repository.operation(OperationKind::Compare, cancel).await?;
        let input = async {
            let record = graph
                .record(ordinal)
                .ok_or_else(|| crab_remote_git::Error::Corrupt {
                    stage: crab_remote_git::CorruptionStage::CommitGraph,
                })?;
            let oid = gix_hash::ObjectId::Sha1(record.oid);
            let snapshot = repository
                .snapshot(&Revision::Commit(oid), &operation)
                .await?;
            let commit = snapshot.commit(&operation).await?;
            let first_parent = commit.parents.first().copied();
            let mutations = if let Some(parent) = first_parent {
                let parent = repository
                    .snapshot(&Revision::Commit(parent), &operation)
                    .await?;
                let comparison = snapshot.compare(&parent, &operation).await?;
                path_state_mutations(&snapshot, comparison.changes, &operation).await?
            } else {
                root_path_state_mutations(&snapshot, &operation).await?
            };
            let message = commit
                .message
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or_default()
                .to_vec();
            Ok::<_, crab_remote_git::Error>(PathStateInput {
                oid: record.oid,
                first_parent: first_parent.map(oid_bytes).transpose()?,
                author: commit.author.name.to_vec(),
                author_seconds: commit.author.seconds,
                message,
                mutations,
            })
        }
        .await;
        inputs.push(operation.finish(input).await?);
    }
    Ok(inputs)
}

async fn root_path_state_mutations(
    snapshot: &RemoteGitSnapshot,
    operation: &OperationContext,
) -> crab_remote_git::Result<Vec<PathStateMutation>> {
    let mut mutations = vec![PathStateMutation {
        path: Vec::new(),
        present: false,
        reset: true,
    }];
    mutations.extend(
        snapshot
            .list_tree_recursive(operation)
            .await?
            .into_iter()
            .map(|entry| PathStateMutation {
                path: entry.path.as_bytes().to_vec(),
                present: true,
                reset: false,
            }),
    );
    Ok(mutations)
}

async fn path_state_mutations(
    snapshot: &RemoteGitSnapshot,
    changes: Vec<TreeChange>,
    operation: &OperationContext,
) -> crab_remote_git::Result<Vec<PathStateMutation>> {
    let mut mutations = Vec::new();
    let mut replaced_trees = Vec::new();
    for change in changes {
        let path = change.path.as_bytes();
        let reset = change.kind == ChangeKind::TypeChanged;
        let present = change.new.is_some();
        let replaced_tree = reset
            && change
                .new
                .as_ref()
                .is_some_and(|entry| entry.kind == EntryKind::Tree);
        mutations.push(PathStateMutation {
            path: path.to_vec(),
            present,
            reset,
        });
        if replaced_tree {
            replaced_trees.push(path.to_vec());
        }
        for (index, byte) in path.iter().enumerate() {
            if *byte == b'/' {
                mutations.push(PathStateMutation {
                    path: path[..index].to_vec(),
                    present: true,
                    reset: false,
                });
            }
        }
    }
    if !replaced_trees.is_empty() {
        mutations.extend(
            snapshot
                .list_tree_recursive(operation)
                .await?
                .into_iter()
                .filter(|entry| {
                    replaced_trees.iter().any(|prefix| {
                        entry
                            .path
                            .as_bytes()
                            .strip_prefix(prefix.as_slice())
                            .is_some_and(|suffix| suffix.starts_with(b"/"))
                    })
                })
                .map(|entry| PathStateMutation {
                    path: entry.path.as_bytes().to_vec(),
                    present: true,
                    reset: false,
                }),
        );
    }
    Ok(mutations)
}

fn oid_bytes(oid: gix_hash::ObjectId) -> std::result::Result<[u8; 20], crab_remote_git::Error> {
    oid.as_bytes()
        .try_into()
        .map_err(|_| crab_remote_git::Error::UnsupportedObjectFormat)
}

fn commit_graph_roots(manifest: &Manifest) -> Result<Vec<[u8; 20]>> {
    manifest
        .refs
        .iter()
        .map(|(name, oid)| manifest.peeled_refs.get(name).unwrap_or(oid))
        .map(|value| {
            let oid = gix_hash::ObjectId::from_hex(value.as_bytes()).map_err(|source| {
                WriteError::CorruptObject {
                    path: "manifest".to_owned(),
                    reason: format!("invalid commit graph root {value}: {source}"),
                }
            })?;
            oid.as_bytes()
                .try_into()
                .map_err(|_| WriteError::CorruptObject {
                    path: "manifest".to_owned(),
                    reason: format!("commit graph root is not SHA-1: {value}"),
                })
        })
        .collect()
}

async fn collect_commit_graph_inputs(
    repository: &RemoteGitRepository,
    base: Option<&SplitCommitGraph>,
    roots: &[[u8; 20]],
    cancel: &CancellationToken,
) -> std::result::Result<Vec<CommitGraphInput>, crab_remote_git::Error> {
    let mut pending = VecDeque::new();
    let mut queued = HashSet::new();
    for root in roots {
        if base.is_none_or(|graph| !graph.contains(root)) && queued.insert(*root) {
            pending.push_back(*root);
        }
    }
    let mut additions = Vec::new();
    while !pending.is_empty() {
        let mut requested = Vec::with_capacity(COMMIT_GRAPH_BATCH_SIZE);
        while requested.len() < COMMIT_GRAPH_BATCH_SIZE {
            let Some(oid) = pending.pop_front() else {
                break;
            };
            if base.is_none_or(|graph| !graph.contains(&oid)) {
                requested.push(gix_hash::ObjectId::from(oid));
            }
        }
        if requested.is_empty() {
            continue;
        }
        // Each bounded batch drains its admission and read budget. A complete
        // history must not exhaust one interactive operation's object limit.
        let operation = repository.operation(OperationKind::History, cancel).await?;
        let objects = operation.read_objects(&requested).await;
        let objects = operation.finish(objects).await?;
        if objects.len() != requested.len() {
            return Err(crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::Commit,
            });
        }
        for (expected, object) in requested.into_iter().zip(objects) {
            if object.oid != expected {
                return Err(crab_remote_git::Error::Corrupt {
                    stage: crab_remote_git::CorruptionStage::Commit,
                });
            }
            let input = commit_graph_input(&object)?;
            for parent in &input.parents {
                if base.is_none_or(|graph| !graph.contains(parent)) && queued.insert(*parent) {
                    pending.push_back(*parent);
                }
            }
            additions.push(input);
        }
    }
    Ok(additions)
}

fn commit_graph_input(
    object: &RemoteGitObject,
) -> std::result::Result<CommitGraphInput, crab_remote_git::Error> {
    if object.kind != gix_object::Kind::Commit {
        return Err(crab_remote_git::Error::ObjectKind {
            oid: object.oid,
            expected: gix_object::Kind::Commit,
            actual: object.kind,
        });
    }
    let parsed = gix_object::CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(|source| crab_remote_git::Error::CommitParse {
            oid: object.oid,
            source,
        })?
        .into_owned()
        .map_err(|source| crab_remote_git::Error::CommitParse {
            oid: object.oid,
            source,
        })?;
    let parents = parsed.parents.into_vec();
    Ok(CommitGraphInput {
        oid: object
            .oid
            .as_bytes()
            .try_into()
            .map_err(|_| crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::Commit,
            })?,
        tree_oid: parsed.tree.as_bytes().try_into().map_err(|_| {
            crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::Commit,
            }
        })?,
        commit_time: parsed.committer.time.seconds,
        parents: parents
            .iter()
            .map(|parent| {
                parent
                    .as_bytes()
                    .try_into()
                    .map_err(|_| crab_remote_git::Error::Corrupt {
                        stage: crab_remote_git::CorruptionStage::Commit,
                    })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?,
    })
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(WriteError::Cancelled);
    }
    Ok(())
}
