//! Immutable Cell-scoped LTX roots prepared independently of ownership CAS.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bytes::Bytes;
use crab_storage::{CellObjectKind, CellStorageLayout};
use futures_util::TryStreamExt as _;

use crate::{CaptureBatch, CrabError, Host, Limits, Position, Result};

mod compaction;
mod directory;
mod restore;
mod root;

use directory::{DirectoryEntry, DirectoryTree, ObjectExtent};
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
pub struct CellObjectRef {
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
        }
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
        if page == crate::ltx::lock_pgno(self.page_size) && page <= self.database_pages {
            return Ok(vec![0; self.page_size as usize]);
        }
        let entry = directory::lookup(
            directory::Verification {
                layout: &self.replica.layout,
                cell: &self.replica.cell,
                incarnation: &self.replica.incarnation,
                page_size: self.page_size,
                database_pages: self.database_pages,
                extents: &self.extents,
                host: &self.replica.host,
            },
            self.directory_digest,
            self.directory_height,
            page,
        )
        .await?;
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
        let frame = self
            .replica
            .layout
            .store()
            .range_get(&path, entry.offset..end)
            .await?;
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

    async fn read_run(&self, first: u32, max_pages: u32) -> Result<Vec<(u32, Vec<u8>)>> {
        if max_pages == 0 || first == 0 || first > self.database_pages {
            return Ok(Vec::new());
        }
        let lock = crate::ltx::lock_pgno(self.page_size);
        if first == lock {
            return Ok(vec![(first, self.read_page(first).await?)]);
        }
        let count = max_pages
            .min((1 << 20) / self.page_size)
            .min(self.database_pages - first + 1);
        let verification = directory::Verification {
            layout: &self.replica.layout,
            cell: &self.replica.cell,
            incarnation: &self.replica.incarnation,
            page_size: self.page_size,
            database_pages: self.database_pages,
            extents: &self.extents,
            host: &self.replica.host,
        };
        let first_entry = directory::lookup(
            verification,
            self.directory_digest,
            self.directory_height,
            first,
        )
        .await?;
        let mut end = first_entry
            .offset
            .checked_add(u64::from(first_entry.length))
            .ok_or(CrabError::LTXCorrupted)?;
        let mut entries = vec![first_entry];
        for offset in 1..count {
            let page = first.checked_add(offset).ok_or(CrabError::LTXCorrupted)?;
            if page == lock {
                break;
            }
            let entry = directory::lookup(
                verification,
                self.directory_digest,
                self.directory_height,
                page,
            )
            .await?;
            if entry.object != entries[0].object || entry.offset != end {
                break;
            }
            end = entry
                .offset
                .checked_add(u64::from(entry.length))
                .ok_or(CrabError::LTXCorrupted)?;
            entries.push(entry);
        }
        let extent = self
            .extents
            .get(&entries[0].object)
            .ok_or(CrabError::LTXCorrupted)?;
        let path = self.replica.layout.incarnation_object_path(
            &self.replica.cell,
            &self.replica.incarnation,
            &entries[0].object,
            extent.kind,
        );
        let start = entries[0].offset;
        let _permit = self.replica.host.io_permit().await?;
        let frames = self
            .replica
            .layout
            .store()
            .range_get(&path, start..end)
            .await?;
        if frames.len() as u64 != end - start {
            return Err(CrabError::ChecksumMismatch);
        }
        let mut output = Vec::with_capacity(entries.len());
        for entry in entries {
            let offset =
                usize::try_from(entry.offset - start).map_err(|_| CrabError::LTXCorrupted)?;
            let frame_end = offset
                .checked_add(entry.length as usize)
                .ok_or(CrabError::LTXCorrupted)?;
            let frame = frames
                .get(offset..frame_end)
                .ok_or(CrabError::LTXCorrupted)?;
            if *blake3::hash(frame).as_bytes() != entry.frame_hash {
                return Err(CrabError::ChecksumMismatch);
            }
            let bytes = crate::paged::decode_frame(frame, self.page_size, entry.page)?;
            if crate::ltx::checksum_page(entry.page, &bytes) != entry.checksum {
                return Err(CrabError::ChecksumMismatch);
            }
            output.push((entry.page, bytes));
        }
        Ok(output)
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

    pub(crate) async fn read_run(&self, first: u32, max_pages: u32) -> Result<Vec<(u32, Vec<u8>)>> {
        self.database.read_run(first, max_pages).await
    }

    /// Opens a fresh sparse SQLite file pinned to this exact Cell root.
    pub fn open_writable(self, destination: &std::path::Path) -> Result<crate::ManagedDb> {
        if destination != self.destination {
            return Err(CrabError::InvalidState(
                "writable destination differs from prepared destination",
            ));
        }
        crate::ManagedDb::open_cell_paged(self, destination)
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
    pub fn open_new(&self, destination: &std::path::Path) -> Result<crate::ManagedDb> {
        crate::recovery::reject_sidecars(destination, &self.host)?;
        let mut file = self.host.filesystem.create(destination)?;
        file.sync_all()?;
        self.host.filesystem.sync_parent(destination)?;
        drop(file);
        crate::ManagedDb::open_with_host(destination, self.limits, self.host.clone())
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
        let base_graph = match base {
            Some(root) => Some(self.load_graph(root).await?),
            None => None,
        };
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

        // Capture files are copied into owned, replayable scratch files. The
        // copy is chunked so a large WAL cut never becomes an in-memory upload
        // body and a retry can reopen the same verified source.
        let _scratch = self.host.for_scratch(captured_bytes).await?;
        let mut inputs = Vec::with_capacity(cuts.segments.len());
        for segment in &cuts.segments {
            let source = segment.path().to_owned();
            let info = segment.info().clone();
            let directory = source
                .parent()
                .ok_or(CrabError::InvalidState("capture path has no parent"))?;
            let scratch = ScratchFile::new(&self.host, directory, "segment")?;
            let scratch_path = scratch.path().to_owned();
            let filesystem = Arc::clone(&self.host.filesystem);
            let expected = info.size_bytes;
            self.host
                .run(move || {
                    let mut source_file = filesystem.open(&source)?;
                    let mut destination = filesystem.open_rw(&scratch_path)?;
                    let mut offset = 0_u64;
                    while offset < expected {
                        let length =
                            usize::try_from((expected - offset).min(STREAM_COPY_BYTES as u64))
                                .map_err(io::Error::other)?;
                        let bytes = source_file.read_exact_at(offset, length)?;
                        destination.write_all(&bytes)?;
                        offset = offset
                            .checked_add(length as u64)
                            .ok_or_else(|| io::Error::other("capture offset overflow"))?;
                    }
                    if source_file.file_len()? != expected {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "capture size changed while copying",
                        ));
                    }
                    destination.sync_all()?;
                    Ok::<_, io::Error>(())
                })
                .await??;
            let index = inspect_segment_file(self, scratch.path(), &info).await?;
            inputs.push(AppendInput {
                info,
                location: BodyLocation::Native,
                index,
                body: AppendBody::Native(scratch),
            });
        }
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
        if bundle.bytes().len() as u64 > self.limits.max_plan_bytes {
            return Err(CrabError::Limit("Cell bundle bytes"));
        }
        let base_graph = match base {
            Some(root) => Some(self.load_graph(root).await?),
            None => None,
        };
        self.validate_append_sequence(&base_graph, commit_sequence)?;

        let (repository, epoch) = crate::bundle::cell_identity(&self.cell, &self.incarnation);
        let bundle_digest = *blake3::hash(bundle.bytes()).as_bytes();
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
            let bytes = bundle.segment(index)?;
            let (file, size, digest, pages) = crate::ltx::inspect_bytes_with_index(bytes)?;
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
            Some(bundle.shared_bytes()),
        )
        .await
    }

    /// Prepares an exact representation-only compaction of a pinned root.
    ///
    /// The output retains the base TXID, checksum, commit sequence and schema.
    /// Only the authority owner may later publish the proposal as a normal root CAS.
    /// `scratch_directory` must already exist, be private to the caller and have
    /// space for the selected indexes plus the compacted LTX and authenticated index.
    /// Owned scratch files are removed after success or failure.
    pub async fn prepare_compaction(
        &self,
        base: &RootRef,
        range: std::ops::Range<usize>,
        level: u8,
        scratch_directory: &Path,
    ) -> Result<PreparedRoot> {
        let mut replica = self.clone();
        replica.host = self.host.for_recovery().await?;
        let graph = replica.load_graph(base).await?;
        let scratch_bytes = compaction_scratch_bytes(&graph)?;
        replica.host = replica.host.for_scratch(scratch_bytes).await?;
        compaction::prepare(&replica, base, graph, range, level, scratch_directory).await
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
        let mut replica = self.clone();
        replica.host = self.host.for_recovery().await?;
        let graph = replica.load_graph(base).await?;
        let scratch_bytes = compaction_scratch_bytes(&graph)?;
        replica.host = replica.host.for_scratch(scratch_bytes).await?;
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
        if graph.descriptors.len() > 1
            && (graph.descriptors.len() >= segment_limit.saturating_sub(1).max(1) || byte_pressure)
        {
            let end = graph.descriptors.len();
            return compaction::prepare(&replica, base, graph, 0..end, 9, scratch_directory)
                .await
                .map(Some);
        }
        for level in 1..=8 {
            if let Some(range) =
                scheduled_compaction_range(&graph.descriptors, level, replica.limits.max_file_bytes)
            {
                return compaction::prepare(&replica, base, graph, range, level, scratch_directory)
                    .await
                    .map(Some);
            }
        }
        Ok(None)
    }

    async fn prepare_append(
        &self,
        base: Option<&RootRef>,
        base_graph: Option<LoadedGraph>,
        inputs: Vec<AppendInput>,
        target: Position,
        commit_sequence: u64,
        schema: u32,
        bundle: Option<Bytes>,
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
        if let Some(bytes) = bundle {
            let digest = *blake3::hash(&bytes).as_bytes();
            self.put_object_bytes(&digest, CellObjectKind::Bundle, bytes)
                .await?;
        }
        let mut directory_inputs = Vec::with_capacity(prepared.len());
        for segment in prepared {
            if segment.descriptor.object_kind() == CellObjectKind::Ltx {
                let AppendBody::Native(source) = segment.body else {
                    return Err(CrabError::InvalidState("native Cell body source missing"));
                };
                compaction::upload(
                    self,
                    source.path(),
                    &segment.descriptor.info.blake3,
                    CellObjectKind::Ltx,
                )
                .await?;
            }
            self.put_object(
                &segment.descriptor.index_digest,
                CellObjectKind::Index,
                segment.index.clone(),
            )
            .await?;
            directory_inputs.push(DirectoryInput {
                descriptor: segment.descriptor,
                index: segment.index,
            });
        }
        self.finish_preparation(
            base,
            base_graph,
            descriptors,
            &directory_inputs,
            target,
            commit_sequence,
            schema,
        )
        .await
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
        for node in directory.objects() {
            self.put_object(&node.digest, CellObjectKind::Directory, node.bytes.clone())
                .await?;
        }

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
        for page in descriptors.chunks(SEGMENTS_PER_PAGE) {
            let bytes = encode_segment_page(page)?;
            let digest = *blake3::hash(&bytes).as_bytes();
            self.put_object(&digest, CellObjectKind::Root, bytes)
                .await?;
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
        self.put_object(&digest, CellObjectKind::Root, bytes)
            .await?;
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
    pub async fn reachable_objects(&self, root: &RootRef) -> Result<Vec<CellObjectRef>> {
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
        objects.insert(CellObjectRef {
            digest: root.digest,
            kind: CellObjectKind::Root,
        });
        objects.extend(
            graph
                .document
                .segment_pages
                .iter()
                .map(|digest| CellObjectRef {
                    digest: *digest,
                    kind: CellObjectKind::Root,
                }),
        );
        for descriptor in &graph.descriptors {
            let body = CellObjectRef {
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
            let index = CellObjectRef {
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
        objects.extend(directory.into_iter().map(|digest| CellObjectRef {
            digest,
            kind: CellObjectKind::Directory,
        }));
        Ok(objects.into_iter().collect())
    }

    async fn verify_remote_object(
        &self,
        object: CellObjectRef,
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
        let (metadata, _, mut stream) = self.layout.store().get_stream(&path, None).await?;
        if metadata.size > max_bytes || expected_bytes.is_some_and(|size| size != metadata.size) {
            return Err(CrabError::LTXCorrupted);
        }
        let mut digest = blake3::Hasher::new();
        while let Some(chunk) = stream.try_next().await? {
            digest.update(&chunk);
        }
        if digest.finalize().as_bytes() != &object.digest {
            return Err(CrabError::ChecksumMismatch);
        }
        Ok(())
    }

    async fn load_graph(&self, root: &RootRef) -> Result<LoadedGraph> {
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
        let mut descriptors = Vec::new();
        for digest in &document.segment_pages {
            let bytes = self
                .read_object(digest, CellObjectKind::Root, SEGMENT_PAGE_BYTES)
                .await?;
            if *blake3::hash(&bytes).as_bytes() != *digest {
                return Err(CrabError::ChecksumMismatch);
            }
            let page = decode_segment_page(&bytes)?;
            if page.is_empty() || page.len() > SEGMENTS_PER_PAGE {
                return Err(CrabError::LTXCorrupted);
            }
            descriptors.extend(page);
        }
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
        let aggregate = directory::verify_root(
            directory::Verification {
                layout: &self.layout,
                cell: &self.cell,
                incarnation: &self.incarnation,
                page_size: document.page_size,
                database_pages: document.database_pages,
                extents: &extents,
                host: &self.host,
            },
            document.directory_digest,
            document.directory_height,
        )
        .await?;
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
        let (bytes, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&path, max_bytes)
            .await?;
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

fn compaction_scratch_bytes(graph: &LoadedGraph) -> Result<u64> {
    let base = crate::recovery::full_job_scratch_bytes(
        graph.document.page_size,
        graph.document.database_pages,
    )?;
    graph
        .descriptors
        .iter()
        .try_fold(base, |total, descriptor| {
            total
                .checked_add(descriptor.index_length)
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
    Native(ScratchFile),
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

const STREAM_COPY_BYTES: usize = 1 << 20;

struct ScratchFile {
    filesystem: Arc<dyn crate::environment::FileSystem>,
    path: PathBuf,
}

impl ScratchFile {
    fn new(host: &Host, directory: &Path, label: &str) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        for _ in 0..16 {
            let path = directory.join(format!(
                ".crab-cell-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match host.filesystem.create(&path) {
                Ok(file) => {
                    drop(file);
                    return Ok(Self {
                        filesystem: Arc::clone(&host.filesystem),
                        path,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Cell publication scratch namespace exhausted",
        )
        .into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        if self.filesystem.remove_file(&self.path).is_ok() {
            let _ = self.filesystem.sync_parent(&self.path);
        }
    }
}

async fn inspect_segment_file(
    replica: &CellReplica,
    path: &Path,
    expected: &crate::SegmentInfo,
) -> Result<Vec<u8>> {
    let host = replica.host.clone();
    let limits = replica.limits;
    let path = path.to_owned();
    let expected = expected.clone();
    replica
        .host
        .run(move || {
            let reader = crate::host::LtxHost {
                facilities: host.clone(),
                max_database_bytes: limits.max_database_bytes,
                max_file_bytes: expected.size_bytes,
            }
            .open(&path)?;
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
