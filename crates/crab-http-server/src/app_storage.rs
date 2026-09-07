use std::time::{SystemTime, UNIX_EPOCH};

use crab_storage::{ETag, StorageError};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};

use crate::{
    app::{Error, Result},
    auth::Identity,
    server::Repository,
};

pub(crate) const MAX_NUMBER: u64 = 9_007_199_254_740_991;
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

pub(crate) fn same_author(left: &Identity, right: &Identity) -> bool {
    left.issuer == right.issuer && left.subject == right.subject
}

pub(crate) fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .map_err(|_| Error::Invalid("Clock exceeds the supported range"))
}

pub(crate) async fn read<T: DeserializeOwned>(
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

pub(crate) async fn create_or_read<T: DeserializeOwned + Serialize>(
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

pub(crate) async fn update<T: Serialize>(
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

pub(crate) async fn last_number(repo: &Repository, root: &str) -> Result<u64> {
    Ok(read::<Sequence>(repo, &format!("{root}/sequence.json"))
        .await?
        .map_or(0, |(value, _)| value.last))
}

pub(crate) async fn reserve_number(repo: &Repository, root: &str) -> Result<u64> {
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
