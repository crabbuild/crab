use serde::{Deserialize, Serialize};

use crate::{
    app::{Error, Result},
    app_storage,
    auth::Identity,
    server::Repository,
};

pub(super) const ROOT: &str = "app/v1/issues";
pub(super) use app_storage::MAX_NUMBER;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(super) enum IssueState {
    #[default]
    Open,
    Closed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Issue {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub title: String,
    pub body: String,
    pub state: IssueState,
    #[serde(default)]
    pub label_ids: Vec<u64>,
    #[serde(default)]
    pub assignee_subjects: Vec<String>,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Comment {
    pub number: u64,
    pub request_id: String,
    pub author: Identity,
    pub body: String,
    pub version: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

pub(super) use app_storage::{now, read, same_author, update};

pub(super) fn issue_path(number: u64) -> String {
    format!("{ROOT}/{number:016}/issue.json")
}
pub(super) fn comments_root(number: u64) -> String {
    format!("{ROOT}/{number:016}/comments")
}
pub(super) fn comment_path(issue: u64, comment: u64) -> String {
    format!("{}/{comment:016}.json", comments_root(issue))
}

pub(super) async fn create_issue(
    repo: &Repository,
    author: Identity,
    request_id: String,
    title: String,
    body: String,
) -> Result<Issue> {
    let reservation = format!("{ROOT}/requests/{request_id}.json");
    let original = match app_storage::read::<Issue>(repo, &reservation).await? {
        Some((issue, _)) => issue,
        None => {
            let number = app_storage::reserve_number(repo, ROOT).await?;
            let timestamp = app_storage::now()?;
            app_storage::create_or_read(
                repo,
                &reservation,
                Issue {
                    number,
                    request_id: request_id.clone(),
                    author: author.clone(),
                    title: title.clone(),
                    body: body.clone(),
                    state: IssueState::Open,
                    label_ids: vec![],
                    assignee_subjects: vec![],
                    version: 1,
                    created_at: timestamp,
                    updated_at: timestamp,
                },
            )
            .await?
        }
    };
    if !app_storage::same_author(&original.author, &author)
        || original.title != title
        || original.body != body
    {
        return Err(Error::RequestConflict);
    }
    // The immutable reservation makes retries converge on one number after response loss.
    // Number gaps are allowed; a reservation alone is never a visible issue.
    let current = app_storage::create_or_read(repo, &issue_path(original.number), original).await?;
    if current.request_id != request_id {
        return Err(Error::Conflict);
    }
    Ok(current)
}

pub(super) async fn create_comment(
    repo: &Repository,
    issue: u64,
    author: Identity,
    request_id: String,
    body: String,
) -> Result<Comment> {
    let root = comments_root(issue);
    let reservation = format!("{root}/requests/{request_id}.json");
    let original = match app_storage::read::<Comment>(repo, &reservation).await? {
        Some((comment, _)) => comment,
        None => {
            let number = app_storage::reserve_number(repo, &root).await?;
            let timestamp = app_storage::now()?;
            app_storage::create_or_read(
                repo,
                &reservation,
                Comment {
                    number,
                    request_id: request_id.clone(),
                    author: author.clone(),
                    body: body.clone(),
                    version: 1,
                    created_at: timestamp,
                    updated_at: timestamp,
                },
            )
            .await?
        }
    };
    if !app_storage::same_author(&original.author, &author) || original.body != body {
        return Err(Error::RequestConflict);
    }
    let current =
        app_storage::create_or_read(repo, &comment_path(issue, original.number), original).await?;
    if current.request_id != request_id {
        return Err(Error::Conflict);
    }
    Ok(current)
}
