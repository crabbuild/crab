use axum::{extract::Query, http::header};
use serde::Deserialize;

use super::{
    super::{
        merged_comparison::{Capture, remote_response},
        storage::PullMerge,
    },
    *,
};

pub(super) async fn coverage(
    server: &Server,
    principal: &Principal,
    repo: &Repository,
    pull: &PullRequest,
    merge: &PullMerge,
) -> std::result::Result<Report, Response> {
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let capture = Capture::open(server, principal, repo, pull, &cancel)
        .await
        .map_err(IntoResponse::into_response)?;
    let ops = capture
        .operations(OperationKind::Compare, &cancel)
        .await
        .map_err(remote_response)?;
    let mut report = Report {
        base_oid: Some(merge.base_oid.clone()),
        head_oid: Some(merge.head_oid.clone()),
        ..Report::default()
    };
    let loaded = async {
        let (base, head) = capture.tips(&ops).await?;
        let Some(text) = report_policy(&mut report, load_policy(&base, ops.base()).await?) else {
            return Ok(None);
        };
        let ancestor = capture.ancestor(base, &head, &ops).await?;
        let changes = head.compare(&ancestor, &ops.head).await?.changes;
        Ok(Some((text, changes)))
    }
    .await;
    let loaded = ops.finish(loaded).await.map_err(remote_response)?;
    let (mut report, groups) = match_loaded(report, loaded, cancel)
        .await
        .map_err(IntoResponse::into_response)?;
    // The merge record pins the rules and changed paths. Current team and
    // reviewer grants cannot establish which approvals counted in the past.
    report.coverage(groups, None);
    capture
        .recheck(server, principal, repo)
        .await
        .map_err(IntoResponse::into_response)?;
    Ok(report)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceParameters {}

pub(super) async fn source(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(address): Path<(String, String, u64)>,
    Query(_): Query<SourceParameters>,
) -> std::result::Result<Response, Response> {
    let admission = read_admission::acquire_bulk(&server).map_err(|_| read_admission::busy())?;
    let (repo, pull) = load_pull(&server, &principal, address)
        .await
        .map_err(IntoResponse::into_response)?;
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let capture = Capture::open(&server, &principal, &repo, &pull, &cancel)
        .await
        .map_err(IntoResponse::into_response)?;
    let ops = capture
        .operations(OperationKind::Content, &cancel)
        .await
        .map_err(remote_response)?;
    let policy = async {
        let base = capture.base_snapshot(&ops).await?;
        load_policy(&base, ops.base()).await
    }
    .await;
    let policy = ops.finish(policy).await.map_err(remote_response)?;
    capture
        .recheck(&server, &principal, &repo)
        .await
        .map_err(IntoResponse::into_response)?;
    let policy = policy.ok_or_else(|| Error::NotFound.into_response())?;
    let text = policy
        .text
        .map_err(|problem| Error::Invalid(problem).into_response())?;
    let response = ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response();
    Ok(read_admission::retain(response, admission))
}
