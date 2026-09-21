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

mod compaction;
mod directory;
mod restore;
mod root;

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
// Each body upload can retain four multipart chunks. Keep multi-segment
// capture batches below the host-wide request ceiling and bounded in memory.
const SEGMENT_UPLOAD_CONCURRENCY: usize = 4;
pub(super) const RESTORE_IN_FLIGHT_WINDOWS: usize = 8;
pub(super) const RESTORE_WINDOW_BYTES: u32 = 1 << 20;

/// An immutable Cell root identity suitable for publication in control state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootRef {
    pub cell: [u8; 32],
    pub incarnation: [u8; 16],
    pub digest: [u8; 32],
    pub position: Position,
    pub commit_sequence: u64,
}

/// One immutable object authenticated as part of an exact Cell root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RootObjectRef {
    pub digest: [u8; 32],
    pub kind: CellObjectKind,
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
}

impl RecoveryOverlay {
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
        }
    }

    /// Keeps temporary recovery storage admitted until this overlay is dropped.
    #[must_use]
    pub fn with_disk_reservation(mut self, reservation: crate::DiskReservation) -> Self {
        self._disk_reservation = Some(reservation);
        self
    }

    #[must_use]
    pub const fn predecessor(&self) -> RootRef {
        self.predecessor
    }

    #[must_use]
    pub const fn final_position(&self) -> Position {
        self.final_position
    }

    #[must_use]
    pub const fn final_commit_sequence(&self) -> u64 {
        self.final_commit_sequence
    }

    #[must_use]
    pub fn bundle(&self) -> &crate::bundle::Bundle {
        &self.bundle
    }
}

impl PreparedRoot {
    #[must_use]
    pub fn root(&self) -> RootRef {
        self.verified.root
    }

    #[must_use]
    pub fn predecessor(&self) -> Option<RootRef> {
        self.predecessor
    }

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
    #[must_use]
    pub fn root(&self) -> RootRef {
        self.root
    }

    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    #[must_use]
    pub fn database_pages(&self) -> u32 {
        self.database_pages
    }

    #[must_use]
    pub fn schema(&self) -> u32 {
        self.schema
    }

    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segment_count
    }

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
    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }

    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.page_size
    }

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

    #[must_use]
    pub fn position(&self) -> Position {
        self.database.position()
    }

    #[must_use]
    pub fn page_size(&self) -> u32 {
        self.database.page_size()
    }

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
        })
    }

    /// Returns the immutable admission limits selected for this replica.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
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

    /// Verifies and uploads a new immutable root without changing authority.
    pub async fn prepare(
        &self,
        base: Option<&RootRef>,
        cuts: &CaptureBatch,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        let mut replica = self.clone();
        replica.host = self.host.for_dirty().await?;
        replica
            .prepare_captured(base, cuts, commit_sequence, schema)
            .await
    }

    async fn prepare_captured(
        &self,
        base: Option<&RootRef>,
        cuts: &CaptureBatch,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        self.validate_metadata(commit_sequence, schema)?;
        if cuts.segments.is_empty() {
            return Err(CrabError::InvalidState("empty Cell append"));
        }
        let captured_bytes = cuts.segments.iter().try_fold(0_u64, |total, segment| {
            if segment.info().size_bytes > self.limits.max_capture_bytes {
                return Err(CrabError::Limit("captured Cell LTX bytes"));
            }
            total
                .checked_add(segment.info().size_bytes)
                .ok_or(CrabError::Limit("captured Cell LTX bytes"))
        })?;
        if captured_bytes > self.limits.max_capture_bytes {
            return Err(CrabError::Limit("captured Cell LTX bytes"));
        }
        let load_base = async {
            match base {
                Some(root) => self.load_graph(root).await.map(Some),
                None => Ok(None),
            }
        };
        // Local captures and the immutable predecessor cannot affect each
        // other; chain validation still waits for both exact inputs.
        let (base_graph, inputs) =
            futures_util::future::try_join(load_base, self.prepare_captured_inputs(&cuts.segments))
                .await?;
        self.validate_append_sequence(&base_graph, commit_sequence)?;

        // Admit the complete prospective chain from trusted capture metadata
        // before reading local bodies or starting immutable uploads.
        let mut descriptors = base_graph
            .as_ref()
            .map(|graph| graph.descriptors.clone())
            .unwrap_or_default();
        descriptors.extend(
            cuts.segments
                .iter()
                .map(|segment| SegmentDescriptor::native(segment.info().clone(), [0; 32], 0)),
        );
        self.validate_chain(&descriptors, cuts.position)?;

        // Keep each exact capture handle open through verification and upload.
        // A path replacement cannot redirect retries, while the inspected LTX
        // digest still rejects in-place mutation before authority may publish.
        self.prepare_append(
            base,
            base_graph,
            inputs,
            cuts.position,
            commit_sequence,
            schema,
            None,
        )
        .await
    }

    async fn prepare_captured_inputs(
        &self,
        segments: &[crate::LocalSegment],
    ) -> Result<Vec<AppendInput>> {
        stream::iter(segments.iter().cloned().map(|segment| async move {
            let source = segment.path().to_owned();
            let info = segment.info().clone();
            let source = PinnedCapture::open(&self.host, source, info.size_bytes).await?;
            let index = match segment.captured_index() {
                Some(index) => index.to_vec(),
                None => inspect_segment_source(self, Arc::clone(&source), &info).await?,
            };
            Ok(AppendInput {
                info,
                location: BodyLocation::Native,
                index,
                body: AppendBody::Native(source),
            })
        }))
        // Preserve descriptor order while overlapping independent file jobs.
        // Host job permits remain the shared process-wide admission boundary.
        .buffered(SEGMENT_UPLOAD_CONCURRENCY)
        .try_collect()
        .await
    }

    /// Verifies selected Cell rows from a shared bundle and prepares one root append.
    ///
    /// Bundle row identity is routing metadata, not authorization. Only rows using
    /// the canonical Cell/incarnation identity are selected, and their complete LTX
    /// chain is independently verified before the immutable bundle is retained.
    pub async fn prepare_bundle(
        &self,
        base: Option<&RootRef>,
        bundle: &crate::bundle::Bundle,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        let mut replica = self.clone();
        replica.host = self.host.for_dirty().await?;
        replica
            .prepare_bundle_admitted(base, bundle, commit_sequence, schema)
            .await
    }

    /// Prepares the exact successor pinned by a recovered node-log overlay.
    ///
    /// Recovery policy and ownership remain caller-owned. This method accepts
    /// only this replica's Cell/incarnation rows, requires the declared final
    /// position to match the bundle, and reuses normal root preparation.
    pub async fn prepare_recovered_overlay(
        &self,
        overlay: &RecoveryOverlay,
        schema: u32,
    ) -> Result<PreparedRoot> {
        if overlay.predecessor.cell != self.cell
            || overlay.predecessor.incarnation != self.incarnation
            || overlay.final_commit_sequence <= overlay.predecessor.commit_sequence
        {
            return Err(CrabError::InvalidState("recovery overlay scope"));
        }
        let (repository, epoch) = crate::bundle::cell_identity(&self.cell, &self.incarnation);
        let final_position = overlay
            .bundle
            .rows()
            .iter()
            .rfind(|row| row.repository == repository && row.epoch == epoch)
            .map(|row| row.info.position())
            .ok_or(CrabError::TxNotAvailable)?;
        if final_position != overlay.final_position {
            return Err(CrabError::ChecksumMismatch);
        }
        let prepared = self
            .prepare_bundle(
                Some(&overlay.predecessor),
                &overlay.bundle,
                overlay.final_commit_sequence,
                schema,
            )
            .await?;
        if prepared.root().position != overlay.final_position {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(prepared)
    }

    async fn prepare_bundle_admitted(
        &self,
        base: Option<&RootRef>,
        bundle: &crate::bundle::Bundle,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        self.validate_metadata(commit_sequence, schema)?;
        if bundle.len() > self.limits.max_plan_bytes {
            return Err(CrabError::Limit("Cell bundle bytes"));
        }
        let base_graph = match base {
            Some(root) => Some(self.load_graph(root).await?),
            None => None,
        };
        self.validate_append_sequence(&base_graph, commit_sequence)?;

        let (repository, epoch) = crate::bundle::cell_identity(&self.cell, &self.incarnation);
        let bundle_digest = bundle.digest();
        let mut inputs = Vec::new();
        let mut selected_bytes = 0_u64;
        let mut prospective = base_graph
            .as_ref()
            .map(|graph| graph.descriptors.clone())
            .unwrap_or_default();
        for (index, row) in bundle.rows().iter().enumerate() {
            if row.repository != repository || row.epoch != epoch {
                continue;
            }
            selected_bytes = selected_bytes
                .checked_add(row.info.size_bytes)
                .ok_or(CrabError::Limit("captured Cell bundle bytes"))?;
            if selected_bytes > self.limits.max_capture_bytes {
                return Err(CrabError::Limit("captured Cell bundle bytes"));
            }
            prospective.push(SegmentDescriptor::bundled(
                row.info.clone(),
                [0; 32],
                0,
                bundle_digest,
                row.offset,
            ));
            let bytes = bundle.read_segment(index)?;
            let (file, size, digest, pages) = crate::ltx::inspect_bytes_with_index(&bytes)?;
            if size != row.info.size_bytes
                || digest != row.info.blake3
                || crate::SegmentInfo::from_inspected(&file, size, digest) != row.info
            {
                return Err(CrabError::ChecksumMismatch);
            }
            let index_bytes = crate::paged::encode_index_from_pages(&pages)?;
            inputs.push(AppendInput {
                info: row.info.clone(),
                location: BodyLocation::Bundle {
                    digest: bundle_digest,
                    offset: row.offset,
                },
                index: index_bytes,
                body: AppendBody::Bundle,
            });
        }
        let target = inputs
            .last()
            .map(|input| input.info.position())
            .ok_or(CrabError::TxNotAvailable)?;
        self.validate_chain(&prospective, target)?;
        self.prepare_append(
            base,
            base_graph,
            inputs,
            target,
            commit_sequence,
            schema,
            Some(bundle),
        )
        .await
    }

    /// Prepares an exact representation-only compaction of a pinned root.
    ///
    /// The output retains the base TXID, checksum, commit sequence and schema.
    /// Only the authority owner may later publish the proposal as a normal root CAS.
    /// `scratch_directory` must already exist, be private to the caller and have
    /// space for selected bodies and indexes plus compacted LTX/index outputs.
    /// Owned scratch files are removed after success or failure.
    pub async fn prepare_compaction(
        &self,
        base: &RootRef,
        range: std::ops::Range<usize>,
        level: u8,
        scratch_directory: &Path,
    ) -> Result<PreparedRoot> {
        let started = self.host.now_monotonic();
        let result = async {
            let mut replica = self.clone();
            replica.host = self.host.for_recovery().await?;
            let graph = replica.load_graph(base).await?;
            let scratch_bytes = compaction_scratch_bytes(&graph, range.clone())?;
            replica.host = replica.host.for_scratch(scratch_bytes).await?;
            compaction::prepare(&replica, base, graph, range, level, scratch_directory).await
        }
        .await;
        self.host
            .observe_ltx_phase(crate::LtxPhase::Compaction, started, result.is_ok());
        result
    }

    /// Prepares one bounded level promotion, or an emergency full compaction.
    ///
    /// Normal promotions require eight contiguous inputs from the preceding
    /// level. A root near its segment or byte ceiling is compacted completely so
    /// the next append cannot strand an otherwise healthy writer at admission.
    pub async fn prepare_scheduled_compaction(
        &self,
        base: &RootRef,
        scratch_directory: &Path,
    ) -> Result<Option<PreparedRoot>> {
        let started = self.host.now_monotonic();
        let result = self
            .prepare_scheduled_compaction_inner(base, scratch_directory)
            .await;
        self.host
            .observe_ltx_phase(crate::LtxPhase::Compaction, started, result.is_ok());
        result
    }

    async fn prepare_scheduled_compaction_inner(
        &self,
        base: &RootRef,
        scratch_directory: &Path,
    ) -> Result<Option<PreparedRoot>> {
        let mut replica = self.clone();
        replica.host = self.host.for_recovery().await?;
        let graph = replica.load_graph(base).await?;
        let segment_limit = MAX_SEGMENTS.min(replica.limits.max_segments);
        let stored_bytes = graph
            .descriptors
            .iter()
            .try_fold(0_u64, |total, descriptor| {
                total
                    .checked_add(descriptor.info.size_bytes)
                    .and_then(|value| value.checked_add(descriptor.index_length))
                    .ok_or(CrabError::Limit("Cell root bytes"))
            })?;
        let byte_pressure = stored_bytes >= replica.limits.max_plan_bytes.saturating_mul(3) / 4;
        let selected = if graph.descriptors.len() > 1
            && (graph.descriptors.len() >= segment_limit.saturating_sub(1).max(1) || byte_pressure)
        {
            let end = graph.descriptors.len();
            Some((0..end, 9))
        } else {
            let mut selected = None;
            for level in 1..=8 {
                if let Some(range) = scheduled_compaction_range(
                    &graph.descriptors,
                    level,
                    replica.limits.max_file_bytes,
                ) {
                    selected = Some((range, level));
                    break;
                }
            }
            selected
        };
        let Some((range, level)) = selected else {
            return Ok(None);
        };
        let scratch_bytes = compaction_scratch_bytes(&graph, range.clone())?;
        replica.host = replica.host.for_scratch(scratch_bytes).await?;
        compaction::prepare(&replica, base, graph, range, level, scratch_directory)
            .await
            .map(Some)
    }

    async fn prepare_append(
        &self,
        base: Option<&RootRef>,
        base_graph: Option<LoadedGraph>,
        inputs: Vec<AppendInput>,
        target: Position,
        commit_sequence: u64,
        schema: u32,
        bundle: Option<&crate::bundle::Bundle>,
    ) -> Result<PreparedRoot> {
        let prepared = inputs
            .into_iter()
            .map(|input| {
                let digest = *blake3::hash(&input.index).as_bytes();
                let descriptor = match input.location {
                    BodyLocation::Native => {
                        SegmentDescriptor::native(input.info, digest, input.index.len() as u64)
                    }
                    BodyLocation::Bundle { digest, offset } => SegmentDescriptor::bundled(
                        input.info,
                        *blake3::hash(&input.index).as_bytes(),
                        input.index.len() as u64,
                        digest,
                        offset,
                    ),
                };
                Ok(PreparedSegment {
                    descriptor,
                    index: input.index,
                    body: input.body,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let mut descriptors = base_graph
            .as_ref()
            .map(|graph| graph.descriptors.clone())
            .unwrap_or_default();
        descriptors.extend(prepared.iter().map(|segment| segment.descriptor.clone()));
        self.validate_chain(&descriptors, target)?;
        let directory_inputs = prepared
            .iter()
            .map(|segment| DirectoryInput {
                descriptor: segment.descriptor.clone(),
                index: segment.index.clone(),
            })
            .collect::<Vec<_>>();
        let dependency_uploads = async {
            if let Some(bundle) = bundle {
                self.put_bundle(bundle).await?;
            }
            stream::iter(
                prepared
                    .into_iter()
                    .map(|segment| self.upload_prepared_segment(segment)),
            )
            .buffered(SEGMENT_UPLOAD_CONCURRENCY)
            .try_collect::<Vec<_>>()
            .await?;
            Ok::<(), CrabError>(())
        };
        let root_preparation = self.finish_preparation(
            base,
            base_graph,
            descriptors,
            &directory_inputs,
            target,
            commit_sequence,
            schema,
        );
        // Content-addressed dependencies and root metadata can upload in
        // parallel. The private proposal is returned only after both branches
        // finish, so a failed branch can leave only unreachable objects.
        let (_, prepared) =
            futures_util::future::try_join(dependency_uploads, root_preparation).await?;
        Ok(prepared)
    }

    async fn finish_preparation(
        &self,
        base: Option<&RootRef>,
        base_graph: Option<LoadedGraph>,
        descriptors: Vec<SegmentDescriptor>,
        directory_inputs: &[DirectoryInput],
        target: Position,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        for descriptor in &descriptors {
            descriptor.validate_published(self.limits)?;
        }
        let endpoint = descriptors.last().ok_or(CrabError::LTXCorrupted)?;
        let page_size = endpoint.info.page_size;
        let database_pages = endpoint.info.database_pages;
        let extents = object_extents(&descriptors)?;
        let directory = if let Some(graph) = &base_graph {
            let (changes, retain_through) =
                directory_changes(directory_inputs, graph.document.database_pages)?;
            let base_extents = object_extents(&graph.descriptors)?;
            DirectoryTree::update(
                directory::Verification {
                    layout: &self.layout,
                    cell: &self.cell,
                    incarnation: &self.incarnation,
                    page_size: graph.document.page_size,
                    database_pages: graph.document.database_pages,
                    extents: &base_extents,
                    host: &self.host,
                    origin: crate::LtxReadOrigin::Cold,
                },
                graph.document.directory_digest,
                graph.document.directory_height,
                graph.aggregate,
                changes,
                retain_through,
                directory::Verification {
                    layout: &self.layout,
                    cell: &self.cell,
                    incarnation: &self.incarnation,
                    page_size,
                    database_pages,
                    extents: &extents,
                    host: &self.host,
                    origin: crate::LtxReadOrigin::Cold,
                },
                target.checksum,
            )
            .await?
        } else {
            let entries = directory::initial_entries(directory_inputs)?;
            let directory =
                directory::build_initial_and_upload(entries, page_size, database_pages, self)
                    .await?;
            if directory.checksum() != target.checksum {
                return Err(CrabError::ChecksumMismatch);
            }
            directory
        };
        self.put_objects(
            CellObjectKind::Directory,
            directory
                .objects()
                .iter()
                .map(|node| (node.digest, node.bytes.clone()))
                .collect(),
        )
        .await?;

        self.finish_root(
            base,
            descriptors,
            target,
            commit_sequence,
            schema,
            page_size,
            database_pages,
            directory,
        )
        .await
    }

    #[expect(clippy::too_many_arguments)]
    async fn finish_root(
        &self,
        base: Option<&RootRef>,
        descriptors: Vec<SegmentDescriptor>,
        target: Position,
        commit_sequence: u64,
        schema: u32,
        page_size: u32,
        database_pages: u32,
        directory: DirectoryTree,
    ) -> Result<PreparedRoot> {
        if directory.checksum() != target.checksum {
            return Err(CrabError::ChecksumMismatch);
        }

        let mut segment_pages = Vec::new();
        let mut root_objects = Vec::new();
        for page in descriptors.chunks(SEGMENTS_PER_PAGE) {
            let bytes = encode_segment_page(page)?;
            let digest = *blake3::hash(&bytes).as_bytes();
            root_objects.push((digest, bytes));
            segment_pages.push(digest);
        }
        if segment_pages.len() > MAX_SEGMENT_PAGES {
            return Err(CrabError::Limit("Cell root segment pages"));
        }
        let document = RootDocument {
            cell: self.cell,
            checksum: target.checksum,
            commit_sequence,
            database_pages,
            directory_digest: directory.root_digest(),
            directory_height: directory.height(),
            incarnation: self.incarnation,
            page_size,
            schema,
            segment_pages,
            txid: target.txid,
        };
        let bytes = encode_root(&document)?;
        let digest = *blake3::hash(&bytes).as_bytes();
        root_objects.push((digest, bytes));
        // The document and its immutable segment pages can be uploaded in
        // parallel. The root digest remains private until all uploads finish.
        self.put_objects(CellObjectKind::Root, root_objects).await?;
        let root = RootRef {
            cell: self.cell,
            incarnation: self.incarnation,
            digest,
            position: target,
            commit_sequence,
        };
        let host = self
            .host
            .clone()
            .without_recovery()
            .without_dirty()
            .without_scratch();
        Ok(PreparedRoot {
            predecessor: base.copied(),
            verified: VerifiedRoot::from_graph(
                self.clone().with_host(host),
                root,
                &document,
                descriptors,
            )?,
        })
    }

    fn validate_metadata(&self, commit_sequence: u64, schema: u32) -> Result<()> {
        if schema == 0 || commit_sequence > i64::MAX as u64 {
            return Err(CrabError::InvalidState("invalid Cell root metadata"));
        }
        Ok(())
    }

    fn validate_append_sequence(
        &self,
        base: &Option<LoadedGraph>,
        commit_sequence: u64,
    ) -> Result<()> {
        if base
            .as_ref()
            .is_some_and(|graph| commit_sequence <= graph.document.commit_sequence)
        {
            return Err(CrabError::InvalidState("commit sequence did not advance"));
        }
        Ok(())
    }

    /// Reopens and verifies an exact immutable root and its metadata graph.
    pub async fn open_root(&self, root: &RootRef) -> Result<VerifiedRoot> {
        let graph = self.load_graph(root).await?;
        VerifiedRoot::from_graph(self.clone(), *root, &graph.document, graph.descriptors)
    }

    /// Verifies and returns the complete immutable dependency set for an exact root.
    ///
    /// Callers may use this bounded inventory for backup pinning and reachability
    /// collection. A missing or corrupt dependency fails the traversal closed.
    pub async fn reachable_objects(&self, root: &RootRef) -> Result<Vec<RootObjectRef>> {
        let graph = self.load_graph(root).await?;
        let extents = object_extents(&graph.descriptors)?;
        let verification = directory::Verification {
            layout: &self.layout,
            cell: &self.cell,
            incarnation: &self.incarnation,
            page_size: graph.document.page_size,
            database_pages: graph.document.database_pages,
            extents: &extents,
            host: &self.host,
            origin: crate::LtxReadOrigin::Cold,
        };
        let directory = directory::reachable_digests(
            verification,
            graph.document.directory_digest,
            graph.document.directory_height,
            graph.aggregate,
        )
        .await?;

        let mut objects = std::collections::BTreeSet::new();
        let mut streamed = std::collections::BTreeMap::new();
        objects.insert(RootObjectRef {
            digest: root.digest,
            kind: CellObjectKind::Root,
        });
        objects.extend(
            graph
                .document
                .segment_pages
                .iter()
                .map(|digest| RootObjectRef {
                    digest: *digest,
                    kind: CellObjectKind::Root,
                }),
        );
        for descriptor in &graph.descriptors {
            let body = RootObjectRef {
                digest: descriptor.object_digest(),
                kind: descriptor.object_kind(),
            };
            let body_limit = match body.kind {
                CellObjectKind::Bundle => self.limits.max_plan_bytes,
                CellObjectKind::Ltx => self.limits.max_file_bytes,
                _ => return Err(CrabError::LTXCorrupted),
            };
            let body_length =
                (body.kind == CellObjectKind::Ltx).then_some(descriptor.info.size_bytes);
            if streamed
                .insert(body, (body_limit, body_length))
                .is_some_and(|previous| previous != (body_limit, body_length))
            {
                return Err(CrabError::LTXCorrupted);
            }
            let index = RootObjectRef {
                digest: descriptor.index_digest,
                kind: CellObjectKind::Index,
            };
            if streamed
                .insert(
                    index,
                    (self.limits.max_plan_bytes, Some(descriptor.index_length)),
                )
                .is_some_and(|(_, length)| length != Some(descriptor.index_length))
            {
                return Err(CrabError::LTXCorrupted);
            }
        }
        for (object, (limit, length)) in &streamed {
            self.verify_remote_object(*object, *limit, *length).await?;
        }
        objects.extend(streamed.into_keys());
        objects.extend(directory.into_iter().map(|digest| RootObjectRef {
            digest,
            kind: CellObjectKind::Directory,
        }));
        Ok(objects.into_iter().collect())
    }

    async fn verify_remote_object(
        &self,
        object: RootObjectRef,
        max_bytes: u64,
        expected_bytes: Option<u64>,
    ) -> Result<()> {
        let path = self.layout.incarnation_object_path(
            &self.cell,
            &self.incarnation,
            &object.digest,
            object.kind,
        );
        let _permit = self.host.io_permit().await?;
        let request = self.layout.store().get_stream(&path, None).await;
        if request.is_err() {
            self.host
                .observe_ltx_origin_request(crate::LtxReadOrigin::Cold, false, 0);
        }
        let (metadata, _, mut stream) = request?;
        if metadata.size > max_bytes || expected_bytes.is_some_and(|size| size != metadata.size) {
            self.host
                .observe_ltx_origin_request(crate::LtxReadOrigin::Cold, true, 0);
            return Err(CrabError::LTXCorrupted);
        }
        let mut digest = blake3::Hasher::new();
        let mut read_bytes = 0usize;
        loop {
            match stream.try_next().await {
                Ok(Some(chunk)) => {
                    digest.update(&chunk);
                    read_bytes = read_bytes.saturating_add(chunk.len());
                }
                Ok(None) => break,
                Err(error) => {
                    self.host.observe_ltx_origin_request(
                        crate::LtxReadOrigin::Cold,
                        false,
                        read_bytes,
                    );
                    return Err(error.into());
                }
            }
        }
        self.host
            .observe_ltx_origin_request(crate::LtxReadOrigin::Cold, true, read_bytes);
        if digest.finalize().as_bytes() != &object.digest {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    async fn load_graph(&self, root: &RootRef) -> Result<LoadedGraph> {
        let started = self.host.now_monotonic();
        let result = self.load_graph_inner(root).await;
        self.host
            .observe_ltx_phase(crate::LtxPhase::RootOpen, started, result.is_ok());
        result
    }

    async fn load_graph_inner(&self, root: &RootRef) -> Result<LoadedGraph> {
        self.check_scope(root)?;
        let bytes = self
            .read_object(&root.digest, CellObjectKind::Root, ROOT_BYTES)
            .await?;
        if *blake3::hash(&bytes).as_bytes() != root.digest {
            return Err(CrabError::ChecksumMismatch);
        }
        let document = decode_root(&bytes)?;
        if document.cell != self.cell
            || document.incarnation != self.incarnation
            || document.txid != root.position.txid
            || document.checksum != root.position.checksum
            || document.commit_sequence != root.commit_sequence
        {
            return Err(CrabError::InvalidState("Cell root reference mismatch"));
        }
        if document.segment_pages.is_empty() || document.segment_pages.len() > MAX_SEGMENT_PAGES {
            return Err(CrabError::LTXCorrupted);
        }
        let pages: Vec<Vec<SegmentDescriptor>> = stream::iter(
            document
                .segment_pages
                .iter()
                .copied()
                .map(|digest| async move {
                    let bytes = self
                        .read_object(&digest, CellObjectKind::Root, SEGMENT_PAGE_BYTES)
                        .await?;
                    if *blake3::hash(&bytes).as_bytes() != digest {
                        return Err(CrabError::ChecksumMismatch);
                    }
                    let page = decode_segment_page(&bytes)?;
                    if page.is_empty() || page.len() > SEGMENTS_PER_PAGE {
                        return Err(CrabError::LTXCorrupted);
                    }
                    Ok(page)
                }),
        )
        .buffered(OBJECT_FETCH_CONCURRENCY)
        .try_collect()
        .await?;
        let descriptors = pages.into_iter().flatten().collect::<Vec<_>>();
        self.validate_chain(&descriptors, root.position)?;
        for descriptor in &descriptors {
            descriptor.validate_published(self.limits)?;
        }
        let endpoint = descriptors.last().ok_or(CrabError::LTXCorrupted)?;
        if document.page_size != endpoint.info.page_size
            || document.database_pages != endpoint.info.database_pages
            || document.schema == 0
        {
            return Err(CrabError::LTXCorrupted);
        }
        let extents = object_extents(&descriptors)?;
        let directory_started = self.host.now_monotonic();
        let aggregate = directory::verify_root(
            directory::Verification {
                layout: &self.layout,
                cell: &self.cell,
                incarnation: &self.incarnation,
                page_size: document.page_size,
                database_pages: document.database_pages,
                extents: &extents,
                host: &self.host,
                origin: crate::LtxReadOrigin::Cold,
            },
            document.directory_digest,
            document.directory_height,
        )
        .await;
        self.host.observe_ltx_phase(
            crate::LtxPhase::Directory,
            directory_started,
            aggregate.is_ok(),
        );
        let aggregate = aggregate?;
        if aggregate.checksum != document.checksum {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(LoadedGraph {
            aggregate,
            document,
            descriptors,
        })
    }

    fn validate_chain(&self, descriptors: &[SegmentDescriptor], target: Position) -> Result<()> {
        if descriptors.is_empty() || descriptors.len() > MAX_SEGMENTS.min(self.limits.max_segments)
        {
            return Err(CrabError::Limit("Cell root segments"));
        }
        let mut previous = Position::default();
        let mut page_size = None;
        let mut total = 0u64;
        for descriptor in descriptors {
            descriptor.validate(self.limits)?;
            let info = &descriptor.info;
            total = total
                .checked_add(info.size_bytes)
                .and_then(|value| value.checked_add(descriptor.index_length))
                .ok_or(CrabError::Limit("Cell root bytes"))?;
            if total > self.limits.max_plan_bytes
                || previous.txid.checked_add(1) != Some(info.min_txid)
                || info.pre_checksum != previous.checksum
                || page_size.is_some_and(|value| value != info.page_size)
            {
                return Err(CrabError::LTXCorrupted);
            }
            previous = info.position();
            page_size = Some(info.page_size);
        }
        if previous != target {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    fn check_scope(&self, root: &RootRef) -> Result<()> {
        if root.cell != self.cell || root.incarnation != self.incarnation {
            return Err(CrabError::InvalidState(
                "root belongs to another Cell incarnation",
            ));
        }
        Ok(())
    }

    async fn put_object(
        &self,
        digest: &[u8; 32],
        kind: CellObjectKind,
        bytes: Vec<u8>,
    ) -> Result<()> {
        self.put_object_bytes(digest, kind, Bytes::from(bytes))
            .await
    }

    async fn put_object_bytes(
        &self,
        digest: &[u8; 32],
        kind: CellObjectKind,
        bytes: Bytes,
    ) -> Result<()> {
        if *blake3::hash(&bytes).as_bytes() != *digest {
            return Err(CrabError::ChecksumMismatch);
        }
        let _permit = self.host.io_permit().await?;
        let path = self
            .layout
            .incarnation_object_path(&self.cell, &self.incarnation, digest, kind);
        self.layout.store().put(&path, bytes.clone()).await?;
        if kind == CellObjectKind::Directory {
            directory::cache_uploaded(
                &self.layout,
                &self.cell,
                &self.incarnation,
                *digest,
                &bytes,
            )?;
        }
        Ok(())
    }

    async fn put_objects(
        &self,
        kind: CellObjectKind,
        objects: Vec<([u8; 32], Vec<u8>)>,
    ) -> Result<()> {
        stream::iter(
            objects
                .into_iter()
                .map(|(digest, bytes)| async move { self.put_object(&digest, kind, bytes).await }),
        )
        .buffer_unordered(OBJECT_UPLOAD_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
        Ok(())
    }

    async fn upload_prepared_segment(&self, segment: PreparedSegment) -> Result<()> {
        let PreparedSegment {
            descriptor,
            index,
            body,
        } = segment;
        let body_upload = async {
            if descriptor.object_kind() != CellObjectKind::Ltx {
                return Ok(());
            }
            let AppendBody::Native(source) = body else {
                return Err(CrabError::InvalidState("native Cell body source missing"));
            };
            compaction::upload_source(
                self,
                source,
                descriptor.info.size_bytes,
                &descriptor.info.blake3,
                CellObjectKind::Ltx,
            )
            .await
        };
        let index_upload = self.put_object(&descriptor.index_digest, CellObjectKind::Index, index);
        futures_util::future::try_join(body_upload, index_upload).await?;
        Ok(())
    }

    async fn put_bundle(&self, bundle: &crate::bundle::Bundle) -> Result<()> {
        let digest = bundle.digest();
        if bundle.len() > self.limits.max_plan_bytes {
            return Err(CrabError::Limit("Cell bundle bytes"));
        }
        let path = self.layout.incarnation_object_path(
            &self.cell,
            &self.incarnation,
            &digest,
            CellObjectKind::Bundle,
        );
        let staged = self.layout.incarnation_staging_path(
            &self.cell,
            &self.incarnation,
            &digest,
            CellObjectKind::Bundle,
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        let _permit = self.host.io_permit().await?;
        let upload = self
            .layout
            .store()
            .put_multipart_source_retry(
                &staged,
                bundle.upload_source(),
                bundle.len(),
                digest,
                MULTIPART_BYTES,
                &cancel,
                None,
            )
            .await;
        if let Err(error) = upload {
            return match cleanup_staged(self.layout.store(), &staged).await {
                Ok(()) => Err(error.into()),
                Err(cleanup_error) => Err(cleanup_error),
            };
        }
        let promotion = self
            .layout
            .store()
            .promote_staged_content_addressed_object(&staged, &path, digest, bundle.len())
            .await;
        match cleanup_staged(self.layout.store(), &staged).await {
            Err(error) => Err(error),
            Ok(()) => promotion.map(|_| ()).map_err(Into::into),
        }
    }

    async fn read_object(
        &self,
        digest: &[u8; 32],
        kind: CellObjectKind,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let _permit = self.host.io_permit().await?;
        let path = self
            .layout
            .incarnation_object_path(&self.cell, &self.incarnation, digest, kind);
        let result = self
            .layout
            .store()
            .get_with_etag_bounded(&path, max_bytes)
            .await;
        self.host.observe_ltx_origin_request(
            crate::LtxReadOrigin::Cold,
            result.is_ok(),
            result.as_ref().map_or(0, |(bytes, _)| bytes.len()),
        );
        let (bytes, _) = result?;
        Ok(bytes.to_vec())
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
                .ok_or(CrabError::Limit("scratch disk bytes"))
        })?;
    selected.iter().try_fold(indexes, |total, descriptor| {
        total
            .checked_add(descriptor.info.size_bytes)
            .ok_or(CrabError::Limit("scratch disk bytes"))
    })
}

struct AppendInput {
    info: crate::SegmentInfo,
    location: BodyLocation,
    index: Vec<u8>,
    body: AppendBody,
}

enum AppendBody {
    Native(Arc<PinnedCapture>),
    Bundle,
}

#[derive(Clone, Copy)]
enum BodyLocation {
    Native,
    Bundle { digest: [u8; 32], offset: u64 },
}

struct PreparedSegment {
    descriptor: SegmentDescriptor,
    index: Vec<u8>,
    body: AppendBody,
}

struct DirectoryInput {
    descriptor: SegmentDescriptor,
    index: Vec<u8>,
}

const MULTIPART_BYTES: usize = 8 << 20;

async fn cleanup_staged(
    store: &crab_storage::Store,
    path: &object_store::path::Path,
) -> Result<()> {
    match store.delete(path).await {
        Ok(()) | Err(crab_storage::StorageError::NotFound { .. }) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

struct PinnedCapture {
    host: Host,
    file: Arc<Mutex<Box<dyn crate::environment::FileIo>>>,
    size: u64,
}

impl PinnedCapture {
    async fn open(host: &Host, path: PathBuf, expected_size: u64) -> Result<Arc<Self>> {
        let source_host = host.clone();
        let filesystem = Arc::clone(&host.filesystem);
        host.run(move || {
            let file = filesystem.open(&path)?;
            if file.file_len()? != expected_size {
                return Err(CrabError::ChecksumMismatch);
            }
            Ok(Arc::new(Self {
                host: source_host,
                file: Arc::new(Mutex::new(file)),
                size: expected_size,
            }))
        })
        .await?
    }

    fn read_exact(&self, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| io::Error::other("capture file lock poisoned"))?;
        file.read_exact_at(offset, length)
    }
}

struct PinnedCaptureReader {
    source: Arc<PinnedCapture>,
    offset: u64,
}

impl io::Read for PinnedCaptureReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let remaining = self.source.size.saturating_sub(self.offset);
        let length =
            usize::try_from(remaining.min(bytes.len() as u64)).map_err(io::Error::other)?;
        if length == 0 {
            return Ok(0);
        }
        let read = self.source.read_exact(self.offset, length)?;
        bytes[..length].copy_from_slice(&read);
        self.offset = self
            .offset
            .checked_add(length as u64)
            .ok_or_else(|| io::Error::other("capture offset overflow"))?;
        Ok(length)
    }
}

#[async_trait::async_trait]
impl crab_storage::MultipartUploadSource for PinnedCapture {
    async fn byte_len(&self) -> crab_storage::Result<u64> {
        let file = Arc::clone(&self.file);
        self.host
            .run(move || {
                let file = file
                    .lock()
                    .map_err(|_| io::Error::other("capture file lock poisoned"))?;
                file.file_len()
            })
            .await
            .map_err(pinned_storage_error)?
            .map_err(|error| crab_storage::StorageError::ReadRejected {
                source: Box::new(error),
            })
    }

    async fn read_exact(&self, offset: u64, length: usize) -> crab_storage::Result<Bytes> {
        let file = Arc::clone(&self.file);
        self.host
            .run(move || {
                let mut file = file
                    .lock()
                    .map_err(|_| io::Error::other("capture file lock poisoned"))?;
                file.read_exact_at(offset, length).map(Bytes::from)
            })
            .await
            .map_err(pinned_storage_error)?
            .map_err(|error| crab_storage::StorageError::ReadRejected {
                source: Box::new(error),
            })
    }
}

async fn inspect_segment_source(
    replica: &CellReplica,
    source: Arc<PinnedCapture>,
    expected: &crate::SegmentInfo,
) -> Result<Vec<u8>> {
    let expected = expected.clone();
    replica
        .host
        .run(move || {
            let reader = PinnedCaptureReader { source, offset: 0 };
            let (file, size, digest, pages) = crate::ltx::inspect_reader_with_index(reader)?;
            if size != expected.size_bytes || digest != expected.blake3 {
                return Err(CrabError::ChecksumMismatch);
            }
            if crate::SegmentInfo::from_inspected(&file, size, digest) != expected {
                return Err(CrabError::ChecksumMismatch);
            }
            crate::paged::encode_index_from_pages(&pages)
        })
        .await?
}

fn pinned_storage_error(error: CrabError) -> crab_storage::StorageError {
    crab_storage::StorageError::ReadRejected {
        source: Box::new(error),
    }
}

impl VerifiedRoot {
    fn from_graph(
        replica: CellReplica,
        root: RootRef,
        document: &RootDocument,
        descriptors: Vec<SegmentDescriptor>,
    ) -> Result<Self> {
        let extents = object_extents(&descriptors)?;
        Ok(Self {
            root,
            page_size: document.page_size,
            database_pages: document.database_pages,
            schema: document.schema,
            segment_count: descriptors.len(),
            directory_height: document.directory_height,
            pages: CellPagedDatabase {
                replica,
                directory_digest: document.directory_digest,
                directory_height: document.directory_height,
                extents: Arc::new(extents),
                page_size: document.page_size,
                database_pages: document.database_pages,
                position: root.position,
            },
        })
    }
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

#[cfg(all(test, unix))]
mod tests {
    use std::io::Read as _;

    use super::{Host, PinnedCapture, PinnedCaptureReader};

    #[tokio::test]
    async fn pinned_capture_ignores_later_path_replacement() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("capture.ltx");
        let displaced = directory.path().join("original.ltx");
        let original = b"verified capture bytes";
        std::fs::write(&path, original).unwrap();
        let source = PinnedCapture::open(&Host::default(), path.clone(), original.len() as u64)
            .await
            .unwrap();

        std::fs::rename(&path, &displaced).unwrap();
        std::fs::write(&path, b"replacement contents!").unwrap();

        let mut reader = PinnedCaptureReader { source, offset: 0 };
        let mut observed = Vec::new();
        reader.read_to_end(&mut observed).unwrap();
        assert_eq!(observed, original);
    }
}
