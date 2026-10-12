//! Build immutable browse indexes from a caller-pinned Git snapshot.

use crab_metadata::capsule_protocol::BrowseIndexes;

use super::*;

/// Build a durable graph without waiting for complete path attribution.
///
/// The caller holds generation ownership and GC fences, supplies an origin-backed
/// reader for `snapshot`, and rechecks capsule freshness before publishing the
/// result. A reusable path prefix is checkpointed before replacing the old record
/// so restart and GC retain it while the new graph is available to readers.
pub async fn build_graph(
    layout: &StoreLayout<Store>,
    repository: &RemoteGitRepository,
    snapshot: &manifest_store::RepositorySnapshot,
    previous: Option<&BrowseIndexes>,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> Result<BrowseIndexes> {
    if !repository.matches_snapshot(snapshot) || !repository.matches_store_layout(layout) {
        return Err(WriteError::Internal(
            "browse-index reader does not match its captured origin and snapshot".to_owned(),
        ));
    }
    let store = layout.store();
    let manifest = &snapshot.manifest;
    let limits = options.object_limits();
    let mut old_graph = None;
    let mut old_paths = None;
    if let Some(previous) = previous {
        match load_split_commit_graph(
            store,
            layout,
            previous.commit_graph_hash(),
            limits.max_commit_graph_bytes,
        )
        .await
        {
            Ok(graph) => {
                if let Some(path_hash) = previous.path_state_hash() {
                    match load_path_state(
                        store,
                        layout,
                        path_hash,
                        &graph,
                        limits.max_path_state_bytes,
                    )
                    .await
                    {
                        Ok(paths) => old_paths = Some(paths),
                        Err(error) if immutable_metadata_unavailable(&error) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                old_graph = Some(graph);
            }
            Err(error) if immutable_metadata_unavailable(&error) => {}
            Err(error) => return Err(error.into()),
        }
    }
    check_cancelled(cancel)?;
    // A superseded partial graph may retain commits no longer reachable from
    // these refs. Without complete attribution to reuse, start from current
    // roots so path construction never authorizes a retained unreachable commit.
    if previous.is_some_and(|previous| previous.state_digest() != snapshot.manifest_etag)
        && old_paths.is_none()
    {
        old_graph = None;
    }
    if let Some(previous) = previous
        && previous.state_digest() == snapshot.manifest_etag
        && old_graph.as_ref().is_some_and(|graph| {
            graph.descriptor.generation == manifest.generation
                && graph.descriptor.pack_index_hash == manifest.pack_index_hash
                && graph.descriptor.git_validation_digest == manifest.git_validation_digest
        })
        && old_paths.is_some()
    {
        return Ok(previous.clone());
    }
    let roots = commit_roots(repository, manifest, cancel).await?;
    let inputs =
        collect_commit_graph_inputs(repository, old_graph.as_ref(), &roots, cancel).await?;
    let write = append_split_commit_graph(
        old_graph,
        manifest.generation,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        &roots,
        inputs,
    )?
    .ok_or_else(|| {
        WriteError::Internal("remote commit graph traversal was incomplete".to_owned())
    })?;
    upload_split_commit_graph(store, layout, &write).await?;
    let graph_hash = write.descriptor_hash;
    let graph =
        load_split_commit_graph(store, layout, &graph_hash, limits.max_commit_graph_bytes).await?;
    if let Some(paths) = old_paths.filter(|paths| path_state_is_prefix(paths, &graph)) {
        let (progress, replace_checkpoint) =
            path_state_progress(store, layout, &graph, limits.max_path_state_bytes).await?;
        if progress
            .as_ref()
            .is_none_or(|progress| progress.descriptor.commit_count < paths.descriptor.commit_count)
        {
            // Checkpoint monotonicity applies to verified progress. Replace a
            // corrupt observed descriptor before publishing the new graph, or
            // restart loses the only reusable attribution for retained commits.
            let write = append_path_state(Some(paths), &graph, Vec::new())?;
            upload_path_state(store, layout, &write).await?;
            publish_path_state_checkpoint(
                store,
                layout,
                &graph,
                &write.descriptor_hash,
                write.commit_count(),
                replace_checkpoint.as_deref(),
            )
            .await?;
        }
    }
    BrowseIndexes::new(snapshot.manifest_etag.clone(), graph_hash, None).map_err(Into::into)
}

/// Finish path attribution from the graph's durable construction checkpoint.
///
/// The caller retains generation ownership and GC fences through publication;
/// the supplied graph record and reader must bind the exact captured snapshot.
pub async fn build_path_state(
    layout: &StoreLayout<Store>,
    repository: &RemoteGitRepository,
    snapshot: &manifest_store::RepositorySnapshot,
    indexes: &BrowseIndexes,
    options: RepositoryOptions,
    cancel: &CancellationToken,
) -> Result<BrowseIndexes> {
    if !repository.matches_snapshot(snapshot)
        || !repository.matches_store_layout(layout)
        || indexes.state_digest() != snapshot.manifest_etag
    {
        return Err(WriteError::Internal(
            "path-state reader and graph do not match their captured origin and snapshot"
                .to_owned(),
        ));
    }
    let store = layout.store();
    let limits = options.object_limits();
    let graph_hash = indexes.commit_graph_hash();
    let graph =
        load_split_commit_graph(store, layout, graph_hash, limits.max_commit_graph_bytes).await?;
    let repository = repository.with_commit_graph(graph_hash, cancel).await?;
    let (mut base, mut replace_checkpoint) =
        path_state_progress(store, layout, &graph, limits.max_path_state_bytes).await?;
    loop {
        check_cancelled(cancel)?;
        let first = base
            .as_ref()
            .map_or(0, |index| index.descriptor.commit_count);
        let end = first
            .saturating_add(PATH_STATE_CHECKPOINT_COMMITS)
            .min(graph.descriptor.commit_count);
        let inputs = collect_path_state_inputs(&repository, &graph, first, end, cancel).await?;
        let write = append_path_state(base, &graph, inputs)?;
        upload_path_state(store, layout, &write).await?;
        if write.commit_count() == graph.descriptor.commit_count {
            return BrowseIndexes::new(
                snapshot.manifest_etag.clone(),
                graph_hash.to_owned(),
                Some(write.descriptor_hash),
            )
            .map_err(Into::into);
        }
        publish_path_state_checkpoint(
            store,
            layout,
            &graph,
            &write.descriptor_hash,
            write.commit_count(),
            replace_checkpoint.as_deref(),
        )
        .await?;
        replace_checkpoint = None;
        base = Some(write.into_index());
    }
}

async fn commit_roots(
    repository: &RemoteGitRepository,
    manifest: &Manifest,
    cancel: &CancellationToken,
) -> Result<Vec<[u8; 20]>> {
    let mut roots = Vec::new();
    for batch in commit_graph_roots(manifest)?.chunks(COMMIT_GRAPH_BATCH_SIZE) {
        let operation = repository.operation(OperationKind::History, cancel).await?;
        let result = async {
            for oid in batch {
                let id = gix_hash::ObjectId::from(*oid);
                match operation.read_object_metadata(id).await?.kind {
                    gix_object::Kind::Commit => roots.push(*oid),
                    gix_object::Kind::Tag => {
                        match repository.snapshot(&Revision::Commit(id), &operation).await {
                            Ok(snapshot) => roots.push(oid_bytes(snapshot.commit_oid())?),
                            Err(crab_remote_git::Error::Revision {
                                reason: crab_remote_git::RevisionError::NotCommit,
                            }) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    // Git permits tags to trees/blobs. They remain readable,
                    // but contribute no commit or first-parent attribution.
                    gix_object::Kind::Tree | gix_object::Kind::Blob => {}
                }
            }
            Ok(())
        }
        .await;
        operation.finish(result).await?;
    }
    roots.sort_unstable();
    roots.dedup();
    Ok(roots)
}
