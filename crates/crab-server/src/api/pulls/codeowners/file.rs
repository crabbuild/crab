use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_remote_git::{EntryKind, GitPath, OperationKind, Revision, RevisionError};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::*;
use crate::api::{
    read_admission,
    representation::{decode_hex, encode_hex},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    rev: String,
    path_hex: String,
}

#[derive(Serialize)]
struct FileOwnership {
    commit: String,
    path_hex: String,
    source: Option<OwnershipRule>,
}

#[derive(Serialize)]
struct OwnershipRule {
    path: &'static str,
    path_hex: String,
    line: Option<usize>,
    owners: Vec<FileOwner>,
    problem: Option<&'static str>,
}

#[derive(Serialize)]
struct FileOwner {
    reference: String,
    eligible: bool,
}

pub(super) async fn show(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<Input>,
) -> std::result::Result<Response, Response> {
    let repo = app::repository(&server, &principal, &key).map_err(IntoResponse::into_response)?;
    let admission = read_admission::acquire_bulk(&server).map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1")],
            Json(json!({"error":{"message":"Code owner lookups are busy. Retry shortly."}})),
        )
            .into_response()
    })?;
    let report = inspect(&server, &repo, &principal, input)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(read_admission::retain(
        Json(report).into_response(),
        admission,
    ))
}

async fn inspect(
    server: &Server,
    repo: &Repository,
    principal: &Principal,
    input: Input,
) -> Result<FileOwnership> {
    let revision = match Revision::parse(&input.rev) {
        Ok(revision @ Revision::Commit(_)) => revision,
        _ => return Err(Error::Invalid("File ownership requires an exact commit ID")),
    };
    let bytes = decode_hex(&input.path_hex).map_err(|_| Error::Invalid("Invalid file path"))?;
    let path = GitPath::new(bytes).map_err(|_| Error::Invalid("Invalid file path"))?;
    if path.is_root() {
        return Err(Error::Invalid("File ownership requires a file path"));
    }
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let opened = repo.revalidate(server, &cancel).await?;
    let git = &opened.git;
    let operation = git
        .operation(OperationKind::Content, &cancel)
        .await
        .map_err(crate::Error::from)?;
    let loaded = async {
        let snapshot = git.snapshot(&revision, &operation).await?;
        let entry = snapshot.entry(&path, &operation).await?;
        if !entry.is_some_and(|entry| matches!(entry.kind, EntryKind::Blob | EntryKind::Symlink)) {
            return Err(crab_remote_git::Error::PathNotFound);
        }
        Ok((
            snapshot.commit_oid().to_string(),
            load_policy(&snapshot, &operation).await?,
        ))
    }
    .await;
    let (commit, policy) = operation
        .finish(loaded)
        .await
        .map_err(|error| match error {
            crab_remote_git::Error::PathNotFound
            | crab_remote_git::Error::PathComponentNotTree { .. }
            | crab_remote_git::Error::Revision {
                reason: RevisionError::NotFound | RevisionError::NotReachable,
            }
            | crab_remote_git::Error::SnapshotUnavailable => Error::NotFound,
            error => crate::Error::from(error).into(),
        })?;
    let mut report = FileOwnership {
        commit,
        path_hex: encode_hex(path.as_bytes()),
        source: None,
    };
    if let Some(policy) = policy {
        let file = policy.path;
        let rule = tokio::task::spawn_blocking(move || {
            let text = policy.text?;
            let rules = patterns::Rules::parse(&text)?;
            Ok(rules
                .matching(path.as_bytes())
                .and_then(|index| rules.rules.into_iter().nth(index)))
        })
        .await
        .map_err(crate::Error::Worker)?;
        report.source = match rule {
            Ok(Some(rule)) if !rule.owners.is_empty() => {
                let membership = app::membership(server, principal, repo).await?;
                let reviewers = membership.merge_reviewers(server).await?;
                let references = rule.owners.iter().map(String::as_str).collect();
                let resolved = resolve(
                    server,
                    repo,
                    &membership,
                    &reviewers,
                    &references,
                    &BTreeSet::new(),
                )
                .await?;
                let owners = rule
                    .owners
                    .into_iter()
                    .map(|reference| FileOwner {
                        eligible: resolved.contains_key(&reference),
                        reference: reference.chars().take(160).collect(),
                    })
                    .collect();
                Some(OwnershipRule {
                    path: file,
                    path_hex: encode_hex(file.as_bytes()),
                    line: Some(rule.line),
                    owners,
                    problem: None,
                })
            }
            Err(problem) => Some(OwnershipRule {
                path: file,
                path_hex: encode_hex(file.as_bytes()),
                line: None,
                owners: Vec::new(),
                problem: Some(problem),
            }),
            _ => None,
        };
    }
    app::membership(server, principal, repo).await?;
    Ok(report)
}
