use std::collections::{BTreeMap, BTreeSet};

use crab_metadata::git_visibility::GitVisibilityIndex;
use crab_remote_git::{BudgetDimension, GeneratedPack, OperationContext, OperationKind};
use gix_hash::ObjectId;
use gix_object::{CommitRef, Kind, TreeRef, tree::EntryKind};
use tokio_util::sync::CancellationToken;

use super::{ReceiveError, Result};
use crate::{repository::Repository, server::Server};

pub(super) async fn select_pack(
    server: &Server,
    destination: &Repository,
    (prior_ref, prior_oid): (&str, ObjectId),
    result: ObjectId,
    (source, source_ref, source_oid): (&Repository, &str, ObjectId),
    objects: Vec<(Kind, Vec<u8>)>,
    cancel: &CancellationToken,
) -> Result<(Vec<(Kind, Vec<u8>)>, Option<GeneratedPack>)> {
    let (upstream_view, (fork_view, git)) = tokio::try_join!(
        async {
            destination
                .open_git_view()
                .await
                .map_err(ReceiveError::from)
        },
        async {
            source
                .open_capsule_repository(
                    server,
                    crab_read::upload_pack_repository_options()?,
                    cancel,
                )
                .await
                .map_err(ReceiveError::from)
        },
    )?;
    if upstream_view.refs().get(prior_ref) != Some(&prior_oid.to_string())
        || fork_view.refs().get(source_ref) != Some(&source_oid.to_string())
    {
        return Err(ReceiveError::BranchChanged);
    }
    let (upstream_index, fork_index) = tokio::try_join!(
        upstream_view.git_visibility_index(),
        fork_view.git_visibility_index(),
    )?;
    let generated = tokio::task::spawn_blocking(move || {
        objects
            .into_iter()
            .map(|(kind, bytes)| {
                let oid = crab_remote::objects::object_id(kind, &bytes)?;
                Ok((oid, (kind, bytes)))
            })
            .collect::<std::result::Result<BTreeMap<_, _>, crab_git::pack_writer::Error>>()
    })
    .await??;
    let operation = git.operation(OperationKind::UploadPack, cancel).await?;
    let selection = select_objects(
        |oid| upstream_index.contains_in_ref(prior_ref, oid),
        |oid| fork_index.contains_in_ref(source_ref, oid),
        result,
        generated,
        &operation,
        cancel,
    )
    .await;
    let (generated, selected) = operation.finish(selection).await?;
    let pack = if selected.is_empty() {
        None
    } else {
        Some(git.generate_pack(&selected, cancel).await?)
    };
    Ok((generated, pack))
}

pub(super) async fn restore_pack(
    git: &crab_remote_git::RemoteGitRepository,
    visibility: &GitVisibilityIndex,
    head: ObjectId,
    cancel: &CancellationToken,
) -> Result<Option<GeneratedPack>> {
    let visible = |oid: &[u8; 20]| {
        visibility.contains_for_refs(
            git.refs()
                .entries
                .iter()
                .map(|reference| reference.name.as_str()),
            oid,
        )
    };
    let oid: &[u8; 20] = head
        .as_bytes()
        .try_into()
        .map_err(|_| ReceiveError::Request("Branch restoration requires a SHA-1 commit"))?;
    if visible(oid) {
        return Ok(None);
    }
    let operation = git.operation(OperationKind::UploadPack, cancel).await?;
    // The caller has revalidated this root against the saved PR in this
    // repository. Read only its verified graph from the retained inventory.
    // Generic Git receive still rejects dangling OIDs supplied by callers.
    let selected =
        select_objects(visible, |_| true, head, BTreeMap::new(), &operation, cancel).await;
    let (_, selected) = operation.finish(selected).await?;
    let pack = git.generate_pack(&selected, cancel).await?;
    Ok(Some(pack))
}

async fn select_objects(
    destination_visible: impl Fn(&[u8; 20]) -> bool,
    source_visible: impl Fn(&[u8; 20]) -> bool,
    result: ObjectId,
    mut unused: BTreeMap<ObjectId, (Kind, Vec<u8>)>,
    operation: &OperationContext,
    cancel: &CancellationToken,
) -> crab_remote_git::Result<(Vec<(Kind, Vec<u8>)>, Vec<ObjectId>)> {
    let mut pending = vec![(result, Kind::Commit)];
    let mut visited = BTreeSet::new();
    let mut generated = Vec::new();
    let mut selected = Vec::new();
    while let Some((oid, kind)) = pending.pop() {
        if cancel.is_cancelled() {
            return Err(crab_remote_git::Error::Cancelled);
        }
        if !visited.insert(oid) {
            continue;
        }
        let hash =
            oid.as_bytes()
                .try_into()
                .map_err(|_| crab_remote_git::Error::InternalInvariant {
                    invariant: "fork merge selected a non-SHA-1 object",
                })?;
        if destination_visible(hash) {
            continue;
        }
        if let Some((actual, bytes)) = unused.remove(&oid) {
            verify_kind(oid, kind, actual)?;
            extend_links(oid, kind, &bytes, &mut pending, operation).await?;
            generated.push((kind, bytes));
            continue;
        }
        // Walk only exact dependencies of the result. In particular, a squash
        // or rebase need not publish the original fork commits or their trees.
        if !source_visible(hash) {
            return Err(crab_remote_git::Error::ObjectNotFound { oid });
        }
        selected.push(oid);
        if kind != Kind::Blob {
            let object = operation.read_object(oid).await?;
            verify_kind(oid, kind, object.kind)?;
            extend_links(oid, kind, &object.data, &mut pending, operation).await?;
        }
    }
    Ok((generated, selected))
}

fn verify_kind(oid: ObjectId, expected: Kind, actual: Kind) -> crab_remote_git::Result<()> {
    if expected != actual {
        return Err(crab_remote_git::Error::ObjectKind {
            oid,
            expected,
            actual,
        });
    }
    Ok(())
}

async fn extend_links(
    oid: ObjectId,
    kind: Kind,
    data: &[u8],
    pending: &mut Vec<(ObjectId, Kind)>,
    operation: &OperationContext,
) -> crab_remote_git::Result<()> {
    match kind {
        Kind::Commit => {
            let commit = CommitRef::from_bytes(data, gix_hash::Kind::Sha1)
                .map_err(|source| crab_remote_git::Error::CommitParse { oid, source })?;
            operation
                .charge(BudgetDimension::Entries, commit.parents.len() as u64 + 1)
                .await?;
            pending.push((commit.tree(), Kind::Tree));
            pending.extend(commit.parents().map(|oid| (oid, Kind::Commit)));
        }
        Kind::Tree => {
            let tree = TreeRef::from_bytes(data, gix_hash::Kind::Sha1)
                .map_err(|source| crab_remote_git::Error::TreeParse { oid, source })?;
            operation
                .charge(BudgetDimension::Entries, tree.entries.len() as u64)
                .await?;
            pending.extend(
                tree.entries
                    .into_iter()
                    .filter_map(|entry| match entry.mode.kind() {
                        EntryKind::Commit => None,
                        EntryKind::Tree => Some((entry.oid.to_owned(), Kind::Tree)),
                        _ => Some((entry.oid.to_owned(), Kind::Blob)),
                    }),
            );
        }
        Kind::Blob => {}
        Kind::Tag => {
            return Err(crab_remote_git::Error::InternalInvariant {
                invariant: "fork merge result reached a tag object",
            });
        }
    }
    Ok(())
}
