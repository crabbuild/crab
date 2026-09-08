use std::collections::BTreeMap;

use bytes::Bytes;
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};

use crate::gateway::Repository;

const VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutAttributes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completion_upload_id: Option<String>,
    pub(crate) cache_control: Option<String>,
    pub(crate) content_disposition: Option<String>,
    pub(crate) content_encoding: Option<String>,
    pub(crate) content_language: Option<String>,
    pub(crate) content_type: Option<String>,
    pub(crate) expires: Option<s3s::dto::Timestamp>,
    pub(crate) metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObjectAttributes {
    pub(crate) blob_oid: String,
    pub(crate) etag: String,
    pub(crate) size: u64,
    pub(crate) modified_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completion_upload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_control: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_disposition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires: Option<s3s::dto::Timestamp>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) metadata: BTreeMap<String, String>,
}

impl ObjectAttributes {
    pub(crate) fn new(
        blob_oid: ObjectId,
        etag: String,
        size: u64,
        modified_seconds: u64,
        pending: PutAttributes,
    ) -> Self {
        Self {
            blob_oid: blob_oid.to_string(),
            etag,
            size,
            modified_seconds,
            completion_upload_id: pending.completion_upload_id,
            cache_control: pending.cache_control,
            content_disposition: pending.content_disposition,
            content_encoding: pending.content_encoding,
            content_language: pending.content_language,
            content_type: pending.content_type,
            expires: pending.expires,
            metadata: pending.metadata,
        }
    }

    pub(crate) fn matches_pending(&self, pending: &PutAttributes, etag: &str, size: usize) -> bool {
        self.etag == etag
            && usize::try_from(self.size).ok() == Some(size)
            && self.completion_upload_id == pending.completion_upload_id
            && self.cache_control == pending.cache_control
            && self.content_disposition == pending.content_disposition
            && self.content_encoding == pending.content_encoding
            && self.content_language == pending.content_language
            && self.content_type == pending.content_type
            && self.expires == pending.expires
            && self.metadata == pending.metadata
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    version: u32,
    objects: BTreeMap<String, ObjectAttributes>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            version: VERSION,
            objects: BTreeMap::new(),
        }
    }
}

impl Manifest {
    pub(crate) fn object(&self, path: &str, oid: ObjectId) -> Option<&ObjectAttributes> {
        self.objects
            .get(path)
            .filter(|attributes| attributes.blob_oid == oid.to_string())
    }

    pub(crate) fn put(&mut self, path: String, attributes: ObjectAttributes) {
        self.objects.insert(path, attributes);
    }

    pub(crate) fn remove(&mut self, path: &str) {
        self.objects.remove(path);
    }
}

pub(crate) async fn load(repository: &Repository, commit: ObjectId) -> crate::Result<Manifest> {
    let path = repository
        .layout
        .repo_path(&format!("s3/attributes/{commit}.json"));
    let bytes = match repository
        .store
        .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
        .await
    {
        Ok((bytes, _)) => bytes,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(Manifest::default()),
        Err(error) => return Err(error.into()),
    };
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|source| crate::Error::Attributes { source })?;
    if manifest.version != VERSION {
        return Err(crate::Error::Config(
            "unsupported S3 attribute manifest version",
        ));
    }
    Ok(manifest)
}

pub(crate) async fn save(
    repository: &Repository,
    commit: ObjectId,
    manifest: &Manifest,
) -> crate::Result<()> {
    let path = repository
        .layout
        .repo_path(&format!("s3/attributes/{commit}.json"));
    let bytes =
        serde_json::to_vec(manifest).map_err(|source| crate::Error::Attributes { source })?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(crate::Error::Config("S3 attribute manifest exceeds 32 MiB"));
    }
    repository
        .store
        .put_exact(&path, Bytes::from(bytes))
        .await?;
    Ok(())
}
