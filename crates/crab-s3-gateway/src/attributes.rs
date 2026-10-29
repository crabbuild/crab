use std::{
    collections::{BTreeMap, HashMap},
    io::{BufWriter, Read as _, Write as _},
    ops::Bound::{Excluded, Included, Unbounded},
    str::FromStr as _,
    sync::Arc,
};

use bytes::Bytes;
use crab_storage::{Store, StoreLayout};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use gix_hash::ObjectId;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::gateway::Repository;

const MAX_CACHED_COMMITS: usize = 16;
const VERSION: u32 = 2;
const LEGACY_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CHECKPOINT_DECODED_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CAPSULE_READ_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const GZIP_MAGIC: &[u8] = b"\x1f\x8b";
const MAX_DELTA_DEPTH: usize = 1_000_000;
const MAX_CHECKPOINT_PUBLISH_ATTEMPTS: usize = 8;

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
    checkpoint: Option<ObjectId>,
    previous_checkpoint: Option<ObjectId>,
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
    branch: String,
    commit: ObjectId,
    slot: String,
    expected_existing: Option<ObjectId>,
    bytes: Bytes,
}

#[cfg(test)]
impl PreparedCheckpoint {
    pub(crate) fn bytes(&self) -> Bytes {
        self.bytes.clone()
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
            checkpoint: None,
            previous_checkpoint: None,
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

    pub(crate) fn mark_checkpoint(&mut self, commit: ObjectId) {
        self.previous_checkpoint = self.checkpoint;
        self.checkpoint = Some(commit);
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

// Attribute deltas precede Git visibility and are immutable by commit. Share
// their lazy replay across paths so cold keys do not repeat the same history.
pub(crate) struct Cache {
    layout: StoreLayout<Store>,
    commits: Mutex<HashMap<ObjectId, Arc<Mutex<ReadState>>>>,
    checkpoint_versions: Mutex<HashMap<String, (ObjectId, crab_storage::ETag)>>,
}

struct ReadState {
    current: Option<ObjectId>,
    depth: usize,
    checkpoint: Option<ObjectId>,
    changes: BTreeMap<String, Option<ObjectAttributes>>,
    manifest: Option<Arc<Manifest>>,
}

impl Cache {
    pub(crate) fn new(layout: StoreLayout<Store>) -> Self {
        Self {
            layout,
            commits: Mutex::new(HashMap::new()),
            checkpoint_versions: Mutex::new(HashMap::new()),
        }
    }

    async fn state(&self, commit: ObjectId) -> Arc<Mutex<ReadState>> {
        let mut commits = self.commits.lock().await;
        if commits.len() >= MAX_CACHED_COMMITS && !commits.contains_key(&commit) {
            commits.clear();
        }
        Arc::clone(commits.entry(commit).or_insert_with(|| {
            Arc::new(Mutex::new(ReadState {
                current: Some(commit),
                depth: 0,
                checkpoint: None,
                changes: BTreeMap::new(),
                manifest: None,
            }))
        }))
    }

    pub(crate) async fn load(&self, commit: ObjectId) -> crate::Result<Arc<Manifest>> {
        let state = self.state(commit).await;
        let mut state = state.lock().await;
        loop {
            if let Some(manifest) = &state.manifest {
                return Ok(Arc::clone(manifest));
            }
            state.advance(&self.layout).await?;
        }
    }

    pub(crate) async fn object(
        &self,
        commit: ObjectId,
        path: &str,
        oid: ObjectId,
    ) -> crate::Result<Option<ObjectAttributes>> {
        let state = self.state(commit).await;
        let mut state = state.lock().await;
        loop {
            if let Some(attributes) = state.object(path, oid) {
                return Ok(attributes.cloned());
            }
            state.advance(&self.layout).await?;
        }
    }

    pub(crate) async fn objects(
        &self,
        commit: ObjectId,
        objects: &[(String, ObjectId)],
    ) -> crate::Result<BTreeMap<String, ListingAttributes>> {
        let state = self.state(commit).await;
        let mut state = state.lock().await;
        while objects
            .iter()
            .any(|(path, oid)| state.object(path, *oid).is_none())
        {
            state.advance(&self.layout).await?;
        }
        Ok(objects
            .iter()
            .filter_map(|(path, oid)| {
                state
                    .object(path, *oid)
                    .flatten()
                    .map(|attributes| (path.clone(), attributes.into()))
            })
            .collect())
    }
}

impl ReadState {
    fn object(&self, path: &str, oid: ObjectId) -> Option<Option<&ObjectAttributes>> {
        if let Some(manifest) = &self.manifest {
            return Some(manifest.object(path, oid));
        }
        self.changes.get(path).map(|attributes| {
            attributes
                .as_ref()
                .filter(|attributes| attributes.blob_oid == oid.to_string())
        })
    }

    async fn advance(&mut self, layout: &StoreLayout<Store>) -> crate::Result<()> {
        let Some(commit) = self.current else {
            return Ok(());
        };
        if self.depth >= MAX_DELTA_DEPTH {
            return Err(crate::Error::Config("S3 attribute delta chain is too deep"));
        }
        match load_stored(layout, commit).await? {
            None => {
                let manifest = Manifest {
                    checkpoint: self.checkpoint,
                    ..Default::default()
                };
                self.finish(manifest);
            }
            Some(Stored::Legacy(legacy)) => {
                self.finish(Manifest::from_objects(legacy.objects, false))
            }
            Some(Stored::Delta(delta)) => {
                if delta.checkpoint
                    && let Some(mut manifest) =
                        load_checkpoint(layout, commit, delta.checkpoint_slot.as_deref()).await?
                {
                    manifest.checkpoint = Some(commit);
                    self.finish(manifest);
                    return Ok(());
                }
                let parent = parse_parent(delta.parent.as_deref())?;
                // Advance only after every fallible read and parse. Cancellation
                // must leave this commit retryable without losing tombstones.
                if delta.checkpoint && self.checkpoint.is_none() {
                    self.checkpoint = Some(commit);
                }
                retain_newest_changes(&mut self.changes, delta.changes);
                self.current = parent;
                self.depth += 1;
                if parent.is_none() {
                    let manifest = Manifest {
                        complete: true,
                        checkpoint: self.checkpoint,
                        ..Default::default()
                    };
                    self.finish(manifest);
                }
            }
        }
        Ok(())
    }

    fn finish(&mut self, mut manifest: Manifest) {
        manifest.apply(std::mem::take(&mut self.changes));
        self.manifest = Some(Arc::new(manifest));
        self.current = None;
    }
}

fn retain_newest_changes(
    selected: &mut BTreeMap<String, Option<ObjectAttributes>>,
    changes: BTreeMap<String, Option<ObjectAttributes>>,
) {
    for (path, attributes) in changes {
        selected.entry(path).or_insert(attributes);
    }
}

pub(crate) async fn save_delta(
    repository: &Repository,
    commit: ObjectId,
    parent: Option<ObjectId>,
    changes: BTreeMap<String, Option<ObjectAttributes>>,
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
        changes,
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
    if !checkpoint_eligible(manifest) {
        return Ok(None);
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    {
        // JSON emits many tiny fields. Buffer them before gzip so compressor
        // setup and output-buffer clearing do not repeat for each field.
        let mut buffered = BufWriter::with_capacity(64 * 1024, &mut encoder);
        serde_json::to_writer(
            &mut buffered,
            &CheckpointPayload {
                version: VERSION,
                commit: commit.to_string(),
                complete: manifest.complete,
                objects: &manifest.objects,
            },
        )
        .map_err(|source| crate::Error::Attributes { source })?;
        buffered.flush()?;
    }
    let bytes = encoder.finish()?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Ok(None);
    }
    Ok(Some(PreparedCheckpoint {
        branch: branch.to_owned(),
        commit,
        slot: checkpoint_slot(branch),
        expected_existing: manifest.previous_checkpoint,
        bytes: Bytes::from(bytes),
    }))
}

pub(crate) fn checkpoint_eligible(manifest: &Manifest) -> bool {
    u64::try_from(manifest.estimated_bytes()).unwrap_or(u64::MAX) <= MAX_CHECKPOINT_DECODED_BYTES
}

pub(crate) fn checkpoint_slot(branch: &str) -> String {
    blake3::hash(branch.as_bytes()).to_hex().to_string()
}

pub(crate) async fn publish_checkpoint(
    repository: &Repository,
    checkpoint: &PreparedCheckpoint,
) -> crate::Result<bool> {
    let path = checkpoint_slot_path(&repository.layout, &checkpoint.slot)?;
    // A confirmed predecessor's token avoids decoding its full namespace again.
    // The CAS still detects other writers; a conflict rereads and validates
    // their checkpoint before deciding whether this one can replace it.
    let mut confirmed = repository
        .attributes
        .checkpoint_versions
        .lock()
        .await
        .get(&checkpoint.slot)
        .filter(|(commit, _)| Some(*commit) == checkpoint.expected_existing)
        .map(|(_, version)| version.clone());
    for _ in 0..MAX_CHECKPOINT_PUBLISH_ATTEMPTS {
        let etag = if let Some(version) = confirmed.take() {
            Some(version)
        } else {
            let existing = repository
                .store
                .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
                .await;
            match existing {
                Ok((bytes, etag)) => {
                    let stored = parse_checkpoint(&bytes)?;
                    let commit = stored.commit.parse::<ObjectId>().map_err(|_| {
                        crate::Error::Config("invalid S3 attribute checkpoint commit")
                    })?;
                    if commit == checkpoint.commit {
                        return Ok(true);
                    }
                    // The prepared checkpoint carries its manifest's process-local
                    // predecessor. Equality proves that lineage; ETag CAS below still
                    // rejects a concurrent slot replacement.
                    if checkpoint.expected_existing != Some(commit)
                        && !checkpoint_is_newer(repository, checkpoint, commit).await?
                    {
                        return Ok(false);
                    }
                    Some(etag)
                }
                Err(crab_storage::StorageError::NotFound { .. }) => None,
                Err(error) => return Err(error.into()),
            }
        };
        let result = match etag {
            Some(etag) => {
                repository
                    .store
                    .update(&path, checkpoint.bytes.clone(), etag)
                    .await
            }
            None => {
                repository
                    .store
                    .create_strict_with_etag(&path, checkpoint.bytes.clone())
                    .await
            }
        };
        match result {
            Ok(version) => {
                if version.e_tag.is_some() || version.version.is_some() {
                    let mut versions = repository.attributes.checkpoint_versions.lock().await;
                    if versions.len() >= MAX_CACHED_COMMITS
                        && !versions.contains_key(&checkpoint.slot)
                    {
                        versions.clear();
                    }
                    versions.insert(checkpoint.slot.clone(), (checkpoint.commit, version));
                }
                return Ok(true);
            }
            Err(crab_storage::StorageError::StateConflict { .. }) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(crab_storage::StorageError::StateConflict {
        path: path.to_string(),
    }
    .into())
}

fn parse_checkpoint(bytes: &[u8]) -> crate::Result<StoredCheckpoint> {
    let decoded;
    let bytes = if bytes.starts_with(GZIP_MAGIC) {
        let mut decoder = GzDecoder::new(bytes).take(MAX_CHECKPOINT_DECODED_BYTES + 1);
        decoded = {
            let mut decoded = Vec::new();
            decoder.read_to_end(&mut decoded)?;
            decoded
        };
        if decoded.len() as u64 > MAX_CHECKPOINT_DECODED_BYTES {
            return Err(crate::Error::Config(
                "S3 attribute checkpoint exceeds its decoded limit",
            ));
        }
        decoded.as_slice()
    } else {
        bytes
    };
    let stored = serde_json::from_slice::<StoredCheckpoint>(bytes)
        .map_err(|source| crate::Error::Attributes { source })?;
    if !matches!(stored.version, LEGACY_VERSION | VERSION) {
        return Err(crate::Error::Config(
            "unsupported S3 attribute checkpoint version",
        ));
    }
    Ok(stored)
}

async fn checkpoint_is_newer(
    repository: &Repository,
    checkpoint: &PreparedCheckpoint,
    existing: ObjectId,
) -> crate::Result<bool> {
    if delta_chain_reaches(repository, checkpoint.commit, existing).await? {
        return Ok(true);
    }
    if delta_chain_reaches(repository, existing, checkpoint.commit).await? {
        return Ok(false);
    }
    let root = crab_metadata::capsule_protocol::load_root(&repository.layout).await?;
    let current = crab_read::capsule_protocol::open_view_from_root_with_control(
        &repository.layout,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: MAX_CAPSULE_READ_BYTES,
            max_frontier_bytes: MAX_CAPSULE_READ_BYTES,
        },
    )
    .await?
    .refs()
    .get(&checkpoint.branch)
    .cloned();
    let Some(current) = current else {
        return Ok(false);
    };
    let current = current
        .parse::<ObjectId>()
        .map_err(|_| crate::Error::Config("repository ref contains an invalid object ID"))?;
    Ok(
        delta_chain_reaches(repository, current, checkpoint.commit).await?
            && !delta_chain_reaches(repository, current, existing).await?,
    )
}

async fn delta_chain_reaches(
    repository: &Repository,
    start: ObjectId,
    target: ObjectId,
) -> crate::Result<bool> {
    let mut current = Some(start);
    for _ in 0..MAX_DELTA_DEPTH {
        let Some(commit) = current else {
            return Ok(false);
        };
        if commit == target {
            return Ok(true);
        }
        current = match load_stored(&repository.layout, commit).await? {
            Some(Stored::Delta(delta)) => parse_parent(delta.parent.as_deref())?,
            Some(Stored::Legacy(_)) | None => None,
        };
    }
    Err(crate::Error::Config("S3 attribute delta chain is too deep"))
}

async fn load_checkpoint(
    layout: &StoreLayout<Store>,
    commit: ObjectId,
    slot: Option<&str>,
) -> crate::Result<Option<Manifest>> {
    let path = match slot {
        Some(slot) => checkpoint_slot_path(layout, slot)?,
        None => legacy_checkpoint_path(layout, commit),
    };
    let bytes = match layout
        .store()
        .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
        .await
    {
        Ok((bytes, _)) => bytes,
        Err(crab_storage::StorageError::NotFound { .. }) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let objects = match slot {
        Some(_) => {
            let stored = parse_checkpoint(&bytes)?;
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
    layout: &StoreLayout<Store>,
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
    Ok(layout.repo_path(&format!("s3/attribute-checkpoints/branches/{slot}.json")))
}

fn legacy_checkpoint_path(
    layout: &StoreLayout<Store>,
    commit: ObjectId,
) -> object_store::path::Path {
    layout.repo_path(&format!("s3/attribute-checkpoints/{commit}.json"))
}

enum Stored {
    Legacy(LegacyManifest),
    Delta(Delta),
}

async fn load_stored(
    layout: &StoreLayout<Store>,
    commit: ObjectId,
) -> crate::Result<Option<Stored>> {
    let path = layout.repo_path(&format!("s3/attributes/{commit}.json"));
    let bytes = match layout
        .store()
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

    fn checkpoint_repository(store: Store) -> Repository {
        let config = serde_json::from_value(serde_json::json!({
            "name": "repository",
            "provider": "local",
            "bucket": "memory",
            "prefix": "checkpoint-test",
        }))
        .unwrap();
        Repository::new(config, store).unwrap()
    }

    #[tokio::test]
    async fn checkpoint_successors_do_not_read_the_confirmed_predecessor_body() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let reads = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&reads);
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_read_byte_observer(Arc::new(move |bytes| {
                observed.fetch_add(bytes as usize, Ordering::Relaxed);
            }));
        let repository = checkpoint_repository(store.clone());
        let mut manifest = Manifest::empty_complete();
        let mut last = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        manifest.mark_checkpoint(last);
        let first = prepare_checkpoint("refs/heads/main", last, &manifest)
            .unwrap()
            .unwrap();
        assert!(publish_checkpoint(&repository, &first).await.unwrap());
        reads.store(0, Ordering::Relaxed);

        for index in 1..=3 {
            last = ObjectId::from_hex(format!("{index:040x}").as_bytes()).unwrap();
            manifest.mark_checkpoint(last);
            let next = prepare_checkpoint("refs/heads/main", last, &manifest)
                .unwrap()
                .unwrap();
            assert!(publish_checkpoint(&repository, &next).await.unwrap());
        }

        let read_bytes = reads.load(Ordering::Relaxed);
        let path = checkpoint_slot_path(&repository.layout, &first.slot).unwrap();
        let (bytes, _) = store
            .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
            .await
            .unwrap();
        let stored = parse_checkpoint(&bytes).unwrap();
        assert_eq!((read_bytes, stored.commit), (0, last.to_string()));
    }

    #[tokio::test]
    async fn checkpoint_version_conflicts_revalidate_before_replacing_corrupt_state() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let repository = checkpoint_repository(store.clone());
        let mut manifest = Manifest::empty_complete();
        let first = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        manifest.mark_checkpoint(first);
        let prepared = prepare_checkpoint("refs/heads/main", first, &manifest)
            .unwrap()
            .unwrap();
        assert!(publish_checkpoint(&repository, &prepared).await.unwrap());
        let path = checkpoint_slot_path(&repository.layout, &prepared.slot).unwrap();
        let corrupt = Bytes::from_static(b"corrupt checkpoint");
        store.put_overwrite(&path, corrupt.clone()).await.unwrap();

        manifest.mark_checkpoint(ObjectId::empty_blob(gix_hash::Kind::Sha1));
        let next = prepare_checkpoint("refs/heads/main", manifest.checkpoint.unwrap(), &manifest)
            .unwrap()
            .unwrap();
        let result = publish_checkpoint(&repository, &next).await;
        let (remaining, _) = store
            .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
            .await
            .unwrap();
        assert!(matches!(result, Err(crate::Error::Attributes { .. })) && remaining == corrupt);
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
            estimated_bytes: MAX_CHECKPOINT_DECODED_BYTES as usize + 1,
            complete: false,
            ..Manifest::default()
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
    fn checkpoint_round_trips_a_large_namespace() {
        let mut manifest = Manifest::empty_complete();
        for index in 0..4096 {
            let path = format!("namespace/object-{index:08}");
            manifest.update(path.clone(), Some(attributes(&path)));
        }
        let commit = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        let payload = CheckpointPayload {
            version: VERSION,
            commit: commit.to_string(),
            complete: true,
            objects: &manifest.objects,
        };
        let expected = serde_json::to_vec(&payload).unwrap();
        let checkpoint = prepare_checkpoint("refs/heads/main", commit, &manifest)
            .unwrap()
            .unwrap();
        let mut actual = Vec::new();
        GzDecoder::new(checkpoint.bytes.as_ref())
            .read_to_end(&mut actual)
            .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn checkpoint_persists_complete_namespace_proof() {
        let commit = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        let complete = prepare_checkpoint("refs/heads/main", commit, &Manifest::empty_complete())
            .unwrap()
            .unwrap();
        assert!(complete.bytes.starts_with(GZIP_MAGIC));
        let stored = parse_checkpoint(&complete.bytes).unwrap();

        assert_eq!(stored.version, VERSION);
        assert_eq!(stored.commit, commit.to_string());
        assert!(stored.complete);

        let legacy = parse_checkpoint(
            format!(r#"{{"version":1,"commit":"{commit}","objects":{{}}}}"#).as_bytes(),
        )
        .unwrap();
        assert!(!legacy.complete);
    }
}
