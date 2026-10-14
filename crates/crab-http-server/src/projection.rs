//! Rebuilds the repository Cell's Git browse projection from object storage.

use std::sync::Arc;
use std::time::Instant;

use cellule_runtime::cell::executor::MutationIdentity;
use cellule_runtime::client::{CellClient, Committed, InvocationError};
use cellule_runtime::identity::CellTarget;
use cellule_runtime::identity::RequestId;
use crab_metadata::manifest_store::RepositorySnapshot;
use crab_metadata::path_state::{PathStateItem, PathStateReader, PathStateRecord};
use crab_metadata::split_commit_graph::{SplitCommitGraph, load_split_commit_graph};
use crab_remote_git::{RemoteGitRepository, RepositoryOptions};
use crab_storage::{Store, StoreLayout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::cells::{RepositoryCellRouter, repository::projection};

pub(crate) use projection::AttributionItem;

const COMMIT_BATCH: u32 = 64;
const MAX_BATCH_PAYLOAD_BYTES: usize = 640 * 1024;
const ATTRIBUTION_BATCH_NODES: usize = 64;
const MAX_MESSAGE_PREVIEW_BYTES: usize = 8 * 1024;
const MAX_ATTRIBUTION_QUERY_BYTES: usize = 48 * 1024;
const REF_BATCH: usize = 128;

/// Rebuild one immutable projection epoch after Git generation maintenance.
pub(crate) async fn reconcile(
    layout: &StoreLayout<Store>,
    context: crate::maintenance::ProjectionContext,
    cancellation: &CancellationToken,
) -> crate::Result<Option<projection::SourceIdentity>> {
    let Some(router) = context.router else {
        return Ok(None);
    };
    let store = layout.store();
    let options = context.options;
    if cancellation.is_cancelled() {
        return Err(crab_remote_git::Error::Cancelled.into());
    }
    let build_started = Instant::now();
    context
        .metrics
        .record_projection_origin_read(crate::metrics::ProjectionOriginReadKind::Snapshot);
    let view = open_view(layout).await?;
    let snapshot = view.git_snapshot()?;
    let source = source_identity(&snapshot)?;
    let repository = view
        .git_repository_from_store(
            layout.clone(),
            context.identity.clone(),
            Arc::clone(&context.runtime),
            options,
            2 * 1024 * 1024 * 1024,
            cancellation,
        )
        .await?;
    let cell = router.route_projection(context.repository_id).await?;
    let target = cell.target.clone();
    let client = cell.client.clone();
    let result = build_epoch(
        store,
        layout,
        options,
        &repository,
        &client,
        &target,
        &source,
        &snapshot,
        &context.metrics,
        cancellation,
    )
    .await;
    let result = match result {
        Ok(true) => collect_epochs(&client, &target, &source)
            .await
            .map(|()| Some(source)),
        Ok(false) => Ok(None),
        Err(error) => Err(error),
    };
    let build_result = match &result {
        Ok(Some(_)) => crate::metrics::ProjectionBuildResult::Ok,
        Ok(None) => crate::metrics::ProjectionBuildResult::Superseded,
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
    result
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

pub(crate) struct CachedSource {
    verified: Instant,
    source_token: String,
}

pub(crate) async fn current_source(
    repository: &crate::server::Repository,
    server: &crate::server::Server,
    cancellation: &CancellationToken,
) -> crate::Result<projection::SourceIdentity> {
    let opened = repository.open(server, cancellation).await?;
    validate_source(repository, &opened.source, cancellation).await?;
    Ok(opened.source)
}

pub(crate) async fn validate_source(
    repository: &crate::server::Repository,
    source: &projection::SourceIdentity,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    let mut cached = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
        cached = repository.projection_source.lock() => cached,
    };
    // Foreground attribution and background probes share proof for the captured
    // source; a second open could validate a newer snapshot than the reader.
    if cached.as_ref().is_some_and(|previous| {
        previous.source_token == source.source_token
            && previous.verified.elapsed() < crate::integrity::SCRUB_INTERVAL
    }) {
        return Ok(());
    }
    tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
        result = async {
            if let Some(hash) = source.path_state_hash.as_deref() {
                crab_metadata::path_state::load_path_state_descriptor(
                    &repository.store,
                    &repository.layout,
                    hash,
                    crab_metadata::path_state::DEFAULT_MAX_PATH_STATE_BYTES,
                )
                .await
                .map_err(path_state_error)?;
            }
            Ok::<_, crate::Error>(())
        } => result?,
    }
    *cached = Some(CachedSource {
        verified: Instant::now(),
        source_token: source.source_token.clone(),
    });
    Ok(())
}

fn path_state_error(error: crab_metadata::error::MetadataError) -> crate::Error {
    use crab_metadata::error::MetadataError;
    match error {
        MetadataError::CorruptObject { .. }
        | MetadataError::Storage {
            source:
                crab_storage::StorageError::CorruptObject { .. }
                | crab_storage::StorageError::NotFound { .. },
        } => crab_remote_git::Error::Corrupt {
            stage: crab_remote_git::CorruptionStage::PathState,
        }
        .into(),
        error => crab_remote_git::Error::Metadata(error).into(),
    }
}

pub(crate) async fn state(
    router: &RepositoryCellRouter,
    repository_id: Uuid,
) -> crate::Result<projection::ProjectionStateView> {
    let cell = router.route_projection(repository_id).await?;
    cell.client
        .query::<projection::GetProjectionState>(&cell.target, None, projection::ProjectionState)
        .await
        .map(|observed| observed.output)
        .map_err(invocation_error)
}

pub(crate) async fn attribution(
    router: &RepositoryCellRouter,
    repository_id: Uuid,
    source_token: &str,
    commit_oid: &[u8],
    paths: Vec<Vec<u8>>,
) -> crate::Result<projection::AttributionResponse> {
    let batches = attribution_batches(source_token, commit_oid, paths)?;
    let cell = router.route_projection(repository_id).await?;
    let target = cell.target.clone();
    let mut items = Vec::new();
    for paths in batches {
        let input = serde_json::to_vec(&projection::AttributionQuery {
            source_token: source_token.to_owned(),
            commit_oid: commit_oid.to_vec(),
            paths,
        })?;
        let observed = cell
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
    snapshot: &RepositorySnapshot,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<bool> {
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
        return Ok(true);
    };

    let result = build_epoch_contents(
        store,
        layout,
        options,
        repository,
        client,
        target,
        source,
        snapshot,
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
        return Ok(false);
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
    Ok(true)
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
    snapshot: &RepositorySnapshot,
    epoch: u64,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    let refs = reference_rows(snapshot)?;
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
    let path_state = PathStateReader::load(
        store,
        layout,
        path_hash,
        crab_metadata::split_commit_graph::CommitGraphView::Materialized(&graph),
        options.object_limits().max_path_state_bytes,
    )
    .await
    .map_err(metadata_error)?;

    project_attribution(
        store,
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

#[expect(
    clippy::too_many_arguments,
    reason = "the importer binds origin, graph, Cell, epoch, metrics and cancellation"
)]
async fn project_attribution(
    store: &Store,
    graph: &SplitCommitGraph,
    path_state: &PathStateReader,
    client: &CellClient,
    target: &CellTarget,
    source: &projection::SourceIdentity,
    epoch: u64,
    metrics: &crate::metrics::Metrics,
    cancellation: &CancellationToken,
) -> crate::Result<()> {
    let mut scan = path_state.scan(store, graph).map_err(metadata_error)?;
    let mut commits = Vec::new();
    let mut commit_bytes = 2;
    let mut attribution = projection::AttributionBatch {
        nodes: Vec::new(),
        edges: Vec::new(),
        roots: Vec::new(),
    };
    let empty_bytes = serde_json::to_vec(&attribution)?.len();
    let mut attribution_bytes = empty_bytes;
    loop {
        let item = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            item = scan.next() => item.map_err(metadata_error)?,
        };
        let Some(item) = item else {
            break;
        };
        check_cancelled(cancellation)?;
        let (nodes, edges, roots) = match item {
            PathStateItem::Node { reference, value } => (
                Some(projection::AttributionNodeRow {
                    node_hash: node_hash(source, reference.layer, reference.index),
                    last_change_ordinal: value,
                    encoded_bytes: 0,
                }),
                None,
                None,
            ),
            PathStateItem::Edge {
                parent,
                name,
                child,
            } => (
                None,
                Some(projection::AttributionEdgeRow {
                    parent_hash: node_hash(source, parent.layer, parent.index),
                    component: name.to_vec(),
                    child_hash: node_hash(source, child.layer, child.index),
                }),
                None,
            ),
            PathStateItem::Record { ordinal, record } => {
                // Cache retained ordinals from verified metadata, without
                // resolving them as visible revisions or rereading Git bodies.
                let row = commit_row(ordinal, &record);
                let bytes = serde_json::to_vec(&row)?.len() + 1;
                if bytes + 2 > MAX_BATCH_PAYLOAD_BYTES {
                    return Err(crate::Error::Config(
                        "one commit summary exceeds the projection wire limit",
                    ));
                }
                if !commits.is_empty()
                    && (commits.len() == COMMIT_BATCH as usize
                        || commit_bytes + bytes > MAX_BATCH_PAYLOAD_BYTES)
                {
                    send_json_batch(
                        client,
                        target,
                        projection::OP_COMMITS,
                        epoch,
                        source,
                        &commits,
                        metrics,
                    )
                    .await?;
                    commits.clear();
                    commit_bytes = 2;
                }
                commit_bytes += bytes;
                commits.push(row);
                (
                    None,
                    None,
                    Some(projection::AttributionRootRow {
                        commit_ordinal: ordinal,
                        root_hash: node_hash(source, record.root.layer, record.root.index),
                    }),
                )
            }
        };
        let bytes = match (&nodes, &edges, &roots) {
            (Some(row), _, _) => serde_json::to_vec(row)?.len(),
            (_, Some(row), _) => serde_json::to_vec(row)?.len(),
            (_, _, Some(row)) => serde_json::to_vec(row)?.len(),
            _ => return Err(crate::Error::Config("empty attribution scan item")),
        } + 1;
        if bytes + empty_bytes > MAX_BATCH_PAYLOAD_BYTES {
            return Err(crate::Error::Config(
                "one attribution row exceeds the projection wire limit",
            ));
        }
        if attribution.nodes.len() == ATTRIBUTION_BATCH_NODES
            || attribution.edges.len() == ATTRIBUTION_BATCH_NODES * 8
            || attribution.roots.len() == ATTRIBUTION_BATCH_NODES
            || attribution_bytes + bytes > MAX_BATCH_PAYLOAD_BYTES
        {
            send_json_batch(
                client,
                target,
                projection::OP_ATTRIBUTION,
                epoch,
                source,
                &attribution,
                metrics,
            )
            .await?;
            attribution.nodes.clear();
            attribution.edges.clear();
            attribution.roots.clear();
            attribution_bytes = empty_bytes;
        }
        attribution.nodes.extend(nodes);
        attribution.edges.extend(edges);
        attribution.roots.extend(roots);
        attribution_bytes += bytes;
    }
    if !commits.is_empty() {
        send_json_batch(
            client,
            target,
            projection::OP_COMMITS,
            epoch,
            source,
            &commits,
            metrics,
        )
        .await?;
    }
    if !attribution.nodes.is_empty()
        || !attribution.edges.is_empty()
        || !attribution.roots.is_empty()
    {
        send_json_batch(
            client,
            target,
            projection::OP_ATTRIBUTION,
            epoch,
            source,
            &attribution,
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

fn commit_row(ordinal: u32, record: &PathStateRecord) -> projection::CommitRow {
    let message = &record.message;
    projection::CommitRow {
        ordinal,
        oid: record.oid.to_vec(),
        author_name: record.author.clone(),
        author_time: record.author_seconds,
        message_preview: message[..message.len().min(MAX_MESSAGE_PREVIEW_BYTES)].to_vec(),
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
        InvocationError::Rejected(_) => crate::Error::Cell(cellule_runtime::Error::Command(
            "projection command rejected",
        )),
        InvocationError::Pending(_) => crate::Error::Cell(cellule_runtime::Error::Command(
            "projection command pending",
        )),
        InvocationError::InvalidPublishedResult { source, .. } => crate::Error::Cell(*source),
    }
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
    _store: &Store,
    layout: &StoreLayout<Store>,
) -> crate::Result<RepositorySnapshot> {
    open_view(layout).await?.git_snapshot().map_err(Into::into)
}

pub(crate) async fn open_view(
    layout: &StoreLayout<Store>,
) -> crate::Result<crab_read::capsule_protocol::CapsuleRepositoryView> {
    let root = crab_metadata::capsule_protocol::load_root(layout)
        .await
        .map_err(metadata_error)?;
    // Origin-backed readers need complete visibility controls, while Git pack
    // bodies remain in their authenticated sources for bounded range reads.
    let view = crab_read::capsule_protocol::open_view_from_root_for_checkpoint(
        layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await?;
    let indexes = load_browse_indexes(layout).await?;
    Ok(view.with_browse_indexes(indexes))
}

pub(crate) async fn load_browse_indexes(
    layout: &StoreLayout<Store>,
) -> crate::Result<Option<crab_metadata::capsule_protocol::BrowseIndexes>> {
    crab_metadata::capsule_protocol::load_browse_indexes(layout)
        .await
        .map_err(|error| match &error {
            crab_metadata::error::MetadataError::CorruptObject { .. }
            | crab_metadata::error::MetadataError::BrowseIndexRecord { .. }
            | crab_metadata::error::MetadataError::Storage {
                source: crab_storage::StorageError::CorruptObject { .. },
            } => crate::Error::BrowseIndexes(Box::new(error)),
            _ => metadata_error(error),
        })
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
}

#[cfg(test)]
#[path = "projection_tests/probes.rs"]
mod probe_tests;
