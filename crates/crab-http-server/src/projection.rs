//! Rebuilds the repository Cell's Git browse projection from object storage.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use crab_cell_runtime::cell::executor::MutationIdentity;
use crab_cell_runtime::client::{CellClient, Committed, InvocationError};
use crab_cell_runtime::identity::CellTarget;
use crab_cell_runtime::identity::RequestId;
use crab_metadata::manifest_store::{RepositorySnapshot, read_repository_snapshot};
use crab_metadata::path_state::{PathStateIndex, load_path_state};
use crab_metadata::split_commit_graph::{SplitCommitGraph, load_split_commit_graph};
use crab_remote_git::{
    Commit, EntryKind, OperationKind, RemoteGitRepository, RepositoryIdentity, RepositoryOptions,
    Revision, TreeEntry,
};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::cells::{RepositoryCellRouter, repository::projection};

pub(crate) use projection::AttributionItem;

const COMMIT_BATCH: u32 = 64;
const MAX_TREE_PAYLOAD_BYTES: usize = 640 * 1024;
const ATTRIBUTION_BATCH_NODES: usize = 64;
const MAX_MESSAGE_PREVIEW_BYTES: usize = 8 * 1024;
const MAX_ATTRIBUTION_QUERY_BYTES: usize = 48 * 1024;
const TREE_HISTORY_WINDOW: u32 = 256;
const REF_BATCH: usize = 128;

/// Rebuild one immutable projection epoch after Git generation maintenance.
pub(crate) async fn reconcile(
    store: &Store,
    layout: &StoreLayout<Store>,
    identity: &RepositoryIdentity,
    runtime: Arc<crab_remote_git::RemoteGitRuntime>,
    options: RepositoryOptions,
    context: crate::maintenance::ProjectionContext,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    if cancellation.is_cancelled() {
        return Ok(());
    }
    let build_started = Instant::now();
    context
        .metrics
        .record_projection_origin_read(crate::metrics::ProjectionOriginReadKind::Snapshot);
    let snapshot = read_snapshot(store, layout).await?;
    if !snapshot.journal.transactions.is_empty() {
        return Ok(());
    }
    let source = source_identity(&snapshot)?;
    let repository = RemoteGitRepository::open(
        store.clone(),
        layout.clone(),
        identity.clone(),
        runtime,
        options,
        cancellation,
    )
    .await?;
    let scheduled = context
        .router
        .route_projection(context.repository_id)
        .await?;
    let target = scheduled.cell.target.clone();
    let client = scheduled.cell.client.clone();
    let release_after = scheduled.should_release();
    let result = build_epoch(
        store,
        layout,
        options,
        &repository,
        &client,
        &target,
        &source,
        &context.metrics,
        cancellation,
    )
    .await;
    let result = match result {
        Ok(()) => collect_epochs(&client, &target, &source).await,
        Err(error) => Err(error),
    };
    let build_result = match &result {
        Ok(()) => crate::metrics::ProjectionBuildResult::Ok,
        Err(crate::Error::Remote(crab_remote_git::Error::SnapshotUnavailable)) => {
            crate::metrics::ProjectionBuildResult::Superseded
        }
        Err(_) => crate::metrics::ProjectionBuildResult::Error,
    };
    context.metrics.record_projection_build(
        crate::metrics::ProjectionPhase::Reconcile,
        build_result,
        build_started.elapsed(),
    );
    drop(scheduled.cell);
    let drained = if release_after {
        context.router.drain_local_target(&target).await
    } else {
        Ok(())
    };
    match (result, drained) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn collect_epochs(
    client: &CellClient,
    target: &CellTarget,
    source: &projection::SourceIdentity,
) -> crate::Result<()> {
    apply_batch(
        client,
        target,
        projection::OP_COLLECT,
        0,
        &serde_json::to_vec(source)?,
        Vec::new(),
    )
    .await
    .map(|_| ())
}

/// Derive the exact source identity used by Cell epoch promotion and reads.
pub(crate) fn source_identity(
    snapshot: &RepositorySnapshot,
) -> crate::Result<projection::SourceIdentity> {
    let manifest = &snapshot.manifest;
    let mut source = projection::SourceIdentity {
        source_token: String::new(),
        manifest_generation: manifest.generation,
        manifest_etag: snapshot.manifest_etag.clone(),
        journal_state_digest: snapshot.journal.state_digest.clone(),
        pack_index_hash: manifest.pack_index_hash.clone(),
        git_validation_digest: manifest.git_validation_digest.clone(),
        commit_graph_hash: manifest.commit_graph_hash.clone(),
        path_state_hash: manifest.path_state_hash.clone(),
        head_ref: manifest.head.as_bytes().to_vec(),
    };
    let canonical = serde_json::to_vec(&source)?;
    source.source_token = blake3::hash(&canonical).to_hex().to_string();
    Ok(source)
}

pub(crate) async fn current_source(
    store: &Store,
    layout: &StoreLayout<Store>,
) -> crate::Result<projection::SourceIdentity> {
    let source = source_identity(&read_snapshot(store, layout).await?)?;
    if let Some(path_state_hash) = source.path_state_hash.as_deref() {
        crab_metadata::path_state::load_path_state_descriptor(
            store,
            layout,
            path_state_hash,
            crab_metadata::path_state::DEFAULT_MAX_PATH_STATE_BYTES,
        )
        .await
        .map_err(|_| {
            crate::Error::Remote(crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::PathState,
            })
        })?;
    }
    Ok(source)
}

pub(crate) async fn state(
    router: &RepositoryCellRouter,
    repository_id: Uuid,
) -> crate::Result<projection::ProjectionStateView> {
    let scheduled = router.route_projection(repository_id).await?;
    let target = scheduled.cell.target.clone();
    let result = scheduled
        .cell
        .client
        .query::<projection::GetProjectionState>(&target, None, projection::ProjectionState)
        .await
        .map(|observed| observed.output)
        .map_err(invocation_error);
    let release_after = scheduled.should_release();
    drop(scheduled.cell);
    let drained = if release_after {
        router.drain_local_target(&target).await
    } else {
        Ok(())
    };
    match (result, drained) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(output), Ok(())) => Ok(output),
    }
}

pub(crate) async fn attribution(
    router: &RepositoryCellRouter,
    repository_id: Uuid,
    source_token: &str,
    commit_oid: &[u8],
    paths: Vec<Vec<u8>>,
) -> crate::Result<projection::AttributionResponse> {
    let batches = attribution_batches(source_token, commit_oid, paths)?;
    let scheduled = router.route_projection(repository_id).await?;
    let target = scheduled.cell.target.clone();
    let result = async {
        let mut items = Vec::new();
        for paths in batches {
            let input = serde_json::to_vec(&projection::AttributionQuery {
                source_token: source_token.to_owned(),
                commit_oid: commit_oid.to_vec(),
                paths,
            })?;
            let observed = scheduled
                .cell
                .client
                .query::<projection::GetProjectionAttribution>(&target, None, input)
                .await
                .map_err(invocation_error)?;
            let response: projection::AttributionResponse =
                serde_json::from_slice(&observed.output).map_err(crate::Error::Json)?;
            if response.state != "ready" {
                return Ok(response);
            }
            items.extend(response.items);
        }
        Ok(projection::AttributionResponse {
            state: "ready".to_owned(),
            items,
        })
    }
    .await;
    let release_after = scheduled.should_release();
    drop(scheduled.cell);
    let drained = if release_after {
        router.drain_local_target(&target).await
    } else {
        Ok(())
    };
    match (result, drained) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(output), Ok(())) => Ok(output),
    }
}

fn attribution_batches(
    source_token: &str,
    commit_oid: &[u8],
    paths: Vec<Vec<u8>>,
) -> crate::Result<Vec<Vec<Vec<u8>>>> {
    let mut batches = Vec::new();
    let mut current = Vec::new();
    for path in paths {
        let mut candidate = current.clone();
        candidate.push(path.clone());
        let encoded = serde_json::to_vec(&projection::AttributionQuery {
            source_token: source_token.to_owned(),
            commit_oid: commit_oid.to_vec(),
            paths: candidate.clone(),
        })?;
        if encoded.len() > MAX_ATTRIBUTION_QUERY_BYTES && !current.is_empty() {
            batches.push(current);
            current = vec![path];
            let single = serde_json::to_vec(&projection::AttributionQuery {
                source_token: source_token.to_owned(),
                commit_oid: commit_oid.to_vec(),
                paths: current.clone(),
            })?;
            if single.len() > MAX_ATTRIBUTION_QUERY_BYTES {
                return Err(crate::Error::Config(
                    "one path exceeds the projection query limit",
                ));
            }
        } else if encoded.len() > MAX_ATTRIBUTION_QUERY_BYTES {
            return Err(crate::Error::Config(
                "one path exceeds the projection query limit",
            ));
        } else {
            current = candidate;
        }
    }
    if current.is_empty() && batches.is_empty() {
        batches.push(Vec::new());
    } else if !current.is_empty() {
        batches.push(current);
    }
    Ok(batches)
}

#[expect(
    clippy::too_many_arguments,
    reason = "the builder keeps origin, Cell, source, metrics, and cancellation boundaries explicit"
)]
async fn build_epoch(
    store: &Store,
    layout: &StoreLayout<Store>,
    options: RepositoryOptions,
    repository: &RemoteGitRepository,
    client: &CellClient,
    target: &CellTarget,
    source: &projection::SourceIdentity,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    let source_bytes = serde_json::to_vec(source)?;
    let Some(epoch) = apply_batch(
        client,
        target,
        projection::OP_BEGIN,
        0,
        source_bytes.as_slice(),
        Vec::new(),
    )
    .await?
    .epoch_id
    else {
        return Ok(());
    };

    let result = build_epoch_contents(
        store,
        layout,
        options,
        repository,
        client,
        target,
        source,
        epoch,
        metrics,
        cancellation,
    )
    .await;
    if let Err(error) = result {
        let _ = apply_batch(
            client,
            target,
            projection::OP_SUPERSEDE,
            epoch,
            source_bytes.as_slice(),
            Vec::new(),
        )
        .await;
        return Err(error);
    }

    let latest = read_snapshot(store, layout).await?;
    let unchanged = latest.journal.transactions.is_empty()
        && source_identity(&latest)?.source_token == source.source_token;
    if !unchanged {
        // The staged epoch is never made visible when direct Git publication
        // advanced the object-store identity during the import.
        metrics.record_projection_superseded();
        apply_batch(
            client,
            target,
            projection::OP_SUPERSEDE,
            epoch,
            source_bytes.as_slice(),
            Vec::new(),
        )
        .await?;
        return Ok(());
    }
    apply_batch(
        client,
        target,
        projection::OP_PROMOTE,
        epoch,
        source_bytes.as_slice(),
        Vec::new(),
    )
    .await?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the builder keeps origin, Cell, source, and cancellation boundaries explicit"
)]
async fn build_epoch_contents(
    store: &Store,
    layout: &StoreLayout<Store>,
    options: RepositoryOptions,
    repository: &RemoteGitRepository,
    client: &CellClient,
    target: &CellTarget,
    source: &projection::SourceIdentity,
    epoch: u64,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    metrics.record_projection_origin_read(crate::metrics::ProjectionOriginReadKind::Snapshot);
    let snapshot = read_snapshot(store, layout).await?;
    let refs = reference_rows(&snapshot)?;
    for batch in refs.chunks(REF_BATCH) {
        send_json_batch(
            client,
            target,
            projection::OP_REFS,
            epoch,
            source,
            &batch,
            metrics,
        )
        .await?;
    }

    if repository.refs().is_empty() {
        return Ok(());
    }
    let graph_hash = source
        .commit_graph_hash
        .as_deref()
        .ok_or(crate::Error::Config(
            "non-empty repository has no commit graph descriptor",
        ))?;
    let path_hash = source
        .path_state_hash
        .as_deref()
        .ok_or(crate::Error::Config(
            "non-empty repository has no path-state descriptor",
        ))?;
    metrics.record_projection_origin_read(crate::metrics::ProjectionOriginReadKind::Graph);
    let graph = load_split_commit_graph(
        store,
        layout,
        graph_hash,
        options.object_limits().max_commit_graph_bytes,
    )
    .await
    .map_err(metadata_error)?;
    let path_state = load_path_state(
        store,
        layout,
        path_hash,
        &graph,
        options.object_limits().max_path_state_bytes,
    )
    .await
    .map_err(metadata_error)?;
    project_commits(
        repository,
        &graph,
        client,
        target,
        source,
        epoch,
        metrics,
        cancellation,
    )
    .await?;
    project_trees(
        repository,
        &graph,
        &snapshot.manifest.refs,
        client,
        target,
        source,
        epoch,
        metrics,
        cancellation,
    )
    .await?;
    project_attribution(
        &graph,
        &path_state,
        client,
        target,
        source,
        epoch,
        metrics,
        cancellation,
    )
    .await
}

async fn project_commits(
    repository: &RemoteGitRepository,
    graph: &SplitCommitGraph,
    client: &CellClient,
    target: &CellTarget,
    source: &projection::SourceIdentity,
    epoch: u64,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    let mut start = 0_u32;
    while start < graph.descriptor.commit_count {
        check_cancelled(cancellation)?;
        let end = start
            .saturating_add(COMMIT_BATCH)
            .min(graph.descriptor.commit_count);
        let operation = repository
            .operation(OperationKind::History, cancellation)
            .await?;
        let result = async {
            let mut rows = Vec::with_capacity((end - start) as usize);
            for ordinal in start..end {
                if cancellation.is_cancelled() {
                    return Err(crab_remote_git::Error::Cancelled);
                }
                let record =
                    graph
                        .record(ordinal)
                        .ok_or(crab_remote_git::Error::InternalInvariant {
                            invariant: "projection commit graph ordinal is missing",
                        })?;
                let oid = object_id(&record.oid).map_err(|_| {
                    crab_remote_git::Error::InternalInvariant {
                        invariant: "projection commit graph has a non-SHA-1 object ID",
                    }
                })?;
                let snapshot = repository
                    .snapshot(&Revision::Commit(oid), &operation)
                    .await?;
                let commit = snapshot.commit(&operation).await?;
                rows.push(commit_row(ordinal, &commit));
            }
            Ok::<_, crab_remote_git::Error>(rows)
        }
        .await;
        let rows = operation.finish(result).await?;
        send_json_batch(
            client,
            target,
            projection::OP_COMMITS,
            epoch,
            source,
            &rows,
            metrics,
        )
        .await?;
        start = end;
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "tree hydration keeps the verified Git and Cell boundaries explicit"
)]
async fn project_trees(
    repository: &RemoteGitRepository,
    graph: &SplitCommitGraph,
    refs: &std::collections::BTreeMap<String, String>,
    client: &CellClient,
    target: &CellTarget,
    source: &projection::SourceIdentity,
    epoch: u64,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    let mut roots = HashMap::<Vec<u8>, Vec<u8>>::new();
    let mut selected = BTreeSet::new();
    let history_start = graph
        .descriptor
        .commit_count
        .saturating_sub(TREE_HISTORY_WINDOW);
    selected.extend(history_start..graph.descriptor.commit_count);
    for oid in refs.values() {
        let oid = decode_sha1(oid)?;
        let oid: [u8; 20] = oid
            .try_into()
            .map_err(|_| crate::Error::Config("manifest ref is not a SHA-1"))?;
        if let Some(ordinal) = graph.ordinal(&oid) {
            selected.insert(ordinal);
        }
    }
    for ordinal in selected {
        let record = graph
            .record(ordinal)
            .ok_or(crate::Error::Config("commit graph ordinal is missing"))?;
        roots
            .entry(record.tree_oid.to_vec())
            .or_insert_with(|| record.oid.to_vec());
    }
    for (root, commit) in roots {
        check_cancelled(cancellation)?;
        metrics.record_projection_origin_read(crate::metrics::ProjectionOriginReadKind::Tree);
        let root_oid = object_id(&root)?;
        let commit_oid = object_id(&commit)?;
        let operation = repository
            .operation(OperationKind::Tree, cancellation)
            .await?;
        let snapshot = repository
            .snapshot(&Revision::Commit(commit_oid), &operation)
            .await?;
        if snapshot.root_tree_oid() != root_oid {
            operation
                .finish(Err(crab_remote_git::Error::Corrupt {
                    stage: crab_remote_git::CorruptionStage::Tree,
                }))
                .await?;
            return Err(crate::Error::Config(
                "commit graph root tree does not match the snapshot",
            ));
        }
        let entries_result = snapshot.list_tree_recursive(&operation).await;
        let entries = operation.finish(entries_result).await?;
        for row in tree_rows(root, entries)? {
            send_json_batch(
                client,
                target,
                projection::OP_TREES,
                epoch,
                source,
                &row,
                metrics,
            )
            .await?;
        }
    }
    Ok(())
}

async fn project_attribution(
    graph: &SplitCommitGraph,
    path_state: &PathStateIndex,
    client: &CellClient,
    target: &CellTarget,
    source: &projection::SourceIdentity,
    epoch: u64,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    if cancellation.is_cancelled() {
        return Err(crate::Error::Remote(crab_remote_git::Error::Cancelled));
    }
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for (layer_index, layer) in path_state.layers.iter().enumerate() {
        for (node_index, node) in layer.nodes.iter().enumerate() {
            check_cancelled(cancellation)?;
            let parent_hash = node_hash(source, layer_index as u32, node_index as u32);
            nodes.push(projection::AttributionNodeRow {
                node_hash: parent_hash.clone(),
                last_change_ordinal: node.value,
                encoded_bytes: 0,
            });
            for (component, child) in &node.children {
                edges.push(projection::AttributionEdgeRow {
                    parent_hash: parent_hash.clone(),
                    component: component.clone(),
                    child_hash: node_hash(source, child.layer, child.index),
                });
            }
        }
    }
    let roots = (0..graph.descriptor.commit_count)
        .map(|ordinal| {
            let root = path_state
                .record(ordinal)
                .ok_or(crate::Error::Config("path-state record is missing"))?
                .root;
            Ok(projection::AttributionRootRow {
                commit_ordinal: ordinal,
                root_hash: node_hash(source, root.layer, root.index),
            })
        })
        .collect::<crate::Result<Vec<_>>>()?;
    // Attribution edges and roots have foreign keys into the node table. Upload the
    // dependency layers separately so a large path-state index cannot expose an edge
    // before its child node has been committed.
    for node_batch in nodes.chunks(ATTRIBUTION_BATCH_NODES) {
        check_cancelled(cancellation)?;
        let batch = projection::AttributionBatch {
            nodes: node_batch.to_vec(),
            edges: Vec::new(),
            roots: Vec::new(),
        };
        send_json_batch(
            client,
            target,
            projection::OP_ATTRIBUTION,
            epoch,
            source,
            &batch,
            metrics,
        )
        .await?;
    }
    for edge_batch in edges.chunks(ATTRIBUTION_BATCH_NODES * 8) {
        check_cancelled(cancellation)?;
        let batch = projection::AttributionBatch {
            nodes: Vec::new(),
            edges: edge_batch.to_vec(),
            roots: Vec::new(),
        };
        send_json_batch(
            client,
            target,
            projection::OP_ATTRIBUTION,
            epoch,
            source,
            &batch,
            metrics,
        )
        .await?;
    }
    for root_batch in roots.chunks(ATTRIBUTION_BATCH_NODES) {
        check_cancelled(cancellation)?;
        let batch = projection::AttributionBatch {
            nodes: Vec::new(),
            edges: Vec::new(),
            roots: root_batch.to_vec(),
        };
        send_json_batch(
            client,
            target,
            projection::OP_ATTRIBUTION,
            epoch,
            source,
            &batch,
            metrics,
        )
        .await?;
    }
    Ok(())
}

fn check_cancelled(cancellation: &CancellationToken) -> crate::Result<()> {
    if cancellation.is_cancelled() {
        return Err(crate::Error::Remote(crab_remote_git::Error::Cancelled));
    }
    Ok(())
}

fn reference_rows(snapshot: &RepositorySnapshot) -> crate::Result<Vec<projection::RefRow>> {
    snapshot
        .manifest
        .refs
        .iter()
        .map(|(name, oid)| {
            Ok(projection::RefRow {
                name: name.as_bytes().to_vec(),
                target_oid: decode_sha1(oid)?,
                peeled_oid: snapshot
                    .manifest
                    .peeled_refs
                    .get(name)
                    .map(|oid| decode_sha1(oid))
                    .transpose()?,
            })
        })
        .collect()
}

fn commit_row(ordinal: u32, commit: &Commit) -> projection::CommitRow {
    let message = commit.message.as_ref();
    let preview = message[..message.len().min(MAX_MESSAGE_PREVIEW_BYTES)].to_vec();
    projection::CommitRow {
        ordinal,
        oid: commit.oid.as_slice().to_vec(),
        tree_oid: commit.tree.as_slice().to_vec(),
        parents: commit
            .parents
            .iter()
            .map(|parent| parent.as_slice().to_vec())
            .collect(),
        author_name: commit.author.name.to_vec(),
        author_email: commit.author.email.to_vec(),
        author_time: commit.author.seconds,
        author_tz_offset_seconds: commit.author.offset_seconds,
        committer_name: commit.committer.name.to_vec(),
        committer_email: commit.committer.email.to_vec(),
        committer_time: commit.committer.seconds,
        committer_tz_offset_seconds: commit.committer.offset_seconds,
        message_preview: preview,
        message_truncated: message.len() > MAX_MESSAGE_PREVIEW_BYTES,
        encoded_bytes: u64::try_from(message.len()).unwrap_or(u64::MAX),
    }
}

fn tree_rows(
    root: Vec<u8>,
    entries: Vec<TreeEntry>,
) -> crate::Result<Vec<Vec<projection::TreeRow>>> {
    let mut tree_by_path = HashMap::<Vec<u8>, Vec<u8>>::new();
    tree_by_path.insert(Vec::new(), root);
    let root_oid = tree_by_path.get(&[][..]).cloned();
    for entry in &entries {
        if entry.kind == EntryKind::Tree {
            let tree_oid = entry.oid.as_slice().to_vec();
            tree_by_path.insert(entry.path.as_bytes().to_vec(), tree_oid.clone());
        }
    }
    // A Git tree object can be mounted at more than one path.  The recursive
    // walk reports each mount, while the projection stores one row per tree
    // object, so coalesce repeated names before sending the row to SQLite.
    let mut grouped = HashMap::<Vec<u8>, BTreeMap<Vec<u8>, projection::TreeEntryRow>>::new();
    if let Some(root_oid) = root_oid {
        grouped.entry(root_oid).or_default();
    }
    for entry in entries {
        let path = entry.path.as_bytes();
        let parent = path
            .iter()
            .rposition(|byte| *byte == b'/')
            .map_or(&[][..], |index| &path[..index]);
        let Some(tree_oid) = tree_by_path.get(parent) else {
            continue;
        };
        grouped.entry(tree_oid.clone()).or_default();
        let Some(name) = entry.path.file_name() else {
            continue;
        };
        let row = projection::TreeEntryRow {
            name: name.to_vec(),
            mode: entry.mode.raw(),
            object_oid: entry.oid.as_slice().to_vec(),
            object_kind: entry_kind_code(entry.kind),
        };
        let entries = grouped.entry(tree_oid.clone()).or_default();
        if let Some(previous) = entries.insert(row.name.clone(), row.clone())
            && previous != row
        {
            return Err(crate::Error::Remote(crab_remote_git::Error::Corrupt {
                stage: crab_remote_git::CorruptionStage::Tree,
            }));
        }
    }
    let mut rows = Vec::new();
    for (tree_oid, entries) in grouped {
        let entries = entries.into_values().collect::<Vec<_>>();
        let row = projection::TreeRow {
            tree_oid,
            encoded_bytes: entries
                .iter()
                .map(|entry| {
                    u64::try_from(entry.name.len() + entry.object_oid.len() + 16)
                        .unwrap_or(u64::MAX)
                })
                .sum(),
            entries,
        };
        let encoded = serde_json::to_vec(&row)?;
        if encoded.len() > MAX_TREE_PAYLOAD_BYTES {
            return Err(crate::Error::Config(
                "one Git tree exceeds the projection batch limit",
            ));
        }
        rows.push(vec![row]);
    }
    Ok(rows)
}

fn entry_kind_code(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Tree => 1,
        EntryKind::Blob => 2,
        EntryKind::Symlink => 3,
        EntryKind::Submodule => 4,
    }
}

async fn send_json_batch<T: serde::Serialize>(
    client: &CellClient,
    target: &CellTarget,
    operation: u8,
    epoch: u64,
    source: &projection::SourceIdentity,
    value: &T,
    metrics: &crate::metrics::Metrics,
) -> crate::Result<()> {
    let payload = serde_json::to_vec(value)?;
    let rows = serde_json::from_slice::<serde_json::Value>(&payload)
        .ok()
        .map_or(1, |value| match &value {
            serde_json::Value::Array(rows) => rows.len(),
            serde_json::Value::Object(fields) => fields
                .values()
                .filter_map(serde_json::Value::as_array)
                .map(Vec::len)
                .sum::<usize>()
                .max(1),
            _ => 1,
        }) as u64;
    let kind = match operation {
        projection::OP_REFS => crate::metrics::ProjectionBatchKind::Refs,
        projection::OP_COMMITS => crate::metrics::ProjectionBatchKind::Commits,
        projection::OP_TREES => crate::metrics::ProjectionBatchKind::Trees,
        projection::OP_ATTRIBUTION => crate::metrics::ProjectionBatchKind::Attribution,
        _ => return Err(crate::Error::Config("invalid projection batch operation")),
    };
    metrics.record_projection_batch(kind, rows, payload.len() as u64);
    apply_batch(
        client,
        target,
        operation,
        epoch,
        &serde_json::to_vec(source)?,
        payload,
    )
    .await
    .map(|_| ())
}

async fn apply_batch(
    client: &CellClient,
    target: &CellTarget,
    operation: u8,
    epoch: u64,
    source: &[u8],
    payload: Vec<u8>,
) -> crate::Result<projection::ProjectionAck> {
    let input = projection::ProjectionBatch {
        operation,
        epoch_id: epoch,
        source: source.to_vec(),
        payload,
    };
    let committed: Committed<projection::ProjectionAck> = client
        .command::<projection::ApplyProjectionBatch>(target, mutation_identity()?, input)
        .await
        .map_err(invocation_error)?;
    Ok(committed.output)
}

fn mutation_identity() -> crate::Result<MutationIdentity> {
    let now_ms = crate::cells::unix_now_ms()?;
    let expires_at_ms = now_ms
        .checked_add(60_000)
        .ok_or(crate::Error::Config("projection request expiry overflowed"))?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms,
    })
}

fn invocation_error<T>(error: InvocationError<T>) -> crate::Error {
    match error {
        InvocationError::NotStarted(error) => crate::Error::Cell(error),
        InvocationError::Rejected(_) => crate::Error::Cell(crab_cell_runtime::Error::Command(
            "projection command rejected",
        )),
        InvocationError::Pending(_) => crate::Error::Cell(crab_cell_runtime::Error::Command(
            "projection command pending",
        )),
        InvocationError::InvalidPublishedResult { source, .. } => crate::Error::Cell(*source),
    }
}

fn object_id(bytes: &[u8]) -> crate::Result<gix_hash::ObjectId> {
    gix_hash::ObjectId::try_from(bytes)
        .map_err(|_| crate::Error::Config("Git object ID is not a SHA-1"))
}

fn decode_sha1(value: &str) -> crate::Result<Vec<u8>> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(crate::Error::Config("manifest ref is not a SHA-1"));
    }
    (0..40)
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| crate::Error::Config("manifest ref is not a SHA-1"))
        })
        .collect()
}

fn node_hash(source: &projection::SourceIdentity, layer: u32, index: u32) -> Vec<u8> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab git attribution node v1\0");
    hasher.update(source.source_token.as_bytes());
    hasher.update(&layer.to_be_bytes());
    hasher.update(&index.to_be_bytes());
    hasher.finalize().as_bytes().to_vec()
}

async fn read_snapshot(
    store: &Store,
    layout: &StoreLayout<Store>,
) -> crate::Result<RepositorySnapshot> {
    read_repository_snapshot(store, layout)
        .await
        .map_err(metadata_error)
}

fn metadata_error(error: crab_metadata::error::MetadataError) -> crate::Error {
    crate::Error::Settings {
        source: Box::new(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_batches_respect_the_query_wire_budget() {
        let paths = (0..200)
            .map(|index| format!("dir/{index:03}/{}", "x".repeat(700)).into_bytes())
            .collect::<Vec<_>>();
        let batches = attribution_batches(&"a".repeat(64), &[0x11; 20], paths.clone()).unwrap();
        let flattened = batches.iter().flatten().cloned().collect::<Vec<_>>();
        assert_eq!(flattened, paths);
        assert!(batches.iter().all(|batch| {
            serde_json::to_vec(&projection::AttributionQuery {
                source_token: "a".repeat(64),
                commit_oid: vec![0x11; 20],
                paths: batch.clone(),
            })
            .unwrap()
            .len()
                <= MAX_ATTRIBUTION_QUERY_BYTES
        }));
    }

    #[test]
    fn attribution_batches_reject_one_oversized_path() {
        let paths = vec![vec![b'x'; MAX_ATTRIBUTION_QUERY_BYTES]];
        assert!(attribution_batches("token", &[0x11; 20], paths).is_err());
    }

    #[test]
    fn tree_rows_coalesce_reused_tree_objects() {
        let tree_oid =
            gix_hash::ObjectId::from_hex(b"2222222222222222222222222222222222222222").unwrap();
        let blob_oid =
            gix_hash::ObjectId::from_hex(b"3333333333333333333333333333333333333333").unwrap();
        let entries = vec![
            TreeEntry {
                path: crab_remote_git::GitPath::new(b"one".to_vec()).unwrap(),
                oid: tree_oid,
                mode: crab_remote_git::EntryMode::Tree,
                kind: EntryKind::Tree,
                size: None,
            },
            TreeEntry {
                path: crab_remote_git::GitPath::new(b"two".to_vec()).unwrap(),
                oid: tree_oid,
                mode: crab_remote_git::EntryMode::Tree,
                kind: EntryKind::Tree,
                size: None,
            },
            TreeEntry {
                path: crab_remote_git::GitPath::new(b"one/file".to_vec()).unwrap(),
                oid: blob_oid,
                mode: crab_remote_git::EntryMode::Regular,
                kind: EntryKind::Blob,
                size: None,
            },
            TreeEntry {
                path: crab_remote_git::GitPath::new(b"two/file".to_vec()).unwrap(),
                oid: blob_oid,
                mode: crab_remote_git::EntryMode::Regular,
                kind: EntryKind::Blob,
                size: None,
            },
        ];
        let rows = tree_rows(vec![0x11; 20], entries).unwrap();
        let reused_tree = rows
            .iter()
            .flatten()
            .find(|row| row.tree_oid == tree_oid.as_slice())
            .unwrap();
        assert_eq!(reused_tree.entries.len(), 1);
        assert_eq!(reused_tree.entries[0].name, b"file");
        assert_eq!(reused_tree.entries[0].object_oid, blob_oid.as_slice());
    }
}
