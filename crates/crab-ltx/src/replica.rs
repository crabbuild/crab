//! Immutable Cell-scoped LTX roots prepared independently of ownership CAS.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{CellObjectKind, CellStorageLayout};
use bytes::Bytes;
use futures_util::{StreamExt as _, TryStreamExt as _, stream};

use crate::{CaptureBatch, CrabError, Host, Limits, Position, Result};

mod cache;
mod compaction;
pub(crate) mod directory;
mod merge;
mod prepare;
mod restore;
pub(crate) mod root;
mod upload;
mod verify;

use directory::{DirectoryEntry, DirectorySpan, DirectoryTree, ObjectExtent};
use root::{
    RootDocument, SegmentDescriptor, decode_root, decode_segment_page, encode_root,
    encode_segment_page,
};

const ROOT_BYTES: u64 = 32 << 10;
const SEGMENT_PAGE_BYTES: u64 = 64 << 10;
const MAX_SEGMENTS: usize = 4096;
const SEGMENTS_PER_PAGE: usize = 96;
const MAX_SEGMENT_PAGES: usize = 64;
const COMPACTION_FANOUT: usize = 8;
const MAX_COMPACTION_INPUTS: usize = 128;
// These buffers schedule immutable reads; Host I/O permits remain the shared
// admission boundary across roots, restores, and concurrent Cells.
const OBJECT_FETCH_CONCURRENCY: usize = 8;
pub(super) const OBJECT_UPLOAD_CONCURRENCY: usize = 8;
// A capture body upload can retain four multipart chunks. Bound each
// multi-segment transfer cohort here; Host permits cap aggregate cohorts.
pub(super) const SEGMENT_TRANSFER_CONCURRENCY: usize = 4;
pub(super) const RESTORE_IN_FLIGHT_WINDOWS: usize = 8;
pub(super) const RESTORE_WINDOW_BYTES: u32 = 1 << 20;

/// An immutable Cell root identity suitable for publication in control state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootRef {
    /// Cell this root belongs to.
    pub cell: [u8; 32],
    /// Incarnation that produced the root.
    pub incarnation: [u8; 16],
    /// Digest of the published root record.
    pub digest: [u8; 32],
    /// Position the root publishes.
    pub position: Position,
    /// Root commit sequence.
    pub commit_sequence: u64,
}

/// One immutable object authenticated as part of an exact Cell root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RootObjectRef {
    /// Digest of the immutable object.
    pub digest: [u8; 32],
    /// Kind of immutable object.
    pub kind: CellObjectKind,
}

/// Immutable publication cost a replica path paid.
///
/// One prepared root uploads a segment body and index, the changed directory
/// nodes, and the root document, so a caller that wants the object-store cost
/// of one command reads this ledger instead of inferring it from the database
/// size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PublicationCost {
    /// Immutable objects uploaded.
    pub objects: u64,
    /// Sum of immutable object bytes uploaded.
    pub bytes: u64,
}

#[derive(Default)]
pub(super) struct PublicationLedger {
    objects: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
}

impl PublicationLedger {
    pub(super) fn record(&self, bytes: u64) {
        self.objects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns the accumulated cost and resets the ledger.
    fn take(&self) -> PublicationCost {
        PublicationCost {
            objects: self.objects.swap(0, std::sync::atomic::Ordering::AcqRel),
            bytes: self.bytes.swap(0, std::sync::atomic::Ordering::AcqRel),
        }
    }

    /// Returns the accumulated cost without resetting the ledger.
    fn snapshot(&self) -> PublicationCost {
        PublicationCost {
            objects: self.objects.load(std::sync::atomic::Ordering::Relaxed),
            bytes: self.bytes.load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// A fully uploaded immutable root proposal.
///
/// Construction is private so callers cannot publish a root before all of its
/// dependencies have passed verification and immutable upload.
#[derive(Clone)]
pub struct PreparedRoot {
    predecessor: Option<RootRef>,
    verified: VerifiedRoot,
}

/// A verified recovery bundle pinned to one exact published predecessor.
///
/// The bundle validates every embedded LTX file at construction. A
/// [`CellReplica`] additionally verifies Cell scope, chain continuity, and the
/// declared final position before it uploads a successor root.
pub struct RecoveryOverlay {
    predecessor: RootRef,
    bundle: crate::bundle::Bundle,
    final_position: Position,
    final_commit_sequence: u64,
    _disk_reservation: Option<crate::DiskReservation>,
    _bundle_lease: Option<Arc<dyn crate::bundle::BundleLease>>,
}

impl RecoveryOverlay {
    /// Describes the overlay that supersedes `predecessor`; the caller adds
    /// the disk reservation and bundle lease that keep it readable.
    #[must_use]
    pub fn new(
        predecessor: RootRef,
        bundle: crate::bundle::Bundle,
        final_position: Position,
        final_commit_sequence: u64,
    ) -> Self {
        Self {
            predecessor,
            bundle,
            final_position,
            final_commit_sequence,
            _disk_reservation: None,
            _bundle_lease: None,
        }
    }

    /// Keeps temporary recovery storage admitted until this overlay is dropped.
    #[must_use]
    pub fn with_disk_reservation(mut self, reservation: crate::DiskReservation) -> Self {
        self._disk_reservation = Some(reservation);
        self
    }

    /// Keeps a server-owned verified artifact alive while this overlay is used.
    #[must_use]
    pub fn with_bundle_lease(mut self, lease: Arc<dyn crate::bundle::BundleLease>) -> Self {
        self._bundle_lease = Some(lease);
        self
    }

    /// Transfers an unleased verified bundle out after publication.
    pub fn into_bundle(self) -> crate::Result<crate::bundle::Bundle> {
        if self._disk_reservation.is_some() || self._bundle_lease.is_some() {
            return Err(crate::CrabError::InvalidState(
                "recovery overlay still owns bundle resources",
            ));
        }
        Ok(self.bundle)
    }

    /// Returns the root this overlay supersedes.
    #[must_use]
    pub const fn predecessor(&self) -> RootRef {
        self.predecessor
    }

    /// Returns the position the overlay publishes.
    #[must_use]
    pub const fn final_position(&self) -> Position {
        self.final_position
    }

    /// Returns the commit sequence the overlay publishes.
    #[must_use]
    pub const fn final_commit_sequence(&self) -> u64 {
        self.final_commit_sequence
    }

    /// Returns the verified recovery bundle.
    #[must_use]
    pub fn bundle(&self) -> &crate::bundle::Bundle {
        &self.bundle
    }
}

impl PreparedRoot {
    /// Returns the exact root that was prepared.
    #[must_use]
    pub fn root(&self) -> RootRef {
        self.verified.root
    }

    /// Returns the root this preparation supersedes, if any.
    #[must_use]
    pub fn predecessor(&self) -> Option<RootRef> {
        self.predecessor
    }

    /// Returns the verified metadata for the prepared root.
    #[must_use]
    pub fn verified(&self) -> &VerifiedRoot {
        &self.verified
    }
}

/// Metadata and dependency graph verified from one exact immutable root.
#[derive(Clone)]
pub struct VerifiedRoot {
    root: RootRef,
    page_size: u32,
    database_pages: u32,
    schema: u32,
    segment_count: usize,
    directory_height: u32,
    pages: CellPagedDatabase,
}

impl VerifiedRoot {
    /// Returns the exact immutable root.
    #[must_use]
    pub fn root(&self) -> RootRef {
        self.root
    }

    /// Returns the SQLite page size.
    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Returns the database page count.
    #[must_use]
    pub fn database_pages(&self) -> u32 {
        self.database_pages
    }

    /// Returns the schema version.
    #[must_use]
    pub fn schema(&self) -> u32 {
        self.schema
    }

    /// Returns the number of segments the root references.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segment_count
    }

    /// Returns the height of the root's directory tree.
    #[must_use]
    pub fn directory_height(&self) -> u32 {
        self.directory_height
    }

    /// Returns a pinned, lazy page reader for this exact root.
    #[must_use]
    pub fn paged(&self) -> CellPagedDatabase {
        self.pages.clone()
    }

    /// Streams this exact root into a new local SQLite file.
    ///
    /// The destination and its SQLite sidecars must not exist. Every page is
    /// authenticated before an atomically installed result becomes visible.
    pub async fn restore(&self, destination: &Path) -> Result<Position> {
        self.pages
            .replica
            .host
            .observe_ltx_logical_read(crate::LtxReadOrigin::Cold);
        restore::run(&self.pages, destination).await
    }
}

/// Lazy authenticated page access through one immutable Cell root.
#[derive(Clone)]
pub struct CellPagedDatabase {
    replica: CellReplica,
    directory_digest: [u8; 32],
    directory_height: u32,
    extents: Arc<BTreeMap<[u8; 32], ObjectExtent>>,
    page_size: u32,
    database_pages: u32,
    position: Position,
}

pub(super) struct FetchedSpan {
    span: DirectorySpan,
    frames: Bytes,
}

impl FetchedSpan {
    pub(super) fn try_for_each_page(
        self,
        page_size: u32,
        mut operation: impl FnMut(u32, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        for entry in self.span.entries {
            let offset = usize::try_from(entry.offset - self.span.start)
                .map_err(|_| CrabError::LTXCorrupted)?;
            let frame_end = offset
                .checked_add(entry.length as usize)
                .ok_or(CrabError::LTXCorrupted)?;
            let frame = self
                .frames
                .get(offset..frame_end)
                .ok_or(CrabError::LTXCorrupted)?;
            if *blake3::hash(frame).as_bytes() != entry.frame_hash {
                return Err(CrabError::ChecksumMismatch);
            }
            let bytes = crate::paged::decode_frame(frame, page_size, entry.page)?;
            if crate::ltx::checksum_page(entry.page, &bytes) != entry.checksum {
                return Err(CrabError::ChecksumMismatch);
            }
            operation(entry.page, bytes)?;
        }
        Ok(())
    }
}

/// Exact immutable Cell root prepared for writable sparse activation.
///
/// Preparation loads authenticated directory checksums, never LTX page bodies.
/// The resulting value can cross into the Cell's assigned SQLite worker.
#[derive(Clone)]
pub struct CellWritableDatabase {
    database: CellPagedDatabase,
    checksums: crate::pages::PageChecksums,
    destination: std::path::PathBuf,
}

impl CellPagedDatabase {
    /// Returns the position this root publishes.
    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }

    /// Returns the SQLite page size.
    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Returns the database page count.
    #[must_use]
    pub fn page_count(&self) -> u32 {
        self.database_pages
    }

    /// Streams the authenticated checksum index to this activation's local disk.
    ///
    /// The destination must be fresh and must later be passed unchanged to
    /// `CellWritableDatabase::open_writable`.
    pub async fn prepare_writable(
        mut self,
        destination: &std::path::Path,
    ) -> Result<CellWritableDatabase> {
        let host = self.replica.host.for_dirty().await?;
        self.replica = self.replica.with_host(host);
        let checksums = directory::load_checksums(
            directory::Verification {
                layout: &self.replica.layout,
                cell: &self.replica.cell,
                incarnation: &self.replica.incarnation,
                page_size: self.page_size,
                database_pages: self.database_pages,
                extents: &self.extents,
                host: &self.replica.host,
                origin: crate::LtxReadOrigin::Cold,
            },
            self.directory_digest,
            self.directory_height,
            destination,
            self.replica.limits,
        )
        .await?;
        let host = self.replica.host.clone().without_dirty();
        self.replica = self.replica.with_host(host);
        Ok(CellWritableDatabase {
            database: self,
            checksums,
            destination: destination.to_owned(),
        })
    }

    /// Reads one page by verifying every radix node and the selected LTX frame.
    pub async fn read_page(&self, page: u32) -> Result<Vec<u8>> {
        self.replica
            .host
            .observe_ltx_logical_read(crate::LtxReadOrigin::Sparse);
        self.read_page_with_origin(page, crate::LtxReadOrigin::Sparse)
            .await
    }

    async fn read_page_with_origin(
        &self,
        page: u32,
        origin: crate::LtxReadOrigin,
    ) -> Result<Vec<u8>> {
        if page == crate::ltx::lock_pgno(self.page_size) && page <= self.database_pages {
            return Ok(vec![0; self.page_size as usize]);
        }
        let directory_started = self.replica.host.now_monotonic();
        let entry = directory::lookup(
            directory::Verification {
                layout: &self.replica.layout,
                cell: &self.replica.cell,
                incarnation: &self.replica.incarnation,
                page_size: self.page_size,
                database_pages: self.database_pages,
                extents: &self.extents,
                host: &self.replica.host,
                origin,
            },
            self.directory_digest,
            self.directory_height,
            page,
        )
        .await;
        self.replica.host.observe_ltx_phase(
            crate::LtxPhase::Directory,
            directory_started,
            entry.is_ok(),
        );
        let entry = entry?;
        let extent = self
            .extents
            .get(&entry.object)
            .ok_or(CrabError::LTXCorrupted)?;
        let end = entry
            .offset
            .checked_add(u64::from(entry.length))
            .ok_or(CrabError::LTXCorrupted)?;
        let path = self.replica.layout.incarnation_object_path(
            &self.replica.cell,
            &self.replica.incarnation,
            &entry.object,
            extent.kind,
        );
        let _permit = self.replica.host.io_permit().await?;
        let fetch_started = self.replica.host.now_monotonic();
        let frame = self
            .replica
            .layout
            .store()
            .range_get(&path, entry.offset..end)
            .await;
        self.replica.host.observe_ltx_phase(
            crate::LtxPhase::FrameFetch,
            fetch_started,
            frame.is_ok(),
        );
        self.replica.host.observe_ltx_origin_request(
            origin,
            frame.is_ok(),
            frame.as_ref().map_or(0, |bytes| bytes.len()),
        );
        let frame = frame?;
        if frame.len() != entry.length as usize
            || *blake3::hash(&frame).as_bytes() != entry.frame_hash
        {
            return Err(CrabError::ChecksumMismatch);
        }
        let bytes = crate::paged::decode_frame(&frame, self.page_size, page)?;
        if crate::ltx::checksum_page(page, &bytes) != entry.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(bytes)
    }

    async fn read_run(
        &self,
        first: u32,
        max_pages: u32,
        origin: crate::LtxReadOrigin,
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        if max_pages == 0 || first == 0 || first > self.database_pages {
            return Ok(Vec::new());
        }
        let lock = crate::ltx::lock_pgno(self.page_size);
        if first == lock {
            return Ok(vec![(
                first,
                self.read_page_with_origin(first, origin).await?,
            )]);
        }
        let count = max_pages
            .min(RESTORE_WINDOW_BYTES / self.page_size)
            .min(self.database_pages - first + 1);
        let span = self
            .lookup_spans(first, count, origin)
            .await?
            .into_iter()
            .next()
            .ok_or(CrabError::LTXCorrupted)?;
        let fetched = self.fetch_span(span, origin).await?;
        let mut output = Vec::new();
        fetched.try_for_each_page(self.page_size, |page, bytes| {
            output.push((page, bytes));
            Ok(())
        })?;
        Ok(output)
    }

    async fn read_restore_window(&self, first: u32, count: u32) -> Result<Vec<FetchedSpan>> {
        let spans = self
            .lookup_spans(first, count, crate::LtxReadOrigin::Cold)
            .await?;
        let runs = stream::iter(
            spans
                .into_iter()
                .map(|span| self.fetch_span(span, crate::LtxReadOrigin::Cold)),
        )
        .buffered(OBJECT_FETCH_CONCURRENCY)
        .try_collect()
        .await?;
        Ok(runs)
    }

    async fn lookup_spans(
        &self,
        first: u32,
        count: u32,
        origin: crate::LtxReadOrigin,
    ) -> Result<Vec<DirectorySpan>> {
        let started = self.replica.host.now_monotonic();
        let result = directory::lookup_spans(
            directory::Verification {
                layout: &self.replica.layout,
                cell: &self.replica.cell,
                incarnation: &self.replica.incarnation,
                page_size: self.page_size,
                database_pages: self.database_pages,
                extents: &self.extents,
                host: &self.replica.host,
                origin,
            },
            self.directory_digest,
            self.directory_height,
            first,
            count,
        )
        .await;
        self.replica
            .host
            .observe_ltx_phase(crate::LtxPhase::Directory, started, result.is_ok());
        result
    }

    async fn fetch_span(
        &self,
        span: DirectorySpan,
        origin: crate::LtxReadOrigin,
    ) -> Result<FetchedSpan> {
        let extent = self
            .extents
            .get(&span.object)
            .ok_or(CrabError::LTXCorrupted)?;
        let path = self.replica.layout.incarnation_object_path(
            &self.replica.cell,
            &self.replica.incarnation,
            &span.object,
            extent.kind,
        );
        let _permit = self.replica.host.io_permit().await?;
        let started = self.replica.host.now_monotonic();
        let frames = self
            .replica
            .layout
            .store()
            .range_get(&path, span.start..span.end)
            .await;
        self.replica
            .host
            .observe_ltx_phase(crate::LtxPhase::FrameFetch, started, frames.is_ok());
        self.replica.host.observe_ltx_origin_request(
            origin,
            frames.is_ok(),
            frames.as_ref().map_or(0, |bytes| bytes.len()),
        );
        let frames = frames?;
        if frames.len() as u64 != span.end - span.start {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(FetchedSpan { span, frames })
    }
}

impl CellWritableDatabase {
    pub(crate) fn host(&self) -> Host {
        self.database.replica.host.clone()
    }

    pub(crate) fn limits(&self) -> Limits {
        self.database.replica.limits
    }

    pub(crate) fn checksums(&self) -> crate::pages::PageChecksums {
        self.checksums.clone()
    }

    /// Returns the position this activation reads.
    #[must_use]
    pub fn position(&self) -> Position {
        self.database.position()
    }

    /// Returns the SQLite page size.
    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.database.page_size()
    }

    /// Returns the database page count.
    #[must_use]
    pub fn page_count(&self) -> u32 {
        self.database.page_count()
    }

    pub(crate) async fn read_run(
        &self,
        first: u32,
        max_pages: u32,
        origin: crate::LtxReadOrigin,
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        self.database.read_run(first, max_pages, origin).await
    }

    /// Opens a fresh sparse SQLite file pinned to this exact Cell root.
    pub fn open_writable(self, destination: &std::path::Path) -> Result<crate::Db> {
        if destination != self.destination {
            return Err(CrabError::InvalidState(
                "writable destination differs from prepared destination",
            ));
        }
        crate::Db::open_cell_paged(self, destination)
    }
}

/// Immutable LTX object graph for one Cell incarnation.
///
/// Ownership, command acknowledgement and the `control.json` CAS belong to the
/// runtime. This type never writes a mutable pointer or lists object storage.
#[derive(Clone)]
pub struct CellReplica {
    layout: CellStorageLayout,
    cell: [u8; 32],
    incarnation: [u8; 16],
    limits: Limits,
    host: Host,
    cost: Arc<PublicationLedger>,
}

impl CellReplica {
    /// Binds all immutable operations to one Cell incarnation.
    pub fn new(
        layout: CellStorageLayout,
        cell: [u8; 32],
        incarnation: [u8; 16],
        limits: Limits,
    ) -> Result<Self> {
        if layout.store().staging_write_prefix().is_some() {
            return Err(CrabError::InvalidState("staged Cell replica store"));
        }
        Ok(Self {
            layout,
            cell,
            incarnation,
            limits: limits.validate()?,
            host: Host::default(),
            cost: Arc::new(PublicationLedger::default()),
        })
    }

    /// Returns the immutable admission limits selected for this replica.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Returns the cumulative immutable publication cost this replica paid.
    ///
    /// The ledger covers every object the replica uploaded: segment bodies and
    /// indexes, directory nodes, root documents, segment pages, bundle bodies,
    /// and compaction outputs. Callers sampling per-command cost use
    /// [`Self::take_publication_cost`] instead.
    #[must_use]
    pub fn publication_cost(&self) -> PublicationCost {
        self.cost.snapshot()
    }

    /// Returns the publication cost accumulated since the last call and resets it.
    ///
    /// One Cell has a single publisher at a time, so the reset is safe there;
    /// a caller that runs concurrent prepares must use
    /// [`Self::publication_cost`] deltas instead.
    #[must_use]
    pub fn take_publication_cost(&self) -> PublicationCost {
        self.cost.take()
    }

    /// Selects the caller's bounded I/O and blocking execution facilities.
    #[must_use]
    pub fn with_host(mut self, host: Host) -> Self {
        self.host = host;
        self
    }

    /// Exclusively creates a fresh local database using this replica's host and limits.
    ///
    /// The destination and SQLite sidecars must not exist. A failed open leaves
    /// its artifacts quarantined for caller-owned inspection and cleanup.
    pub fn open_new(&self, destination: &std::path::Path) -> Result<crate::Db> {
        crate::recovery::reject_sidecars(destination, &self.host)?;
        let mut file = self.host.filesystem.create(destination)?;
        file.sync_all()?;
        self.host.filesystem.sync_parent(destination)?;
        drop(file);
        crate::Db::open_with_host(destination, self.limits, self.host.clone())
    }
}

fn scheduled_compaction_range(
    descriptors: &[SegmentDescriptor],
    level: u8,
    max_file_bytes: u64,
) -> Option<std::ops::Range<usize>> {
    let source_level = level.checked_sub(1)?;
    let mut start = 0;
    while start < descriptors.len() {
        if descriptors[start].level() != source_level {
            start += 1;
            continue;
        }
        let mut end = start;
        let mut bytes = 0_u64;
        while end < descriptors.len()
            && descriptors[end].level() == source_level
            && end - start < MAX_COMPACTION_INPUTS
            && bytes
                .checked_add(descriptors[end].info.size_bytes)
                .is_some_and(|next| next <= max_file_bytes)
        {
            bytes += descriptors[end].info.size_bytes;
            end += 1;
        }
        if end - start >= COMPACTION_FANOUT {
            return Some(start..end);
        }
        start = end.max(start + 1);
    }
    None
}

struct LoadedGraph {
    aggregate: directory::Aggregate,
    document: RootDocument,
    descriptors: Vec<SegmentDescriptor>,
}

fn compaction_scratch_bytes(graph: &LoadedGraph, range: std::ops::Range<usize>) -> Result<u64> {
    let selected = graph
        .descriptors
        .get(range)
        .filter(|descriptors| !descriptors.is_empty())
        .ok_or(CrabError::TxNotAvailable)?;
    let base = crate::recovery::full_job_scratch_bytes(
        graph.document.page_size,
        graph.document.database_pages,
    )?;
    let indexes = graph
        .descriptors
        .iter()
        .try_fold(base, |total, descriptor| {
            total
                .checked_add(descriptor.index_length)
                .ok_or(CrabError::Limit(crate::LimitKind::ScratchDiskBytes))
        })?;
    selected.iter().try_fold(indexes, |total, descriptor| {
        total
            .checked_add(descriptor.info.size_bytes)
            .ok_or(CrabError::Limit(crate::LimitKind::ScratchDiskBytes))
    })
}

struct AppendInput {
    info: crate::SegmentInfo,
    location: BodyLocation,
    index: Bytes,
    body: AppendBody,
}

enum AppendBody {
    Native(Arc<upload::PinnedCapture>),
    Bundle,
}

#[derive(Clone, Copy)]
enum BodyLocation {
    Native,
    Bundle { digest: [u8; 32], offset: u64 },
}

struct PreparedSegment {
    descriptor: SegmentDescriptor,
    index: Bytes,
    body: AppendBody,
}

struct DirectoryInput {
    descriptor: SegmentDescriptor,
    index: Bytes,
}

fn object_extents(descriptors: &[SegmentDescriptor]) -> Result<BTreeMap<[u8; 32], ObjectExtent>> {
    let mut extents = BTreeMap::new();
    for descriptor in descriptors {
        let (digest, offset, length, kind) = descriptor.object_extent();
        let end = offset.checked_add(length).ok_or(CrabError::LTXCorrupted)?;
        match extents.entry(digest) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ObjectExtent {
                    kind,
                    ranges: std::iter::once(offset..end).collect(),
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let extent = entry.get_mut();
                if extent.kind != kind {
                    return Err(CrabError::LTXCorrupted);
                }
                if !extent
                    .ranges
                    .iter()
                    .any(|range| range.start == offset && range.end == end)
                {
                    extent.ranges.push(offset..end);
                }
            }
        }
    }
    for extent in extents.values_mut() {
        extent.ranges.sort_by_key(|range| range.start);
        if extent
            .ranges
            .windows(2)
            .any(|ranges| ranges[0].end > ranges[1].start)
        {
            return Err(CrabError::LTXCorrupted);
        }
    }
    Ok(extents)
}

fn directory_changes(
    prepared: &[DirectoryInput],
    base_pages: u32,
) -> Result<(BTreeMap<u32, DirectoryEntry>, u32)> {
    let mut changes = BTreeMap::new();
    let mut retain_through = base_pages;
    for prepared in prepared {
        let info = &prepared.descriptor.info;
        let index = &prepared.index;
        // Once a cut truncates a page, later growth must provide a new frame;
        // retaining its old locator would resurrect bytes from before truncation.
        retain_through = retain_through.min(info.database_pages);
        changes.retain(|page, _| *page <= info.database_pages);
        for entry in crate::paged::decode_index(index)? {
            changes.insert(
                entry.page,
                DirectoryEntry {
                    page: entry.page,
                    object: prepared.descriptor.object_digest(),
                    offset: prepared
                        .descriptor
                        .offset()
                        .checked_add(entry.offset)
                        .ok_or(CrabError::LTXCorrupted)?,
                    length: u32::try_from(entry.size).map_err(|_| CrabError::LTXCorrupted)?,
                    frame_hash: entry.hash,
                    checksum: entry.checksum,
                },
            );
        }
    }
    Ok((changes, retain_through))
}
