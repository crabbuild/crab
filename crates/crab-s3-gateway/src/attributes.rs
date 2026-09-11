use std::{collections::BTreeMap, str::FromStr as _};

use bytes::Bytes;
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};

use crate::gateway::Repository;

const VERSION: u32 = 2;
const LEGACY_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DELTA_DEPTH: usize = 1_000_000;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Checksums {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) crc32: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) crc32c: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) crc64nvme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sha1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checksum_type: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PartAttributes {
    pub(crate) number: i32,
    pub(crate) size: u64,
    #[serde(default, skip_serializing_if = "Checksums::is_empty")]
    pub(crate) checksums: Checksums,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PutAttributes {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag_override: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) completion_upload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) logical_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Checksums::is_empty")]
    pub(crate) checksums: Checksums,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) tags: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) parts: Vec<PartAttributes>,
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
    #[serde(default, skip_serializing_if = "Checksums::is_empty")]
    pub(crate) checksums: Checksums,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) tags: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) parts: Vec<PartAttributes>,
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
            checksums: pending.checksums,
            tags: pending.tags,
            parts: pending.parts,
            cache_control: pending.cache_control,
            content_disposition: pending.content_disposition,
            content_encoding: pending.content_encoding,
            content_language: pending.content_language,
            content_type: pending.content_type,
            expires: pending.expires,
            metadata: pending.metadata,
        }
    }

    pub(crate) fn matches_pending(&self, pending: &PutAttributes, etag: &str, size: u64) -> bool {
        self.etag == etag
            && self.size == size
            && self.completion_upload_id == pending.completion_upload_id
            && self.checksums == pending.checksums
            && self.tags == pending.tags
            && self.parts == pending.parts
            && self.cache_control == pending.cache_control
            && self.content_disposition == pending.content_disposition
            && self.content_encoding == pending.content_encoding
            && self.content_language == pending.content_language
            && self.content_type == pending.content_type
            && self.expires == pending.expires
            && self.metadata == pending.metadata
    }
}

impl Checksums {
    pub(crate) fn is_empty(&self) -> bool {
        self.crc32.is_none()
            && self.crc32c.is_none()
            && self.crc64nvme.is_none()
            && self.sha1.is_none()
            && self.sha256.is_none()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Manifest {
    objects: BTreeMap<String, ObjectAttributes>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyManifest {
    version: u32,
    objects: BTreeMap<String, ObjectAttributes>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delta {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    changes: BTreeMap<String, Option<ObjectAttributes>>,
}

impl Manifest {
    pub(crate) fn object(&self, path: &str, oid: ObjectId) -> Option<&ObjectAttributes> {
        self.objects
            .get(path)
            .filter(|attributes| attributes.blob_oid == oid.to_string())
    }

    fn apply(&mut self, changes: BTreeMap<String, Option<ObjectAttributes>>) {
        for (path, attributes) in changes {
            match attributes {
                Some(attributes) => {
                    self.objects.insert(path, attributes);
                }
                None => {
                    self.objects.remove(&path);
                }
            }
        }
    }
}

pub(crate) async fn load(repository: &Repository, commit: ObjectId) -> crate::Result<Manifest> {
    let mut current = Some(commit);
    let mut deltas = Vec::new();
    let mut manifest = Manifest::default();
    while let Some(commit) = current {
        if deltas.len() >= MAX_DELTA_DEPTH {
            return Err(crate::Error::Config("S3 attribute delta chain is too deep"));
        }
        match load_stored(repository, commit).await? {
            None => break,
            Some(Stored::Legacy(legacy)) => {
                manifest.objects = legacy.objects;
                break;
            }
            Some(Stored::Delta(delta)) => {
                current = parse_parent(delta.parent.as_deref())?;
                deltas.push(delta.changes);
            }
        }
    }
    for changes in deltas.into_iter().rev() {
        manifest.apply(changes);
    }
    Ok(manifest)
}

pub(crate) async fn load_object(
    repository: &Repository,
    commit: ObjectId,
    path: &str,
    oid: ObjectId,
) -> crate::Result<Option<ObjectAttributes>> {
    let mut current = Some(commit);
    for _ in 0..MAX_DELTA_DEPTH {
        let Some(commit) = current else {
            return Ok(None);
        };
        match load_stored(repository, commit).await? {
            None => return Ok(None),
            Some(Stored::Legacy(legacy)) => {
                return Ok(legacy
                    .objects
                    .get(path)
                    .filter(|attributes| attributes.blob_oid == oid.to_string())
                    .cloned());
            }
            Some(Stored::Delta(delta)) => {
                if let Some(attributes) = delta.changes.get(path) {
                    return Ok(attributes
                        .as_ref()
                        .filter(|attributes| attributes.blob_oid == oid.to_string())
                        .cloned());
                }
                current = parse_parent(delta.parent.as_deref())?;
            }
        }
    }
    Err(crate::Error::Config("S3 attribute delta chain is too deep"))
}

pub(crate) async fn load_objects(
    repository: &Repository,
    commit: ObjectId,
    objects: &[(String, ObjectId)],
) -> crate::Result<BTreeMap<String, ObjectAttributes>> {
    let mut unresolved = objects.iter().cloned().collect::<BTreeMap<_, _>>();
    let mut resolved = BTreeMap::new();
    let mut current = Some(commit);
    for _ in 0..MAX_DELTA_DEPTH {
        if unresolved.is_empty() {
            return Ok(resolved);
        }
        let Some(commit) = current else {
            return Ok(resolved);
        };
        match load_stored(repository, commit).await? {
            None => return Ok(resolved),
            Some(Stored::Legacy(legacy)) => {
                for (path, oid) in unresolved {
                    if let Some(attributes) = legacy
                        .objects
                        .get(&path)
                        .filter(|attributes| attributes.blob_oid == oid.to_string())
                    {
                        resolved.insert(path, attributes.clone());
                    }
                }
                return Ok(resolved);
            }
            Some(Stored::Delta(delta)) => {
                current = parse_parent(delta.parent.as_deref())?;
                for (path, attributes) in delta.changes {
                    let Some(oid) = unresolved.remove(&path) else {
                        continue;
                    };
                    if let Some(attributes) =
                        attributes.filter(|attributes| attributes.blob_oid == oid.to_string())
                    {
                        resolved.insert(path, attributes);
                    }
                }
            }
        }
    }
    Err(crate::Error::Config("S3 attribute delta chain is too deep"))
}

pub(crate) async fn save_delta(
    repository: &Repository,
    commit: ObjectId,
    parent: Option<ObjectId>,
    path: String,
    attributes: Option<ObjectAttributes>,
) -> crate::Result<()> {
    let target = repository
        .layout
        .repo_path(&format!("s3/attributes/{commit}.json"));
    let bytes = serde_json::to_vec(&Delta {
        version: VERSION,
        parent: parent.map(|oid| oid.to_string()),
        changes: BTreeMap::from([(path, attributes)]),
    })
    .map_err(|source| crate::Error::Attributes { source })?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(crate::Error::Config("S3 attribute delta exceeds 32 MiB"));
    }
    repository
        .store
        .put_exact(&target, Bytes::from(bytes))
        .await?;
    Ok(())
}

enum Stored {
    Legacy(LegacyManifest),
    Delta(Delta),
}

async fn load_stored(repository: &Repository, commit: ObjectId) -> crate::Result<Option<Stored>> {
    let path = repository
        .layout
        .repo_path(&format!("s3/attributes/{commit}.json"));
    let bytes = match repository
        .store
        .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
        .await
    {
        Ok((bytes, _)) => bytes,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let version = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|source| crate::Error::Attributes { source })?
        .get("version")
        .and_then(serde_json::Value::as_u64);
    match version {
        Some(value) if value == u64::from(VERSION) => serde_json::from_slice(&bytes)
            .map(Stored::Delta)
            .map_err(|source| crate::Error::Attributes { source }),
        // Version 1 is a persisted migration seam from the first gateway
        // release. New commits always write path-local version 2 deltas.
        Some(value) if value == u64::from(LEGACY_VERSION) => serde_json::from_slice(&bytes)
            .map(Stored::Legacy)
            .map_err(|source| crate::Error::Attributes { source }),
        _ => Err(crate::Error::Config(
            "unsupported S3 attribute manifest version",
        )),
    }
    .and_then(|stored| match &stored {
        Stored::Legacy(value) if value.version != LEGACY_VERSION => Err(crate::Error::Config(
            "unsupported S3 attribute manifest version",
        )),
        Stored::Delta(value) if value.version != VERSION => Err(crate::Error::Config(
            "unsupported S3 attribute manifest version",
        )),
        Stored::Legacy(_) | Stored::Delta(_) => Ok(Some(stored)),
    })
}

fn parse_parent(value: Option<&str>) -> crate::Result<Option<ObjectId>> {
    value
        .map(ObjectId::from_str)
        .transpose()
        .map_err(|_| crate::Error::Config("S3 attribute delta parent is corrupt"))
}
