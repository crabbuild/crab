use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::{RepoIdParams, State, cancellable_read, ensure_desktop_layout, find_active_op};
use crate::repository::{BrowseSession, ReadScope};
use crate::{AgentError, Result};

#[derive(Deserialize)]
pub(super) struct OpenLocalParams {
    repo_id: String,
    path: PathBuf,
    op_id: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct OpenUrlParams {
    repo_id: String,
    url: String,
    op_id: Option<String>,
}

enum Location {
    Local(PathBuf),
    Url(String),
}

pub(super) async fn open_local(state: &State, params: OpenLocalParams) -> Result<Value> {
    open(
        state,
        params.repo_id,
        params.op_id,
        Location::Local(params.path),
    )
    .await
}

pub(super) async fn open_url(state: &State, params: OpenUrlParams) -> Result<Value> {
    open(
        state,
        params.repo_id,
        params.op_id,
        Location::Url(params.url),
    )
    .await
}

async fn open(
    state: &State,
    repo_id: String,
    op_id: Option<String>,
    location: Location,
) -> Result<Value> {
    let _lifecycle = state.repository_lifecycle(&repo_id).await;
    let scope = ReadScope::default();
    // The prepared session remains owned here until commit or explicit close.
    // A cancelled read result may be discarded; an SDK client cannot be.
    let mut prepared = None;
    let work = async {
        scope.check()?;
        state.remove(&repo_id).await?;
        scope.check()?;
        prepared = Some(match &location {
            Location::Local(path) => BrowseSession::open_local(path, &scope).await?,
            Location::Url(url) => BrowseSession::open_url(url, &scope).await?,
        });
        let repo = prepared.as_ref().ok_or_else(|| {
            AgentError::Other("repository preparation did not produce a session".into())
        })?;
        let mut response = json!({
            "repo_id": repo_id, "url": repo.url,
            "is_remote": repo.work_dir.is_none(), "cache_dir": "",
        });
        if let Location::Local(_) = &location {
            scope.check()?;
            // Discovery resolves Git's worktree root. Desktop writes and resume
            // lookup must not start in a caller's selected subdirectory.
            let work_dir = repo.work_dir.as_deref().ok_or_else(|| {
                AgentError::Other("local repository has no working directory".into())
            })?;
            ensure_desktop_layout(work_dir).await;
            let detected = crate::forge::detect::detect_and_persist(work_dir, &scope).await;
            response["work_dir"] = json!(work_dir);
            response["active_op"] = find_active_op(work_dir);
            response["forge_provider"] = json!(detected.as_ref().map(|forge| forge.provider));
        }
        scope.check()?;
        Ok(response)
    };
    let result = cancellable_read(state, op_id, &scope, work).await;
    match result {
        Ok(response) => {
            let repo = prepared.ok_or_else(|| {
                AgentError::Other("repository preparation did not produce a session".into())
            })?;
            let work_dir = repo.work_dir.clone();
            state.insert(repo_id.clone(), repo, work_dir).await;
            info!(%repo_id, "opened repository");
            Ok(response)
        }
        Err(operation) => {
            if let Some(repo) = prepared
                && let Err(cleanup) = repo.close().await
            {
                return Err(AgentError::Cleanup {
                    operation: Box::new(operation),
                    cleanup: Box::new(cleanup),
                });
            }
            Err(operation)
        }
    }
}

pub(super) async fn close(state: &State, params: RepoIdParams) -> Result<Value> {
    let _lifecycle = state.repository_lifecycle(&params.repo_id).await;
    state.remove(&params.repo_id).await?;
    Ok(json!({ "closed": params.repo_id }))
}
