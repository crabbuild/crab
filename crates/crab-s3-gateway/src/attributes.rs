use std::{
    collections::BTreeMap,
    ops::Bound::{Excluded, Included, Unbounded},
    str::FromStr as _,
};

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ListingAttributes {
    pub(crate) etag: String,
    pub(crate) size: u64,
    pub(crate) modified_seconds: u64,
}

impl From<&ObjectAttributes> for ListingAttributes {
    fn from(attributes: &ObjectAttributes) -> Self {
        Self {
            etag: attributes.etag.clone(),
            size: attributes.size,
            modified_seconds: attributes.modified_seconds,
        }
    }
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
    estimated_bytes: usize,
    complete: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyManifest {
    version: u32,
    objects: BTreeMap<String, ObjectAttributes>,
}

#[derive(Serialize)]
struct CheckpointPayload<'a> {
    version: u32,
    commit: String,
    complete: bool,
    objects: &'a BTreeMap<String, ObjectAttributes>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint {
    version: u32,
    commit: String,
    #[serde(default)]
    complete: bool,
    objects: BTreeMap<String, ObjectAttributes>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManifestListingItem {
    Object {
        path: String,
        attributes: ListingAttributes,
    },
    CommonPrefix(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestListingPage {
    pub(crate) items: Vec<ManifestListingItem>,
    pub(crate) has_more: bool,
}

pub(crate) struct PreparedCheckpoint {
    slot: String,
    bytes: Bytes,
}

impl PreparedCheckpoint {
    pub(crate) fn slot(&self) -> &str {
        &self.slot
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Delta {
    version: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    checkpoint: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint_slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    changes: BTreeMap<String, Option<ObjectAttributes>>,
}

impl Manifest {
    fn from_objects(objects: BTreeMap<String, ObjectAttributes>, complete: bool) -> Self {
        let estimated_bytes = objects.iter().fold(0usize, |total, (path, attributes)| {
            total.saturating_add(estimated_object_bytes(path, attributes))
        });
        Self {
            objects,
            estimated_bytes,
            complete,
        }
    }

    pub(crate) fn empty_complete() -> Self {
        Self {
            complete: true,
            ..Self::default()
        }
    }

    pub(crate) fn object(&self, path: &str, oid: ObjectId) -> Option<&ObjectAttributes> {
        self.objects
            .get(path)
            .filter(|attributes| attributes.blob_oid == oid.to_string())
    }

    pub(crate) fn update(&mut self, path: String, attributes: Option<ObjectAttributes>) {
        self.apply(BTreeMap::from([(path, attributes)]));
    }

    pub(crate) fn listing(
        &self,
        objects: &[(String, ObjectId)],
    ) -> BTreeMap<String, ListingAttributes> {
        objects
            .iter()
            .filter_map(|(path, oid)| {
                self.object(path, *oid)
                    .map(|attributes| (path.clone(), attributes.into()))
            })
            .collect()
    }

    pub(crate) fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.complete
    }

    /// Page the durable S3 namespace only when it covers the complete Git tree.
    pub(crate) fn complete_listing(
        &self,
        prefix: &str,
        after: Option<&str>,
        delimiter: Option<u8>,
        limit: usize,
    ) -> Option<ManifestListingPage> {
        if !self.complete {
            return None;
        }
        let bounds = match after {
            Some(after) => (Excluded(after.to_owned()), Unbounded),
            None => (Included(prefix.to_owned()), Unbounded),
        };
        let mut items = Vec::with_capacity(limit.saturating_add(1));
        let mut last_prefix = None;
        for (path, attributes) in self.objects.range(bounds) {
            if !path.starts_with(prefix) {
                if path.as_str() > prefix {
                    break;
                }
                continue;
            }
            if !crate::namespace::listable_path(path.as_bytes()) {
                continue;
            }
            if delimiter == Some(b'/')
                && let Some(offset) = path[prefix.len()..].find('/')
            {
                let common = &path[..prefix.len() + offset + 1];
                if after.is_some_and(|after| common <= after)
                    || last_prefix.as_deref() == Some(common)
                {
                    continue;
                }
                last_prefix = Some(common.to_owned());
                items.push(ManifestListingItem::CommonPrefix(common.to_owned()));
            } else {
                last_prefix = None;
                items.push(ManifestListingItem::Object {
                    path: path.clone(),
                    attributes: attributes.into(),
                });
            }
            if items.len() > limit {
                break;
            }
        }
        let has_more = items.len() > limit;
        items.truncate(limit);
        Some(ManifestListingPage { items, has_more })
    }

    fn apply(&mut self, changes: BTreeMap<String, Option<ObjectAttributes>>) {
        for (path, attributes) in changes {
            match attributes {
                Some(attributes) => {
                    let added = estimated_object_bytes(&path, &attributes);
                    if let Some(previous) = self.objects.insert(path.clone(), attributes) {
                        self.estimated_bytes = self
                            .estimated_bytes
                            .saturating_sub(estimated_object_bytes(&path, &previous));
                    }
                    self.estimated_bytes = self.estimated_bytes.saturating_add(added);
                }
                None => {
                    if let Some(previous) = self.objects.remove(&path) {
                        self.estimated_bytes = self
                            .estimated_bytes
                            .saturating_sub(estimated_object_bytes(&path, &previous));
                    }
                }
            }
        }
    }
}

fn estimated_object_bytes(path: &str, attributes: &ObjectAttributes) -> usize {
    path.len()
        .saturating_add(serde_json::to_vec(attributes).map_or(usize::MAX, |bytes| bytes.len()))
}

pub(crate) async fn load(repository: &Repository, commit: ObjectId) -> crate::Result<Manifest> {
    let mut current = Some(commit);
    let mut depth = 0;
    let mut changes = BTreeMap::new();
    let mut manifest = Manifest::default();
    while let Some(commit) = current {
        if depth >= MAX_DELTA_DEPTH {
            return Err(crate::Error::Config("S3 attribute delta chain is too deep"));
        }
        depth += 1;
        match load_stored(repository, commit).await? {
            None => break,
            Some(Stored::Legacy(legacy)) => {
                manifest = Manifest::from_objects(legacy.objects, false);
                break;
            }
            Some(Stored::Delta(delta)) => {
                if delta.checkpoint
                    && let Some(checkpoint) =
                        load_checkpoint(repository, commit, delta.checkpoint_slot.as_deref())
                            .await?
                {
                    manifest = checkpoint;
                    break;
                }
                current = parse_parent(delta.parent.as_deref())?;
                retain_newest_changes(&mut changes, delta.changes);
            }
        }
    }
    if current.is_none() {
        manifest.complete = true;
    }
    manifest.apply(changes);
    Ok(manifest)
}

fn retain_newest_changes(
    selected: &mut BTreeMap<String, Option<ObjectAttributes>>,
    changes: BTreeMap<String, Option<ObjectAttributes>>,
) {
    for (path, attributes) in changes {
        selected.entry(path).or_insert(attributes);
    }
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
                if delta.checkpoint
                    && let Some(checkpoint) =
                        load_checkpoint(repository, commit, delta.checkpoint_slot.as_deref())
                            .await?
                {
                    return Ok(checkpoint.object(path, oid).cloned());
                }
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
) -> crate::Result<BTreeMap<String, ListingAttributes>> {
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
                        resolved.insert(path, attributes.into());
                    }
                }
                return Ok(resolved);
            }
            Some(Stored::Delta(delta)) => {
                for (path, attributes) in delta.changes {
                    let Some(oid) = unresolved.remove(&path) else {
                        continue;
                    };
                    if let Some(attributes) =
                        attributes.filter(|attributes| attributes.blob_oid == oid.to_string())
                    {
                        resolved.insert(path, (&attributes).into());
                    }
                }
                if delta.checkpoint
                    && let Some(manifest) =
                        load_checkpoint(repository, commit, delta.checkpoint_slot.as_deref())
                            .await?
                {
                    for (path, oid) in unresolved {
                        if let Some(attributes) = manifest.object(&path, oid) {
                            resolved.insert(path, attributes.into());
                        }
                    }
                    return Ok(resolved);
                }
                current = parse_parent(delta.parent.as_deref())?;
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
    checkpoint: bool,
    checkpoint_slot: Option<String>,
) -> crate::Result<()> {
    let target = repository
        .layout
        .repo_path(&format!("s3/attributes/{commit}.json"));
    let bytes = serde_json::to_vec(&Delta {
        version: VERSION,
        checkpoint,
        checkpoint_slot,
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

pub(crate) fn prepare_checkpoint(
    branch: &str,
    commit: ObjectId,
    manifest: &Manifest,
) -> crate::Result<Option<PreparedCheckpoint>> {
    if u64::try_from(manifest.estimated_bytes()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(&CheckpointPayload {
        version: VERSION,
        commit: commit.to_string(),
        complete: manifest.complete,
        objects: &manifest.objects,
    })
    .map_err(|source| crate::Error::Attributes { source })?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Ok(None);
    }
    Ok(Some(PreparedCheckpoint {
        slot: blake3::hash(branch.as_bytes()).to_hex().to_string(),
        bytes: Bytes::from(bytes),
    }))
}

pub(crate) async fn publish_checkpoint(
    repository: &Repository,
    checkpoint: &PreparedCheckpoint,
) -> crate::Result<()> {
    repository
        .store
        .put_overwrite(
            &checkpoint_slot_path(repository, &checkpoint.slot)?,
            checkpoint.bytes.clone(),
        )
        .await?;
    Ok(())
}

async fn load_checkpoint(
    repository: &Repository,
    commit: ObjectId,
    slot: Option<&str>,
) -> crate::Result<Option<Manifest>> {
    let path = match slot {
        Some(slot) => checkpoint_slot_path(repository, slot)?,
        None => legacy_checkpoint_path(repository, commit),
    };
    let bytes = match repository
        .store
        .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
        .await
    {
        Ok((bytes, _)) => bytes,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let objects = match slot {
        Some(_) => {
            let stored = serde_json::from_slice::<StoredCheckpoint>(&bytes)
                .map_err(|source| crate::Error::Attributes { source })?;
            if !matches!(stored.version, LEGACY_VERSION | VERSION) {
                return Err(crate::Error::Config(
                    "unsupported S3 attribute checkpoint version",
                ));
            }
            if stored.commit != commit.to_string() {
                return Ok(None);
            }
            return Ok(Some(Manifest::from_objects(
                stored.objects,
                stored.version == VERSION && stored.complete,
            )));
        }
        None => {
            let stored = serde_json::from_slice::<LegacyManifest>(&bytes)
                .map_err(|source| crate::Error::Attributes { source })?;
            if stored.version != LEGACY_VERSION {
                return Err(crate::Error::Config(
                    "unsupported S3 attribute checkpoint version",
                ));
            }
            stored.objects
        }
    };
    Ok(Some(Manifest::from_objects(objects, false)))
}

fn checkpoint_slot_path(
    repository: &Repository,
    slot: &str,
) -> crate::Result<object_store::path::Path> {
    if slot.len() != blake3::OUT_LEN * 2
        || !slot
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(crate::Error::Config(
            "S3 attribute checkpoint slot is corrupt",
        ));
    }
    Ok(repository
        .layout
        .repo_path(&format!("s3/attribute-checkpoints/branches/{slot}.json")))
}

fn legacy_checkpoint_path(repository: &Repository, commit: ObjectId) -> object_store::path::Path {
    repository
        .layout
        .repo_path(&format!("s3/attribute-checkpoints/{commit}.json"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn attributes(etag: &str) -> ObjectAttributes {
        ObjectAttributes::new(
            ObjectId::empty_blob(gix_hash::Kind::Sha1),
            etag.to_owned(),
            0,
            0,
            PutAttributes::default(),
        )
    }

    #[test]
    fn newest_delta_change_wins_without_retaining_history() {
        let mut selected = BTreeMap::new();
        retain_newest_changes(
            &mut selected,
            BTreeMap::from([
                ("same".to_owned(), Some(attributes("new"))),
                ("deleted".to_owned(), None),
            ]),
        );
        retain_newest_changes(
            &mut selected,
            BTreeMap::from([
                ("same".to_owned(), Some(attributes("old"))),
                ("deleted".to_owned(), Some(attributes("old"))),
                ("older".to_owned(), Some(attributes("old"))),
            ]),
        );

        assert_eq!(
            selected["same"].as_ref().map(|value| value.etag.as_str()),
            Some("new")
        );
        assert!(selected["deleted"].is_none());
        assert_eq!(
            selected["older"].as_ref().map(|value| value.etag.as_str()),
            Some("old")
        );
    }

    #[test]
    fn version_two_delta_without_checkpoint_marker_remains_compatible() {
        let delta: Delta = serde_json::from_str(r#"{"version":2,"changes":{}}"#).unwrap();

        assert!(!delta.checkpoint);
    }

    #[test]
    fn manifest_memory_estimate_tracks_replacement_and_removal() {
        let mut manifest = Manifest::default();
        manifest.update("object".to_owned(), Some(attributes("first")));
        manifest.update("object".to_owned(), Some(attributes("replacement")));
        let rebuilt = Manifest::from_objects(manifest.objects.clone(), false);
        assert_eq!(manifest.estimated_bytes(), rebuilt.estimated_bytes());

        manifest.update("object".to_owned(), None);
        assert_eq!(manifest.estimated_bytes(), 0);
    }

    #[test]
    fn oversized_manifest_skips_checkpoint_before_serialization() {
        let manifest = Manifest {
            objects: BTreeMap::new(),
            estimated_bytes: MAX_MANIFEST_BYTES as usize + 1,
            complete: false,
        };

        assert!(
            prepare_checkpoint(
                "refs/heads/main",
                ObjectId::empty_tree(gix_hash::Kind::Sha1),
                &manifest,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn complete_listing_pages_objects_and_prefixes_without_git_traversal() {
        let mut manifest = Manifest::empty_complete();
        for path in [
            "prefix/a-1",
            "prefix/a/item",
            "prefix/a0",
            "prefix/b/item",
            "prefix/c",
            "prefix/.git/hidden",
        ] {
            manifest.update(path.to_owned(), Some(attributes(path)));
        }

        let first = manifest
            .complete_listing("prefix/", None, Some(b'/'), 2)
            .unwrap();
        assert_eq!(
            first,
            ManifestListingPage {
                items: vec![
                    ManifestListingItem::Object {
                        path: "prefix/a-1".to_owned(),
                        attributes: ListingAttributes {
                            etag: "prefix/a-1".to_owned(),
                            size: 0,
                            modified_seconds: 0,
                        },
                    },
                    ManifestListingItem::CommonPrefix("prefix/a/".to_owned()),
                ],
                has_more: true,
            }
        );
        let second = manifest
            .complete_listing("prefix/", Some("prefix/a/"), Some(b'/'), 2)
            .unwrap();
        assert_eq!(
            second.items,
            vec![
                ManifestListingItem::Object {
                    path: "prefix/a0".to_owned(),
                    attributes: ListingAttributes {
                        etag: "prefix/a0".to_owned(),
                        size: 0,
                        modified_seconds: 0,
                    },
                },
                ManifestListingItem::CommonPrefix("prefix/b/".to_owned()),
            ]
        );
        assert!(second.has_more);
    }

    #[test]
    fn incomplete_manifest_cannot_replace_git_tree_listing() {
        let mut manifest = Manifest::default();
        manifest.update("known".to_owned(), Some(attributes("known")));

        assert!(manifest.complete_listing("", None, None, 1000).is_none());
    }

    #[test]
    fn checkpoint_persists_complete_namespace_proof() {
        let commit = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        let complete = prepare_checkpoint("refs/heads/main", commit, &Manifest::empty_complete())
            .unwrap()
            .unwrap();
        let stored: StoredCheckpoint = serde_json::from_slice(&complete.bytes).unwrap();

        assert_eq!(stored.version, VERSION);
        assert_eq!(stored.commit, commit.to_string());
        assert!(stored.complete);

        let legacy: StoredCheckpoint = serde_json::from_str(&format!(
            r#"{{"version":1,"commit":"{commit}","objects":{{}}}}"#
        ))
        .unwrap();
        assert!(!legacy.complete);
    }
}
