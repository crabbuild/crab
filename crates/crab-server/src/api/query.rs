use super::{
    Action, Parameters, Payload,
    representation::{
        commit_json, commit_summary_json, content_json, display_path, encode_cursor, encode_hex,
        entry_json, json_attribution_summary, path_history_json,
    },
};
use crate::{repository::projection, server::Server};
use crab_remote_git::{
    Blob, EntryKind, Error, GitPath, HistoryTraversal, OperationKind, PageRequest,
    RemoteGitRepository, RemoteGitSnapshot, Revision, RevisionError, TreeChange, TreeEntry,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[expect(
    clippy::too_many_arguments,
    reason = "one dispatcher carries the already validated HTTP read context"
)]
pub(super) async fn execute(
    server: &Server,
    repository: &RemoteGitRepository,
    repository_id: Uuid,
    action: Action,
    params: &Parameters,
    path: GitPath,
    page: PageRequest,
    search_query: Option<&str>,
    cancellation: &CancellationToken,
    projection_source: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> crab_remote_git::Result<Payload> {
    if matches!(action, Action::Refs) {
        let refs = repository.refs();
        return Ok(Payload::Json(json!({
            "generation": repository.generation(), "packs": repository.pack_count(),
            "head": refs.head.as_ref().map(|head| json!({"name":head.name, "oid":head.target.to_string()})),
            "unborn_head": refs.unborn_head,
            "refs": refs.entries.iter().map(|entry| json!({"name":entry.name,"oid":entry.target.to_string(),"peeled":entry.peeled.map(|oid|oid.to_string())})).collect::<Vec<_>>(),
        })));
    }
    let revision = revision(repository, params.rev.as_deref())?;
    let path_history = !path.is_root() && matches!(action, Action::Commit | Action::Commits);
    let kind = match action {
        Action::Refs => OperationKind::Repository,
        Action::Commit if path_history => OperationKind::PathHistory,
        Action::Commit => OperationKind::Commit,
        Action::Commits if path_history => OperationKind::PathHistory,
        Action::Commits => OperationKind::History,
        Action::Tree | Action::Search => OperationKind::Tree,
        Action::TreeAttribution => OperationKind::DirectoryAttribution,
        Action::Blob | Action::Asset | Action::File | Action::FilePreview => OperationKind::Content,
        Action::Changes => OperationKind::Compare,
        Action::Diff => OperationKind::Diff,
        Action::Blame => OperationKind::Blame,
    };
    let mut limits = server.options.operation_limits();
    if matches!(action, Action::Blob | Action::Asset) {
        // Raw bodies have their own bounded allowance. Raising the JSON cap
        // would admit large tree/diff responses without helping blob reads.
        limits.max_response_bytes = crate::server::MAX_BLOB_BYTES;
    }
    let operation = repository
        .operation_with_limits(kind, cancellation, limits)
        .await?;
    let result = async {
        let snapshot = repository.snapshot(&revision, &operation).await?;
        let commit = snapshot.commit(&operation).await?;
        let mut value = match action {
            Action::Refs => commit_json(&commit),
            Action::Commit if path_history => {
                let result = snapshot
                    .path_history(
                        &path,
                        HistoryTraversal::FirstParent,
                        &PageRequest::new(1, None)?,
                        &operation,
                    )
                    .await?;
                let latest = result.items.first().ok_or(Error::PathNotFound)?;
                path_history_json(latest)
            }
            Action::Commit => commit_json(&commit),
            Action::Commits => {
                let (items, next) = if path_history {
                    let result = snapshot
                        .path_history(&path, HistoryTraversal::FirstParent, &page, &operation)
                        .await?;
                    (
                        result.items.iter().map(path_history_json).collect::<Vec<_>>(),
                        result.next,
                    )
                } else if let Some(base) = params.base.as_deref() {
                    let base = repository
                        .snapshot(&Revision::parse(base)?, &operation)
                        .await?;
                    let result = snapshot.ahead_history(&base, &page, &operation).await?;
                    (
                        result.items.iter().map(commit_json).collect::<Vec<_>>(),
                        result.next,
                    )
                } else {
                    let result = snapshot
                        .history(HistoryTraversal::FirstParent, &page, &operation)
                        .await?;
                    (
                        result.items.iter().map(commit_json).collect::<Vec<_>>(),
                        result.next,
                    )
                };
                json!({"items":items,"next":next.map(|cursor|encode_cursor(&server.cursor_key,repository_id,cursor))})
            }
            Action::Tree => {
                let result = snapshot.list_directory(&path, &page, &operation).await?;
                let directory = snapshot
                    .entry(&path, &operation)
                    .await?
                    .ok_or(Error::PathNotFound)?;
                json!({
                    "directory_oid": directory.oid.to_string(),
                    "items": result.items.iter().map(entry_json).collect::<Vec<_>>(),
                    "next": result.next.map(|cursor|encode_cursor(&server.cursor_key,repository_id,cursor)),
                })
            }
            Action::TreeAttribution => {
                let result = snapshot.list_directory(&path, &page, &operation).await?;
                let directory = snapshot
                    .entry(&path, &operation)
                    .await?
                    .ok_or(Error::PathNotFound)?;
                let projected = server
                    .cells()
                    .zip(projection_source)
                    .map(|(router, source)| {
                        let paths = result
                            .items
                            .iter()
                            .map(|entry| entry.path.as_bytes().to_vec())
                            .collect::<Vec<_>>();
                        (router.clone(), source.to_owned(), paths)
                    });
                let projected = if let Some((router, source, paths)) = projected {
                    match projection::attribution(
                        &router,
                        repository_id,
                        &source,
                        snapshot.commit_oid().as_slice(),
                        paths,
                    )
                    .await
                    {
                        Ok(response)
                            if response.state == "ready"
                                && response.items.len() == result.items.len()
                                && response
                                    .items
                                    .iter()
                                    .zip(result.items.iter())
                                    .all(|(item, entry)| {
                                        item.path.as_slice() == entry.path.as_bytes()
                                    }) =>
                        {
                            Some(response.items)
                        }
                        Ok(response) => {
                            tracing::debug!(
                                state = %response.state,
                                "projection attribution is not ready for the pinned snapshot"
                            );
                            return Err(Error::PathStateIndexing {
                                generation: repository.generation(),
                            });
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = ?error,
                                "projection attribution query failed for the pinned snapshot"
                            );
                            return Err(Error::PathStateIndexing {
                                generation: repository.generation(),
                            });
                        }
                    }
                } else {
                    None
                };
                let commits = if let Some(items) = projected {
                    items
                        .into_iter()
                        .map(|item| json_attribution_summary(&item))
                        .collect::<Vec<_>>()
                } else {
                    snapshot
                        .latest_directory_entry_commits(&path, &result.items, &operation)
                        .await?
                        .into_iter()
                        .map(|commit| commit_summary_json(&commit))
                        .collect::<Vec<_>>()
                };
                let items = result
                    .items
                    .iter()
                    .zip(commits.iter())
                    .map(|(entry, commit)| {
                        let mut value = entry_json(entry);
                        value["last_commit"] = commit.clone();
                        value
                    })
                    .collect::<Vec<_>>();
                json!({
                    "state": "ready",
                    "directory_oid": directory.oid.to_string(),
                    "items": items,
                    "next": result.next.map(|cursor|encode_cursor(&server.cursor_key,repository_id,cursor)),
                })
            }
            Action::Search => {
                let query = search_query.ok_or(Error::InternalInvariant {
                    invariant: "validated repository search had no query",
                })?;
                let mut matches = Vec::new();
                for entry in snapshot.list_tree_recursive(&operation).await? {
                    if entry.kind == EntryKind::Tree {
                        continue;
                    }
                    let Some(score) = search_score(&display_path(entry.path.as_bytes()), query)
                    else {
                        continue;
                    };
                    matches
                        .try_reserve(1)
                        .map_err(|source| Error::Allocation {
                            requested: std::mem::size_of::<((u8, usize), TreeEntry)>(),
                            source,
                        })?;
                    matches.push((score, entry));
                }
                matches.sort_by(|left, right| {
                    left.0.cmp(&right.0).then_with(|| {
                        left.1.path.as_bytes().cmp(right.1.path.as_bytes())
                    })
                });
                let truncated = matches.len() > page.limit();
                matches.truncate(page.limit());
                json!({"items":matches.iter().map(|(_,entry)|entry_json(entry)).collect::<Vec<_>>(),"truncated":truncated})
            }
            Action::Blob | Action::FilePreview => {
                return super::revalidation::read(server, &snapshot, &path, &operation, action, headers).await.map(|(response, elapsed)| Payload::Response(response, elapsed));
            }
            Action::Asset => {
                return Ok(Payload::Asset(snapshot.read_blob(&path, &operation).await?));
            }
            // The released truncation flag requires inspected UTF-8. Metadata
            // omission belongs to file-preview; guessing here changes clients'
            // binary/encoding behavior even when the JSON shape stays the same.
            Action::File => content_json(&snapshot.read_blob(&path, &operation).await?),
            Action::Changes | Action::Diff => {
                let base_revision = params.base.as_deref().map(Revision::parse).transpose()?
                    .or_else(|| commit.parents.first().copied().map(Revision::Commit));
                let mut base = match base_revision {
                    Some(revision) => Some(repository.snapshot(&revision, &operation).await?),
                    None => None,
                };
                let mut base_tip = None;
                if params.merge_base && let Some(tip) = &base {
                    base_tip = Some(tip.commit_oid().to_string());
                    base = Some(merge_base_snapshot(repository, tip, &snapshot, &operation).await?);
                }
                if matches!(action, Action::Diff) {
                    let blobs = comparison_blobs_for_snapshots(
                        base.as_ref(),
                        &snapshot,
                        &path,
                        &operation,
                    )
                    .await?;
                    if blobs.old.is_none() && blobs.new.is_none() {
                        return Err(Error::PathNotFound);
                    }
                    json!({
                        "base": base.as_ref().map(|base| base.commit_oid().to_string()),
                        "old": blobs.old.as_ref().map(content_json),
                        "new": blobs.new.as_ref().map(content_json),
                        "path": display_path(path.as_bytes()),
                        "path_hex": encode_hex(path.as_bytes())
                    })
                } else if let Some(base) = &base {
                    let result = snapshot.compare(base, &operation).await?;
                    comparison_json(result, base_tip)
                } else {
                    let mut pending = vec![GitPath::root()];
                    let mut changes = Vec::new();
                    while let Some(directory) = pending.pop() {
                        let mut cursor = None;
                        loop {
                            let entries = snapshot.list_directory(&directory, &PageRequest::new(200,cursor)?, &operation).await?;
                            for entry in entries.items {
                                if entry.kind == EntryKind::Tree { pending.push(entry.path); }
                                else { changes.push(json!({"path":display_path(entry.path.as_bytes()),"path_hex":encode_hex(entry.path.as_bytes()),"kind":"Added","old":null,"new":entry_json(&entry)})); }
                            }
                            cursor = entries.next;
                            if cursor.is_none() { break; }
                        }
                    }
                    json!({"base":null,"changes":changes})
                }
            }
            Action::Blame => {
                let blame = snapshot.shared_blame(&path, &operation).await?;
                return Ok(Payload::Blame(blame, repository.generation()));
            }
        };
        value["commit"] = json!(snapshot.commit_oid().to_string());
        value["generation"] = json!(repository.generation());
        Ok(Payload::Json(value))
    }.await;
    operation.finish(result).await
}

async fn comparison_side(
    snapshot: &RemoteGitSnapshot,
    path: &GitPath,
    operation: &crab_remote_git::OperationContext,
) -> crab_remote_git::Result<(Option<TreeEntry>, Option<Blob>)> {
    let Some(entry) = snapshot.entry(path, operation).await? else {
        return Ok((None, None));
    };
    let blob = if matches!(entry.kind, EntryKind::Blob | EntryKind::Symlink) {
        Some(snapshot.read_blob(path, operation).await?)
    } else {
        None
    };
    Ok((Some(entry), blob))
}

pub(super) fn revision(
    repository: &RemoteGitRepository,
    requested: Option<&str>,
) -> crab_remote_git::Result<Revision> {
    if let Some(value) = requested {
        return Revision::parse(value);
    }
    let refs = repository.refs();
    let head = refs.head.as_ref().ok_or(if refs.is_empty() {
        Error::EmptyRepository
    } else {
        Error::Revision {
            reason: RevisionError::NotFound,
        }
    })?;
    Ok(Revision::Reference(head.name.clone()))
}

fn comparison_change(change: &TreeChange, view_identity: bool) -> serde_json::Value {
    let path = change.path.as_bytes();
    let mut value = json!({
        "path": display_path(path), "path_hex": encode_hex(path),
        "kind": format!("{:?}", change.kind),
        "old": change.old.as_ref().map(entry_json), "new": change.new.as_ref().map(entry_json),
    });
    if view_identity {
        let mut path_id = blake3::Hasher::new_derive_key("crab review path identity v1");
        path_id.update(path);
        let mut revision = blake3::Hasher::new_derive_key("crab review diff identity v1");
        for entry in [&change.old, &change.new] {
            if let Some(entry) = entry {
                revision.update(&[1]);
                revision.update(&entry.mode.raw().to_be_bytes());
                revision.update(entry.oid.as_bytes());
            } else {
                revision.update(&[0]);
            }
        }
        // Keep viewed progress when other files or the branch tip change.
        value["view_id"] = json!(format!(
            "{}{}",
            path_id.finalize().to_hex(),
            revision.finalize().to_hex()
        ));
    }
    value
}

fn comparison_json(
    comparison: crab_remote_git::Comparison,
    base_tip: Option<String>,
) -> serde_json::Value {
    let changes = comparison
        .changes
        .iter()
        .map(|change| comparison_change(change, base_tip.is_some()))
        .collect::<Vec<_>>();
    let mut result = json!({"base":comparison.base.to_string(), "changes":changes});
    if let Some(base_tip) = base_tip {
        result["base_tip"] = json!(base_tip);
    }
    result
}

pub(super) async fn fork_comparison(
    fork: &RemoteGitRepository,
    source: &RemoteGitRepository,
    params: &Parameters,
    cancel: &CancellationToken,
) -> crab_remote_git::Result<serde_json::Value> {
    let operation = fork.operation(OperationKind::Compare, cancel).await?;
    let result = async {
        let head = fork
            .snapshot(&revision(fork, params.rev.as_deref())?, &operation)
            .await?;
        let base_revision = revision(source, params.base.as_deref())?;
        let (ancestor, base_tip) =
            fork_ancestry(source, &base_revision, &head, &operation, cancel).await?;
        // Both sides contain the merge base. Keep subsequent file reads in
        // the fork so exact per-file diff URLs reuse the usual access checks.
        let base = fork
            .snapshot(&Revision::Commit(ancestor), &operation)
            .await?;
        let comparison = head.compare(&base, &operation).await?;
        let mut result = comparison_json(comparison, Some(base_tip.to_string()));
        result["commit"] = json!(head.commit_oid().to_string());
        result["generation"] = json!(fork.generation());
        Ok(result)
    }
    .await;
    operation.finish(result).await
}

pub(super) async fn fork_commits(
    fork: &RemoteGitRepository,
    source: &RemoteGitRepository,
    params: &Parameters,
    page: &PageRequest,
    cancel: &CancellationToken,
) -> crab_remote_git::Result<crab_remote_git::Page<crab_remote_git::Commit>> {
    let operation = fork.operation(OperationKind::History, cancel).await?;
    let result = async {
        let head = fork
            .snapshot(&revision(fork, params.rev.as_deref())?, &operation)
            .await?;
        let upstream = source.operation(OperationKind::History, cancel).await?;
        let result = async {
            let base = source
                .snapshot(&revision(source, params.base.as_deref())?, &upstream)
                .await?;
            head.ahead_history_across_captures(&base, page, &operation, &upstream)
                .await
        }
        .await;
        upstream.finish(result).await
    }
    .await;
    operation.finish(result).await
}

pub(super) async fn fork_ancestry(
    upstream: &RemoteGitRepository,
    base: &Revision,
    head: &RemoteGitSnapshot,
    head_operation: &crab_remote_git::OperationContext,
    cancel: &CancellationToken,
) -> crab_remote_git::Result<(gix_hash::ObjectId, gix_hash::ObjectId)> {
    let operation = upstream.operation(OperationKind::Compare, cancel).await?;
    let result = async {
        let base = upstream.snapshot(base, &operation).await?;
        let ancestor = comparison_ancestor(head, &base, head_operation, &operation).await?;
        Ok((ancestor, base.commit_oid()))
    }
    .await;
    operation.finish(result).await
}

pub(super) async fn comparison_ancestor(
    head: &RemoteGitSnapshot,
    base: &RemoteGitSnapshot,
    head_operation: &crab_remote_git::OperationContext,
    base_operation: &crab_remote_git::OperationContext,
) -> crab_remote_git::Result<gix_hash::ObjectId> {
    first_merge_base(
        head.merge_bases_across_captures(base, head_operation, base_operation)
            .await?,
    )
}

fn first_merge_base(
    ancestors: Vec<gix_hash::ObjectId>,
) -> crab_remote_git::Result<gix_hash::ObjectId> {
    ancestors.into_iter().next().ok_or(Error::Revision {
        reason: RevisionError::NoMergeBase,
    })
}

pub(crate) struct ComparisonBlobs {
    pub old: Option<Blob>,
    pub new: Option<Blob>,
    pub changed: bool,
}

pub(super) async fn merge_base_snapshot(
    repository: &RemoteGitRepository,
    base: &RemoteGitSnapshot,
    head: &RemoteGitSnapshot,
    operation: &crab_remote_git::OperationContext,
) -> crab_remote_git::Result<RemoteGitSnapshot> {
    let ancestor = comparison_ancestor(head, base, operation, operation).await?;
    if ancestor == base.commit_oid() {
        return Ok(base.clone());
    }
    repository
        .snapshot(&Revision::Commit(ancestor), operation)
        .await
}

pub(super) async fn pull_comparison_base(
    repository: &RemoteGitRepository,
    fork: Option<&RemoteGitRepository>,
    base: &str,
    head: &str,
    cancellation: &CancellationToken,
) -> crab_remote_git::Result<String> {
    let head_repository = fork.unwrap_or(repository);
    let operation = head_repository
        .operation(OperationKind::Compare, cancellation)
        .await?;
    let result = async {
        let head = head_repository
            .snapshot(&Revision::parse(head)?, &operation)
            .await?;
        let base = Revision::parse(base)?;
        if fork.is_some() {
            let (ancestor, _) =
                fork_ancestry(repository, &base, &head, &operation, cancellation).await?;
            return Ok(ancestor.to_string());
        }
        let base = repository.snapshot(&base, &operation).await?;
        Ok(merge_base_snapshot(repository, &base, &head, &operation)
            .await?
            .commit_oid()
            .to_string())
    }
    .await;
    operation.finish(result).await
}

async fn comparison_blobs_for_snapshots(
    base: Option<&RemoteGitSnapshot>,
    head: &RemoteGitSnapshot,
    path: &GitPath,
    operation: &crab_remote_git::OperationContext,
) -> crab_remote_git::Result<ComparisonBlobs> {
    let (old_entry, old) = match base {
        Some(base) => comparison_side(base, path, operation).await?,
        None => (None, None),
    };
    let (new_entry, new) = comparison_side(head, path, operation).await?;
    // A comparison emits changed leaves and type replacements. A directory
    // that remains a directory only contributes changes for its descendants.
    let changed = match (old_entry, new_entry) {
        (Some(old), Some(new)) => {
            (old.kind != EntryKind::Tree || new.kind != EntryKind::Tree)
                && (old.oid != new.oid || old.mode != new.mode || old.kind != new.kind)
        }
        (Some(entry), None) | (None, Some(entry)) => entry.kind != EntryKind::Tree,
        (None, None) => false,
    };
    Ok(ComparisonBlobs { old, new, changed })
}

pub(crate) async fn comparison_blobs(
    repository: &RemoteGitRepository,
    base_oid: &str,
    head_oid: &str,
    path: &GitPath,
    cancellation: &CancellationToken,
) -> crab_remote_git::Result<ComparisonBlobs> {
    let operation = repository
        .operation(OperationKind::Diff, cancellation)
        .await?;
    let result = async {
        let base = repository
            .snapshot(&Revision::parse(base_oid)?, &operation)
            .await?;
        let head = repository
            .snapshot(&Revision::parse(head_oid)?, &operation)
            .await?;
        comparison_blobs_for_snapshots(Some(&base), &head, path, &operation).await
    }
    .await;
    operation.finish(result).await
}

pub(super) fn search_score(path: &str, query: &str) -> Option<(u8, usize)> {
    let path = path.to_lowercase();
    let query = query.to_lowercase();
    let name = path.rsplit('/').next().unwrap_or(&path);
    if name == query {
        return Some((0, path.len()));
    }
    if name.starts_with(&query) {
        return Some((1, name.len().saturating_sub(query.len())));
    }
    if path.starts_with(&query) {
        return Some((2, path.len().saturating_sub(query.len())));
    }
    if let Some(position) = name.find(&query) {
        return Some((3, position.saturating_add(name.len())));
    }
    if let Some(position) = path.find(&query) {
        return Some((4, position.saturating_add(path.len())));
    }
    fuzzy_cost(&path, &query).map(|cost| (5, cost.saturating_add(path.len())))
}

fn fuzzy_cost(path: &str, query: &str) -> Option<usize> {
    let mut cursor = 0;
    let mut cost = 0usize;
    for character in query.chars() {
        let offset = path[cursor..].find(character)?;
        cost = cost.saturating_add(offset);
        cursor = cursor
            .saturating_add(offset)
            .saturating_add(character.len_utf8());
    }
    Some(cost)
}
