//! Exact-plan adaptation of Celld replica/client/remote-compactor mechanics.
//! See UPSTREAM.md. Provider construction and transport stay in crab-storage.

use bytes::Bytes;
use crab_storage::{ETag, StorageError, Store, StoreLayout};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::{CaptureBatch, CrabError, Limits, Position, Result, SegmentInfo, VerifiedLocalPlan};

const HEAD_BYTES: u64 = 1 << 20;

mod append;
mod bundles;
pub(crate) mod io;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteSegment {
    pub epoch: String,
    pub level: u8,
    pub bundle: Option<BundleLocation>,
    pub info: SegmentInfo,
    pub index_hash: [u8; 32],
    pub index_size: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BundleLocation {
    pub hash: [u8; 32],
    pub offset: u64,
    pub size: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Parent {
    epoch: String,
    digest: [u8; 32],
    position: Position,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    epoch: String,
    position: Position,
    segments: Vec<RemoteSegment>,
    parent: Option<Parent>,
}

/// A pinned replica head and its conditional-write token, not an ownership lease.
#[derive(Clone)]
pub struct ReplicaHead {
    manifest: Manifest,
    etag: Option<ETag>,
    digest: [u8; 32],
    key: String,
    // An immutable, verified page map is scoped to this replica instance. A
    // receipt used with another store must rebuild its indexes at that store.
    pages: Option<crate::PagedDatabase>,
}

impl ReplicaHead {
    /// Returns the immutable manifest identity to pin in external control/backup state.
    #[must_use]
    pub fn manifest_digest(&self) -> [u8; 32] {
        self.digest
    }

    #[must_use]
    pub fn position(&self) -> Position {
        self.manifest.position
    }

    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.manifest.segments.len()
    }

    /// Exposes exact object expectations for reconciliation after an ambiguous PUT.
    pub fn segments(&self) -> impl ExactSizeIterator<Item = &SegmentInfo> {
        self.manifest.segments.iter().map(|segment| &segment.info)
    }
}

/// One explicit epoch's immutable objects and compare-and-swap replica head.
///
/// The supplied store must support conditional writes and must not stage them.
/// Callers own epoch allocation, writer leases, HTTP acknowledgement and retention.
/// No operation lists objects, elects an owner, or deletes source artifacts.
#[derive(Clone)]
pub struct Replica {
    pub(crate) identity: std::sync::Arc<()>,
    pub(crate) host: crate::Host,
    layout: StoreLayout<Store>,
    epoch: String,
    pub(crate) limits: Limits,
}

impl Replica {
    /// Selects local I/O, SQLite VFS and job/worker facilities without changing transport.
    #[must_use]
    pub fn with_host(mut self, host: crate::Host) -> Self {
        self.host = host;
        self
    }
    pub(crate) fn compaction_range(
        &self,
        head: &ReplicaHead,
        level: u8,
    ) -> Result<Option<std::ops::Range<usize>>> {
        self.check_head(head)?;
        let mut start = 0;
        let mut bytes = 0u64;
        let mut count = 0;
        for (index, segment) in head.manifest.segments.iter().enumerate() {
            if segment.level + 1 != level
                || bytes.saturating_add(segment.info.size_bytes) > self.limits.max_file_bytes
                || count == 128
            {
                if count > 0 {
                    return Ok(Some(start..index));
                }
                count = 0;
                bytes = 0;
            }
            if segment.level + 1 == level {
                if count == 0 {
                    start = index;
                }
                bytes += segment.info.size_bytes;
                count += 1;
            }
        }
        Ok((count > 0).then_some(start..head.segment_count()))
    }
    /// Binds a caller-owned repository layout and unique epoch to a replica.
    pub fn new(layout: StoreLayout<Store>, epoch: &str, limits: Limits) -> Result<Self> {
        if !valid_epoch(epoch) || layout.store().staging_write_prefix().is_some() {
            return Err(CrabError::InvalidState(
                "invalid epoch or staged replica store",
            ));
        }
        Ok(Self {
            identity: std::sync::Arc::new(()),
            host: crate::Host::default(),
            layout,
            epoch: epoch.to_owned(),
            limits: limits.validate()?,
        })
    }

    /// Reads only this epoch's named head; absence is not inferred from a listing.
    pub async fn head(&self) -> Result<Option<ReplicaHead>> {
        let _permit = self.host.io_permit().await?;
        let key = self.path("head.json");
        let (body, etag) = match self
            .layout
            .store()
            .get_with_etag_bounded(&key, HEAD_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let manifest: Manifest = serde_json::from_slice(&body)?;
        self.validate_manifest(&manifest)?;
        Ok(Some(ReplicaHead {
            manifest,
            etag: Some(etag),
            digest: *blake3::hash(&body).as_bytes(),
            key: key.to_string(),
            pages: None,
        }))
    }

    /// Reopens an exact immutable manifest after restart, without reading the head.
    ///
    /// The returned historical view has no mutation token. Restore and paging
    /// accept it; publication/compaction require a current `head()` receipt.
    pub async fn open_exact(&self, digest: [u8; 32]) -> Result<ReplicaHead> {
        let _permit = self.host.io_permit().await?;
        let key = self.object_path(&digest, "manifest.json");
        let (body, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&key, HEAD_BYTES)
            .await?;
        if *blake3::hash(&body).as_bytes() != digest {
            return Err(CrabError::ChecksumMismatch);
        }
        let manifest = serde_json::from_slice(&body)?;
        self.validate_manifest(&manifest)?;
        Ok(ReplicaHead {
            manifest,
            etag: None,
            digest,
            key: self.path("head.json").to_string(),
            pages: None,
        })
    }

    /// Verifies and uploads every cut, then conditionally advances the named head.
    ///
    /// Pass `None` only for the first full snapshot/capture. Retain the batch on
    /// error or cancellation: immutable PUTs are retryable, but a head PUT may
    /// already have committed. Re-read `head()` and reconcile the exact plan
    /// before another write; never acknowledge an error as remote durability.
    /// New cuts are fully verified against an authenticated predecessor page map.
    /// Reopened heads fetch indexes only; live receipts reuse their immutable map.
    pub async fn replicate(
        &self,
        batch: &CaptureBatch,
        expected: Option<&ReplicaHead>,
    ) -> Result<ReplicaHead> {
        if let Some(head) = expected {
            self.check_head(head)?;
            if head.etag.is_none() {
                return Err(CrabError::InvalidState("historical head is read-only"));
            }
        }
        if batch.segments.is_empty() {
            return expected
                .filter(|h| h.position() == batch.position)
                .cloned()
                .ok_or(CrabError::TxNotAvailable);
        }
        self.admit_append(
            expected,
            batch.segments.iter().map(|s| s.info().clone()),
            batch.position,
        )?;
        let local = batch.segments.clone();
        let host = self.host.clone();
        let inputs = self
            .host
            .run(move || {
                local
                    .into_iter()
                    .map(|segment| {
                        let bytes = host.read(segment.path(), segment.info().size_bytes)?;
                        Ok((bytes, segment.info().clone(), None))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await??;
        let (prepared, pages) = self
            .prepare_append(inputs, expected, batch.position)
            .await?;
        self.publish_append(prepared, pages, expected).await
    }

    /// Downloads and verifies a pinned head, then atomically restores a new file.
    ///
    /// Once installation enters the blocking executor it is not abortable.
    /// Keep the destination exclusive and supervise this future through completion.
    pub async fn restore(&self, head: &ReplicaHead, destination: &Path) -> Result<Position> {
        self.check_head(head)?;
        let scope = self.recovery_scope().await?;
        let plan = scope.verified(head).await?;
        let destination = destination.to_owned();
        let host = self.host.clone();
        scope
            .host
            .run(move || host.restore(&plan, &destination))
            .await?
    }

    /// Publishes a verified standalone snapshot with CAS, retaining all sources.
    ///
    /// A concurrent head advance wins over this compaction: a failed CAS leaves
    /// an unreferenced immutable output, never rolls the writer back. This is
    /// complete-chain snapshot compaction, not partial-range level scheduling.
    pub async fn compact(&self, expected: &ReplicaHead) -> Result<ReplicaHead> {
        self.compact_range(expected, 0..expected.segment_count(), 9)
            .await
    }

    /// Replaces an exact span with a byte-verified merge and authenticated endpoint.
    ///
    /// Levels 1 through 8 merge lower-level files; level 9 is a full snapshot.
    /// All inputs remain available to pinned historical manifests.
    pub async fn compact_range(
        &self,
        expected: &ReplicaHead,
        range: std::ops::Range<usize>,
        level: u8,
    ) -> Result<ReplicaHead> {
        self.check_head(expected)?;
        self.recovery_scope()
            .await?
            .compact_range_inner(expected, range, level)
            .await
    }

    async fn compact_range_inner(
        &self,
        expected: &ReplicaHead,
        range: std::ops::Range<usize>,
        level: u8,
    ) -> Result<ReplicaHead> {
        self.check_head(expected)?;
        if expected.etag.is_none() {
            return Err(CrabError::InvalidState("historical head is read-only"));
        }
        let inputs = expected
            .manifest
            .segments
            .get(range.clone())
            .filter(|s| !s.is_empty())
            .ok_or(CrabError::TxNotAvailable)?;
        if !(1..=9).contains(&level)
            || (level == 9 && (range.start != 0 || range.end != expected.segment_count()))
            || (level < 9 && inputs.iter().any(|s| s.level >= level))
        {
            return Err(CrabError::InvalidState("invalid compaction level or range"));
        }
        // Validate every original index state, including cuts removed by this
        // compaction. Only the selected LTX bodies need downloading and merging.
        self.paged(expected).await?;
        let bytes = self.download(inputs).await?;
        let selected = inputs.to_vec();
        let limits = self.limits;
        let (bytes, info) = self
            .host
            .run(move || {
                for (segment, bytes) in selected.iter().zip(&bytes) {
                    let index = crate::paged::encode_index(bytes)?;
                    if *blake3::hash(&index).as_bytes() != segment.index_hash {
                        return Err(CrabError::ChecksumMismatch);
                    }
                }
                let infos: Vec<_> = selected.iter().map(|s| s.info.clone()).collect();
                crate::recovery::compact_inputs(&bytes, &infos, limits)
            })
            .await??;
        let mut segment = self.upload(bytes, info).await?;
        segment.level = level;
        let mut segments = expected.manifest.segments.clone();
        segments.splice(range, [segment]);
        let pages = crate::paged::build(self.clone(), &segments, expected.position()).await?;
        let mut head = self
            .publish(segments, expected.position(), Some(expected))
            .await?;
        head.pages = Some(pages);
        Ok(head)
    }

    /// Starts a fresh epoch from an explicitly pinned predecessor in this repository.
    ///
    /// Admits the pinned plan before I/O, verifies destination indexes and object
    /// sizes, and defers body verification to authenticated page reads. No data
    /// objects are copied. Epoch allocation, fencing and retention are caller-owned.
    pub async fn inherit(&self, source: &Replica, parent: &ReplicaHead) -> Result<ReplicaHead> {
        if self.epoch == source.epoch || self.layout.repo_path("") != source.layout.repo_path("") {
            return Err(CrabError::InvalidState(
                "invalid predecessor repository or epoch",
            ));
        }
        source.check_head(parent)?;
        let segments = parent.manifest.segments.clone();
        let position = parent.position();
        let predecessor = Some(Parent {
            epoch: source.epoch.clone(),
            digest: parent.digest,
            position,
        });
        self.validate_manifest(&Manifest {
            version: 2,
            epoch: self.epoch.clone(),
            position,
            segments: segments.clone(),
            parent: predecessor.clone(),
        })?;
        // Resolve against the destination, not a source receipt's cached map.
        // Equal repository paths do not prove equal backing stores or routes.
        let pages = crate::paged::build(self.clone(), &segments, position).await?;
        for segment in &segments {
            let (key, size) = match &segment.bundle {
                Some(bundle) => (
                    self.epoch_object_path(&segment.epoch, &bundle.hash, "bundle"),
                    bundle.size,
                ),
                None => (
                    self.epoch_object_path(&segment.epoch, &segment.info.blake3, "ltx"),
                    segment.info.size_bytes,
                ),
            };
            let _permit = self.host.io_permit().await?;
            if self.layout.store().head(&key).await?.size != size {
                return Err(CrabError::ChecksumMismatch);
            }
        }
        let mut head = self
            .publish_with_parent(segments, position, None, predecessor)
            .await?;
        head.pages = Some(pages);
        Ok(head)
    }

    /// Restores an exact cut and starts a fresh local writer continuing its TXID.
    pub async fn resume(&self, head: &ReplicaHead, destination: &Path) -> Result<crate::ManagedDb> {
        self.check_head(head)?;
        let scope = self.recovery_scope().await?;
        let plan = scope.verified(head).await?;
        let destination = destination.to_owned();
        let limits = self.limits;
        let host = self.host.clone();
        scope
            .host
            .run(move || crate::ManagedDb::resume_with_host(&plan, &destination, limits, host))
            .await?
    }

    async fn recovery_scope(&self) -> Result<Self> {
        Ok(self.clone().with_host(self.host.for_recovery().await?))
    }

    /// Opens a pinned page map without downloading LTX page bodies.
    pub async fn paged(&self, head: &ReplicaHead) -> Result<crate::PagedDatabase> {
        self.check_head(head)?;
        crate::paged::build(self.clone(), &head.manifest.segments, head.position()).await
    }

    async fn verified(&self, head: &ReplicaHead) -> Result<VerifiedLocalPlan> {
        self.check_head(head)?;
        let inputs = self.download(&head.manifest.segments).await?;
        let infos: Vec<_> = head
            .manifest
            .segments
            .iter()
            .map(|s| s.info.clone())
            .collect();
        let limits = self.limits;
        let position = head.position();
        self.host
            .run(move || VerifiedLocalPlan::from_bytes(inputs, &infos, position, limits))
            .await?
    }

    async fn download(&self, segments: &[RemoteSegment]) -> Result<Vec<Vec<u8>>> {
        io::ordered(segments.iter().cloned().map(|segment| {
            let replica = self.clone();
            async move { replica.frame(&segment, 0, segment.info.size_bytes).await }
        }))
        .await
    }

    async fn upload(&self, bytes: Vec<u8>, info: SegmentInfo) -> Result<RemoteSegment> {
        let limits = self.limits;
        let (bytes, info, index) = self
            .host
            .run(move || {
                crate::recovery::verify_segment(&bytes, &info, limits)?;
                let index = crate::paged::encode_index(&bytes)?;
                Ok::<_, CrabError>((bytes, info, index))
            })
            .await??;
        let index_hash = *blake3::hash(&index).as_bytes();
        let index_size = index.len() as u64;
        let key = self.object_path(&info.blake3, "ltx");
        self.put(&key, Bytes::from(bytes)).await?;
        let key = self.object_path(&index_hash, "idx");
        self.put(&key, Bytes::from(index)).await?;
        Ok(RemoteSegment {
            epoch: self.epoch.clone(),
            level: 0,
            bundle: None,
            info,
            index_hash,
            index_size,
        })
    }

    async fn publish(
        &self,
        segments: Vec<RemoteSegment>,
        position: Position,
        expected: Option<&ReplicaHead>,
    ) -> Result<ReplicaHead> {
        self.publish_with_parent(
            segments,
            position,
            expected,
            expected.and_then(|h| h.manifest.parent.clone()),
        )
        .await
    }

    async fn publish_with_parent(
        &self,
        segments: Vec<RemoteSegment>,
        position: Position,
        expected: Option<&ReplicaHead>,
        parent: Option<Parent>,
    ) -> Result<ReplicaHead> {
        let manifest = Manifest {
            version: 2,
            epoch: self.epoch.clone(),
            position,
            segments,
            parent,
        };
        self.validate_manifest(&manifest)?;
        let bytes = serde_json::to_vec(&manifest)?;
        if bytes.len() as u64 > HEAD_BYTES {
            return Err(CrabError::Limit("replica head bytes"));
        }
        let key = self.path("head.json");
        let body = Bytes::from(bytes);
        let digest = *blake3::hash(&body).as_bytes();
        // Persist a content-addressed recovery root before the mutable pointer;
        // external control can pin it without trusting a former owner's head.
        let manifest_key = self.object_path(&digest, "manifest.json");
        self.put(&manifest_key, body.clone()).await?;
        let _permit = self.host.io_permit().await?;
        let etag = match expected {
            Some(head) => {
                self.layout
                    .store()
                    .update(
                        &key,
                        body,
                        head.etag
                            .clone()
                            .ok_or(CrabError::InvalidState("historical head is read-only"))?,
                    )
                    .await?
            }
            None => {
                self.layout
                    .store()
                    .create_strict_with_etag(&key, body)
                    .await?
            }
        };
        Ok(ReplicaHead {
            manifest,
            etag: Some(etag),
            digest,
            key: key.to_string(),
            pages: None,
        })
    }

    fn check_head(&self, head: &ReplicaHead) -> Result<()> {
        if head.key != self.path("head.json").as_ref() {
            return Err(CrabError::InvalidState("head belongs to another replica"));
        }
        self.validate_manifest(&head.manifest)
    }

    fn validate_manifest(&self, manifest: &Manifest) -> Result<()> {
        if manifest.version != 2 || manifest.epoch != self.epoch || manifest.segments.is_empty() {
            return Err(CrabError::InvalidState("invalid replica manifest"));
        }
        if manifest.segments.len() > self.limits.max_segments {
            return Err(CrabError::Limit("plan segments"));
        }
        let mut position = Position::default();
        let mut page_size = None;
        let mut total = 0u64;
        if manifest.parent.as_ref().is_some_and(|p| {
            !valid_epoch(&p.epoch)
                || p.epoch == self.epoch
                || p.position.txid > manifest.position.txid
                || p.position.txid == 0
                || p.position.checksum & crate::CHECKSUM_FLAG == 0
        }) {
            return Err(CrabError::LTXCorrupted);
        }
        for segment in &manifest.segments {
            let info = &segment.info;
            if !valid_epoch(&segment.epoch)
                || segment.level > 9
                || segment.bundle.as_ref().is_some_and(|b| {
                    b.size > self.limits.max_plan_bytes
                        || b.offset
                            .checked_add(info.size_bytes)
                            .is_none_or(|end| end > b.size)
                })
            {
                return Err(CrabError::LTXCorrupted);
            }
            total = total
                .checked_add(info.size_bytes)
                .and_then(|n| n.checked_add(segment.index_size))
                .ok_or(CrabError::Limit("remote plan bytes"))?;
            if total > self.limits.max_plan_bytes
                || info.size_bytes > self.limits.max_file_bytes
                || segment.index_size > u64::from(info.database_pages) * 60
                || info.size_bytes < 128
                || info.database_pages == 0
                || u64::from(info.database_pages) * u64::from(info.page_size)
                    > self.limits.max_database_bytes
            {
                return Err(CrabError::Limit("remote plan bytes or pages"));
            }
            if !(512..=65536).contains(&info.page_size)
                || !info.page_size.is_power_of_two()
                || page_size.is_some_and(|size| size != info.page_size)
                || position.txid.checked_add(1) != Some(info.min_txid)
                || info.max_txid < info.min_txid
                || info.pre_checksum != position.checksum
                || info.post_checksum & crate::CHECKSUM_FLAG == 0
            {
                return Err(CrabError::LTXCorrupted);
            }
            position = info.position();
            page_size = Some(info.page_size);
        }
        if position != manifest.position {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    pub(crate) async fn index(&self, segment: &RemoteSegment) -> Result<Vec<u8>> {
        let _permit = self.host.io_permit().await?;
        let key = self.epoch_object_path(&segment.epoch, &segment.index_hash, "idx");
        let (bytes, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&key, segment.index_size)
            .await?;
        if bytes.len() as u64 != segment.index_size
            || *blake3::hash(&bytes).as_bytes() != segment.index_hash
        {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(bytes.to_vec())
    }

    pub(crate) async fn frame(
        &self,
        segment: &RemoteSegment,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>> {
        let info = &segment.info;
        let end = offset.checked_add(size).ok_or(CrabError::LTXCorrupted)?;
        if end > info.size_bytes {
            return Err(CrabError::LTXCorrupted);
        }
        let (key, base) = match &segment.bundle {
            Some(bundle) => (
                self.epoch_object_path(&segment.epoch, &bundle.hash, "bundle"),
                bundle.offset,
            ),
            None => (
                self.epoch_object_path(&segment.epoch, &info.blake3, "ltx"),
                0,
            ),
        };
        let start = base.checked_add(offset).ok_or(CrabError::LTXCorrupted)?;
        let end = base.checked_add(end).ok_or(CrabError::LTXCorrupted)?;
        let _permit = self.host.io_permit().await?;
        let bytes = self.layout.store().range_get(&key, start..end).await?;
        if bytes.len() as u64 != size {
            return Err(CrabError::LTXCorrupted);
        }
        Ok(bytes.to_vec())
    }

    fn object_path(&self, hash: &[u8; 32], extension: &str) -> object_store::path::Path {
        self.epoch_object_path(&self.epoch, hash, extension)
    }

    fn epoch_object_path(
        &self,
        epoch: &str,
        hash: &[u8; 32],
        extension: &str,
    ) -> object_store::path::Path {
        let hash = blake3::Hash::from_bytes(*hash).to_hex();
        self.layout
            .repo_path(&format!("ltx/{epoch}/objects/{hash}.{extension}"))
    }

    fn path(&self, suffix: &str) -> object_store::path::Path {
        self.layout
            .repo_path(&format!("ltx/{}/{suffix}", self.epoch))
    }
}

pub(crate) fn valid_epoch(epoch: &str) -> bool {
    !epoch.is_empty()
        && epoch.len() <= 128
        && epoch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
