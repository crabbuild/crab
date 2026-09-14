//! Immutable Cell-scoped LTX roots prepared independently of ownership CAS.

use std::collections::BTreeMap;

use bytes::Bytes;
use crab_storage::{CellObjectKind, CellStorageLayout};

use crate::{CaptureBatch, CrabError, Host, Limits, Position, Result};

mod directory;
mod root;

use directory::{DirectoryEntry, DirectoryTree};
use root::{
    RootDocument, SegmentDescriptor, decode_root, decode_segment_page, encode_root,
    encode_segment_page,
};

const ROOT_BYTES: u64 = 32 << 10;
const SEGMENT_PAGE_BYTES: u64 = 64 << 10;
const MAX_SEGMENTS: usize = 4096;
const SEGMENTS_PER_PAGE: usize = 96;
const MAX_SEGMENT_PAGES: usize = 64;

/// An immutable Cell root identity suitable for publication in control state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootRef {
    pub cell: [u8; 32],
    pub incarnation: [u8; 16],
    pub digest: [u8; 32],
    pub position: Position,
    pub commit_sequence: u64,
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

    /// Selects the caller's bounded I/O and blocking execution facilities.
    #[must_use]
    pub fn with_host(mut self, host: Host) -> Self {
        self.host = host;
        self
    }

    /// Verifies and uploads a new immutable root without changing authority.
    pub async fn prepare(
        &self,
        base: Option<&RootRef>,
        cuts: &CaptureBatch,
        commit_sequence: u64,
        schema: u32,
    ) -> Result<PreparedRoot> {
        if schema == 0 || commit_sequence > i64::MAX as u64 || cuts.segments.is_empty() {
            return Err(CrabError::InvalidState("invalid Cell root metadata"));
        }
        let base_graph = match base {
            Some(root) => Some(self.load_graph(root).await?),
            None => None,
        };
        if let Some(graph) = &base_graph
            && commit_sequence <= graph.document.commit_sequence
        {
            return Err(CrabError::InvalidState("commit sequence did not advance"));
        }

        // Admit the complete prospective chain from trusted capture metadata
        // before reading local bodies or starting immutable uploads.
        let mut descriptors = base_graph
            .as_ref()
            .map(|graph| graph.descriptors.clone())
            .unwrap_or_default();
        let base_len = descriptors.len();
        descriptors.extend(
            cuts.segments
                .iter()
                .map(|segment| SegmentDescriptor::native(segment.info().clone(), [0; 32], 0)),
        );
        self.validate_chain(&descriptors, cuts.position)?;

        let local = cuts.segments.clone();
        let host = self.host.clone();
        let limits = self.limits;
        let prepared = self
            .host
            .run(move || {
                local
                    .into_iter()
                    .map(|segment| {
                        let bytes = host.read(segment.path(), segment.info().size_bytes)?;
                        crate::recovery::verify_segment(&bytes, segment.info(), limits)?;
                        let index = crate::paged::encode_index(&bytes)?;
                        Ok((bytes, segment.info().clone(), index))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .await??;

        descriptors.truncate(base_len);
        descriptors.extend(prepared.iter().map(|(_, info, index)| {
            SegmentDescriptor::native(
                info.clone(),
                *blake3::hash(index).as_bytes(),
                index.len() as u64,
            )
        }));
        self.validate_chain(&descriptors, cuts.position)?;
        for (bytes, info, index) in &prepared {
            let index_digest = *blake3::hash(index).as_bytes();
            self.put_object(&info.blake3, CellObjectKind::Ltx, bytes.clone())
                .await?;
            self.put_object(&index_digest, CellObjectKind::Index, index.clone())
                .await?;
        }

        let (entries, page_size, database_pages) = self.directory_entries(&descriptors).await?;
        let directory = DirectoryTree::build(entries, page_size, database_pages)?;
        for node in directory.objects() {
            self.put_object(&node.digest, CellObjectKind::Directory, node.bytes.clone())
                .await?;
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
            checksum: cuts.position.checksum,
            commit_sequence,
            database_pages,
            directory_digest: directory.root_digest(),
            directory_height: directory.height(),
            incarnation: self.incarnation,
            page_size,
            schema,
            segment_pages,
            txid: cuts.position.txid,
        };
        let bytes = encode_root(&document)?;
        let digest = *blake3::hash(&bytes).as_bytes();
        self.put_object(&digest, CellObjectKind::Root, bytes)
            .await?;
        let root = RootRef {
            cell: self.cell,
            incarnation: self.incarnation,
            digest,
            position: cuts.position,
            commit_sequence,
        };
        Ok(PreparedRoot {
            predecessor: base.copied(),
            verified: VerifiedRoot::from_graph(root, &document, descriptors.len()),
        })
    }

    /// Reopens and verifies an exact immutable root and its metadata graph.
    pub async fn open_root(&self, root: &RootRef) -> Result<VerifiedRoot> {
        let graph = self.load_graph(root).await?;
        Ok(VerifiedRoot::from_graph(
            *root,
            &graph.document,
            graph.descriptors.len(),
        ))
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
        let endpoint = descriptors.last().ok_or(CrabError::LTXCorrupted)?;
        if document.page_size != endpoint.info.page_size
            || document.database_pages != endpoint.info.database_pages
            || document.schema == 0
        {
            return Err(CrabError::LTXCorrupted);
        }
        let aggregate = directory::verify_tree(
            directory::Verification {
                layout: &self.layout,
                cell: &self.cell,
                incarnation: &self.incarnation,
                page_size: document.page_size,
                database_pages: document.database_pages,
                descriptors: &descriptors,
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
            document,
            descriptors,
        })
    }

    async fn directory_entries(
        &self,
        descriptors: &[SegmentDescriptor],
    ) -> Result<(BTreeMap<u32, DirectoryEntry>, u32, u32)> {
        let mut entries = BTreeMap::new();
        let mut page_size = 0;
        let mut database_pages = 0;
        for descriptor in descriptors {
            page_size = descriptor.info.page_size;
            database_pages = descriptor.info.database_pages;
            entries.retain(|page, _| *page <= database_pages);
            let bytes = self
                .read_object(
                    &descriptor.index_digest,
                    CellObjectKind::Index,
                    descriptor.index_length,
                )
                .await?;
            if bytes.len() as u64 != descriptor.index_length
                || *blake3::hash(&bytes).as_bytes() != descriptor.index_digest
            {
                return Err(CrabError::ChecksumMismatch);
            }
            let object = descriptor.object_digest();
            let base = descriptor.offset();
            for entry in crate::paged::decode_index(&bytes)? {
                let offset = base
                    .checked_add(entry.offset)
                    .ok_or(CrabError::LTXCorrupted)?;
                entries.insert(
                    entry.page,
                    DirectoryEntry {
                        page: entry.page,
                        object,
                        offset,
                        length: u32::try_from(entry.size).map_err(|_| CrabError::LTXCorrupted)?,
                        frame_hash: entry.hash,
                        checksum: entry.checksum,
                    },
                );
            }
            let expected = u64::from(database_pages)
                - u64::from(crate::ltx::lock_pgno(page_size) <= database_pages);
            let checksum =
                entries.values().fold(0, |sum, entry| sum ^ entry.checksum) | crate::CHECKSUM_FLAG;
            if entries.len() as u64 != expected || checksum != descriptor.info.post_checksum {
                return Err(CrabError::ChecksumMismatch);
            }
        }
        Ok((entries, page_size, database_pages))
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
        if *blake3::hash(&bytes).as_bytes() != *digest {
            return Err(CrabError::ChecksumMismatch);
        }
        let _permit = self.host.io_permit().await?;
        let path = self
            .layout
            .incarnation_object_path(&self.cell, &self.incarnation, digest, kind);
        self.layout.store().put(&path, Bytes::from(bytes)).await?;
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

struct LoadedGraph {
    document: RootDocument,
    descriptors: Vec<SegmentDescriptor>,
}

impl VerifiedRoot {
    fn from_graph(root: RootRef, document: &RootDocument, segment_count: usize) -> Self {
        Self {
            root,
            page_size: document.page_size,
            database_pages: document.database_pages,
            schema: document.schema,
            segment_count,
            directory_height: document.directory_height,
        }
    }
}
