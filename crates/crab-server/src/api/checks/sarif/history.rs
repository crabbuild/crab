use axum::routing::get;
use crab_remote_git::{CursorError, HistoryTraversal, OperationKind, PageRequest, Revision};

use super::*;
use crate::api::{
    branches::branch_ref,
    representation::{decode_cursor, encode_cursor},
};

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new().route(
        "/api/repos/{owner}/{name}/code-scanning/history",
        get(history),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameters {
    branch: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct History {
    branch: Option<String>,
    tip: Option<String>,
    tip_reports: usize,
    commits_examined: usize,
    items: Vec<Item>,
    next: Option<String>,
}

#[derive(Serialize)]
struct Item {
    oid: String,
    title: String,
    reports: usize,
}

async fn history(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(key): Path<(String, String)>,
    Query(input): Query<Parameters>,
) -> Result<Json<History>> {
    let repo = app::repository(&server, &principal, &key)?;
    if principal.is_public_reader(repo.id) {
        return Err(Error::NotFound);
    }
    app::actor(&principal)?;
    let limit = input.limit.unwrap_or(30);
    if !(1..=check_scans::HISTORY_PAGE_LIMIT).contains(&limit) {
        return Err(Error::Invalid(
            "Scan history limit must be between 1 and 50",
        ));
    }
    if let Some(branch) = &input.branch {
        let name = branch
            .strip_prefix("refs/heads/")
            .ok_or(Error::Invalid("Choose a full branch ref"))?;
        branch_ref(name).map_err(Error::Invalid)?;
    }
    let cancel = server.cancellation.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let git = repo.open_current(&server, server.options, &cancel).await?;
    let view = git.refs();
    let branch = input
        .branch
        .or_else(|| view.head.as_ref().map(|head| head.name.clone()));
    let Some(branch) = branch else {
        if input.cursor.is_some() {
            return Err(Error::PageChanged);
        }
        return Ok(Json(History {
            branch: None,
            tip: None,
            tip_reports: 0,
            commits_examined: 0,
            items: vec![],
            next: None,
        }));
    };
    if !view
        .entries
        .iter()
        .any(|entry| entry.name == branch && entry.name.starts_with("refs/heads/"))
    {
        return Err(if input.cursor.is_some() {
            Error::PageChanged
        } else {
            Error::NotFound
        });
    }
    let mut digest = blake3::Hasher::new_keyed(&server.cursor_key);
    digest.update(b"crab.scan-history.v1\0");
    digest.update(branch.as_bytes());
    let cursor_key = *digest.finalize().as_bytes();
    let cursor = input
        .cursor
        .as_deref()
        .map(|value| {
            decode_cursor(&cursor_key, repo.id, value)
                .map_err(|_| Error::Invalid("Invalid scan history cursor for this branch"))
        })
        .transpose()?;
    let page = PageRequest::new(limit, cursor).map_err(crate::Error::from)?;
    let operation = git
        .operation(OperationKind::History, &cancel)
        .await
        .map_err(crate::Error::from)?;
    let result = async {
        let snapshot = git.snapshot(&Revision::parse(&branch)?, &operation).await?;
        let commits = snapshot
            .history(HistoryTraversal::FirstParent, &page, &operation)
            .await?;
        Ok((snapshot.commit_oid().to_string(), commits))
    }
    .await;
    let (tip, history) = match operation.finish(result).await {
        Ok(result) => result,
        Err(crab_remote_git::Error::InvalidCursor {
            reason: CursorError::ContextMismatch,
        }) => {
            return Err(Error::PageChanged);
        }
        Err(error) => return Err(crate::Error::from(error).into()),
    };
    let mut oids = Vec::with_capacity(history.items.len() + 1);
    oids.push(tip.clone());
    oids.extend(history.items.iter().map(|commit| commit.oid.to_string()));
    let routed = app::repository_cell(&server, &repo)?;
    let counts = app::query_output(
        routed
            .client
            .query::<check_scans::CountByCommit>(&routed.target, None, oids)
            .await,
    )?;
    let [tip_reports, reported @ ..] = counts.as_slice() else {
        return Err(Error::CellContract(
            "Scan history is missing the branch tip",
        ));
    };
    if reported.len() != history.items.len() {
        return Err(Error::CellContract(
            "Scan history does not match the commit window",
        ));
    }
    let items = history
        .items
        .iter()
        .zip(reported)
        .filter(|(_, reports)| **reports > 0)
        .map(|(commit, reports)| Item {
            oid: commit.oid.to_string(),
            title: String::from_utf8_lossy(&commit.message)
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            reports: *reports,
        })
        .collect();
    app::membership(&server, &principal, &repo).await?;
    Ok(Json(History {
        branch: Some(branch),
        tip: Some(tip),
        tip_reports: *tip_reports,
        commits_examined: history.items.len(),
        items,
        next: history
            .next
            .map(|cursor| encode_cursor(&cursor_key, repo.id, cursor)),
    }))
}
