use std::time::{SystemTime, UNIX_EPOCH};

use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};

use super::{Error, Result};
use crate::{auth::Identity, server::Repository};

pub(super) const ROOT: &str = "app/v1/issues";
pub(super) const MAX_NUMBER: u64 = 9_007_199_254_740_991;
const MAX_DOCUMENT_BYTES: u64 = 256 * 1024;

#[derive(Clone, Copy, Serialize)]
#[serde(transparent)]
struct Schema(u8);

impl Default for Schema {
    fn default() -> Self {
        Self(1)
    }
}

impl<'de> Deserialize<'de> for Schema {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        if u8::deserialize(deserializer)? != 1 {
            return Err(serde::de::Error::custom("unsupported collaboration schema"));
        }
        Ok(Self(1))
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Document<T> {
    schema_version: Schema,
    data: T,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Sequence {
    #[serde(deserialize_with = "safe_number")]
    last: u64,
}

fn safe_number<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<u64, D::Error> {
    let value = u64::deserialize(deserializer)?;
    if value > MAX_NUMBER {
        return Err(serde::de::Error::custom("number exceeds supported range"));
    }
    Ok(value)
}

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

pub(super) fn same_author(left: &Identity, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

pub(super) fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .map_err(|_| Error::Invalid("Clock exceeds the supported range"))
}

pub(super) fn issue_path(number: u64) -> String {
    format!("{ROOT}/{number:016}/issue.json")
}
pub(super) fn comments_root(number: u64) -> String {
    format!("{ROOT}/{number:016}/comments")
}
pub(super) fn comment_path(issue: u64, comment: u64) -> String {
    format!("{}/{comment:016}.json", comments_root(issue))
}

pub(super) async fn read<T: DeserializeOwned>(
    repo: &Repository,
    relative: &str,
) -> Result<Option<(T, ETag)>> {
    match repo
        .store
        .get_with_etag_bounded(&repo.layout.repo_path(relative), MAX_DOCUMENT_BYTES)
        .await
    {
        Ok((body, etag)) => {
            let document: Document<T> = serde_json::from_slice(&body)?;
            Ok(Some((document.data, etag)))
        }
        Err(StorageError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn encode<T: Serialize>(data: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&Document {
        schema_version: Schema::default(),
        data,
    })?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return Err(Error::Invalid("Content is too large"));
    }
    Ok(bytes)
}

async fn create_or_read<T: DeserializeOwned + Serialize>(
    repo: &Repository,
    relative: &str,
    data: T,
) -> Result<T> {
    let bytes = encode(&data)?;
    match repo
        .store
        .create_strict(&repo.layout.repo_path(relative), bytes.into())
        .await
    {
        Ok(()) => Ok(data),
        Err(StorageError::StateConflict { .. }) => read(repo, relative)
            .await?
            .map(|(value, _)| value)
            .ok_or(Error::Conflict),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn update<T: Serialize>(
    repo: &Repository,
    relative: &str,
    value: &T,
    etag: ETag,
) -> Result<()> {
    repo.store
        .update(
            &repo.layout.repo_path(relative),
            encode(value)?.into(),
            etag,
        )
        .await?;
    Ok(())
}

pub(super) async fn last_number(repo: &Repository, root: &str) -> Result<u64> {
    Ok(read::<Sequence>(repo, &format!("{root}/sequence.json"))
        .await?
        .map_or(0, |(value, _)| value.last))
}

async fn reserve_number(repo: &Repository, root: &str) -> Result<u64> {
    let path = repo.layout.repo_path(&format!("{root}/sequence.json"));
    let value = crab_storage::cas_update_bounded::<Document<Sequence>, _>(
        &repo.store,
        path.as_ref(),
        10,
        1024,
        |value| value.data.last = value.data.last.saturating_add(1).min(MAX_NUMBER),
    )
    .await?;
    if value.data.last == MAX_NUMBER {
        return Err(Error::Invalid("Repository numbering is exhausted"));
    }
    Ok(value.data.last)
}

pub(super) async fn create_issue(
    repo: &Repository,
    author: Identity,
    request_id: String,
    title: String,
    body: String,
) -> Result<Issue> {
    let reservation = format!("{ROOT}/requests/{request_id}.json");
    let original = match read::<Issue>(repo, &reservation).await? {
        Some((issue, _)) => issue,
        None => {
            let number = reserve_number(repo, ROOT).await?;
            let timestamp = now()?;
            create_or_read(
                repo,
                &reservation,
                Issue {
                    number,
                    request_id: request_id.clone(),
                    author: author.clone(),
                    title: title.clone(),
                    body: body.clone(),
                    state: IssueState::Open,
                    version: 1,
                    created_at: timestamp,
                    updated_at: timestamp,
                },
            )
            .await?
        }
    };
    if !same_author(&original.author, &author) || original.title != title || original.body != body {
        return Err(Error::RequestConflict);
    }
    // The immutable reservation makes retries converge on one number after response loss.
    // Number gaps are allowed; a reservation alone is never a visible issue.
    let current = create_or_read(repo, &issue_path(original.number), original).await?;
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
    let original = match read::<Comment>(repo, &reservation).await? {
        Some((comment, _)) => comment,
        None => {
            let number = reserve_number(repo, &root).await?;
            let timestamp = now()?;
            create_or_read(
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
    if !same_author(&original.author, &author) || original.body != body {
        return Err(Error::RequestConflict);
    }
    let current = create_or_read(repo, &comment_path(issue, original.number), original).await?;
    if current.request_id != request_id {
        return Err(Error::Conflict);
    }
    Ok(current)
}
