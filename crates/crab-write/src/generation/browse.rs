//! Build immutable browse indexes from a caller-pinned Git snapshot.

use crab_metadata::capsule_protocol::BrowseIndexes;

use super::*;

/// Build complete indexes without publishing mutable repository authority.
///
/// The caller holds generation ownership and GC fences, supplies an origin-backed
/// reader for `snapshot`, and rechecks capsule freshness before publishing the
/// result. Interrupted path-state work resumes from verified 32-commit checkpoints.
pub async fn build(
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
                match load_path_state(
                    store,
                    layout,
                    previous.path_state_hash(),
                    &graph,
                    limits.max_path_state_bytes,
                )
                .await
                {
                    Ok(paths) => old_paths = Some(paths),
                    Err(error) if immutable_metadata_unavailable(&error) => {}
                    Err(error) => return Err(error.into()),
                }
                old_graph = Some(graph);
            }
            Err(error) if immutable_metadata_unavailable(&error) => {}
            Err(error) => return Err(error.into()),
        }
    }
    check_cancelled(cancel)?;
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
    let mut replace_checkpoint = None;
    let mut base = match load_path_state_checkpoint(
        store,
        layout,
        &graph,
        limits.max_path_state_bytes,
    )
    .await
    {
        Ok(base) => base,
        Err(error) if immutable_metadata_unavailable(&error) => {
            replace_checkpoint = match load_path_state_checkpoint_record(
                store,
                layout,
                &graph.descriptor.git_validation_digest,
            )
            .await
            {
                Ok(record) => record.map(|record| record.descriptor_hash),
                Err(error) if immutable_metadata_unavailable(&error) => None,
                Err(error) => return Err(error.into()),
            };
            None
        }
        Err(error) => return Err(error.into()),
    };
    if base.is_none() {
        base = old_paths.filter(|paths| path_state_is_prefix(paths, &graph));
    }
    loop {
        check_cancelled(cancel)?;
        let first = base
            .as_ref()
            .map_or(0, |index| index.descriptor.commit_count);
        let end = first
            .saturating_add(PATH_STATE_CHECKPOINT_COMMITS)
            .min(graph.descriptor.commit_count);
        let inputs = collect_path_state_inputs(repository, &graph, first, end, cancel).await?;
        let write = append_path_state(base, &graph, inputs)?;
        upload_path_state(store, layout, &write).await?;
        if write.commit_count() == graph.descriptor.commit_count {
            return BrowseIndexes::new(
                snapshot.manifest_etag.clone(),
                graph_hash,
                write.descriptor_hash,
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
