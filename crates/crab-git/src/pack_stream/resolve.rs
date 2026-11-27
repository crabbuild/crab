//! Second pass: reconstruct and identify every object of a scanned pack.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use gix_hash::ObjectId;
use gix_object::Kind;
use sha1::{Digest, Sha1};

use super::{
    BaseCache, BoxError, Bytes, EntryKind, PackEntry, PackStreamError, PackTable, Result,
    StreamLimits, TRAILER_BYTES, inflate_entry, object_id,
};
use crate::delta;
use crate::pack_locator::{PackIndexEntry, encode_pack_index_v2, encode_pack_reverse_index};

/// Random access to the same pack bytes, used for bases evicted from memory.
pub trait EntrySource {
    /// Read exactly the pack bytes in `[start, end)`.
    fn read(&mut self, start: u64, end: u64) -> std::result::Result<Vec<u8>, BoxError>;
}

/// One reconstructed object whose identity was computed from its bytes.
pub struct ResolvedObject<'a> {
    /// Position of the object's entry in [`PackTable::entries`].
    pub index: usize,
    pub oid: ObjectId,
    pub kind: Kind,
    pub data: &'a [u8],
}

/// Identities and standard sidecars of a completely resolved pack.
#[derive(Debug)]
pub struct IndexedPack {
    /// Version-2 pack index.
    pub index: Vec<u8>,
    /// Standard reverse index.
    pub reverse_index: Vec<u8>,
    /// Crab object-kind sidecar in pack-offset order.
    pub kinds: Vec<u8>,
    /// The pack trailer's SHA-1 checksum.
    pub checksum: ObjectId,
    pub object_count: u32,
    /// The complete pack length, including its trailer.
    pub size: u64,
    /// Deepest delta chain.
    pub max_depth: u32,
    /// Bases re-read through the entry source after eviction.
    pub reread_bases: u64,
}

/// Second pass over the exact bytes scanned by a [`super::PackScanner`].
pub struct PackResolver<'t> {
    table: &'t PackTable,
    limits: StreamLimits,
    cancelled: Box<dyn Fn() -> bool + Send + 't>,
    // Unconsumed input is `buffer[start..]`; `buffer[0]` is at `buffer_offset`.
    buffer: Vec<u8>,
    start: usize,
    buffer_offset: u64,
    hasher: Sha1,
    hashed: u64,
    next: usize,
    oids: Vec<Option<ObjectId>>,
    kinds: Vec<Option<Kind>>,
    depths: Vec<u32>,
    by_oid: HashMap<ObjectId, u32>,
    // OFS children of each entry, as ascending offsets in compressed-row form.
    child_starts: Vec<u32>,
    child_offsets: Vec<u64>,
    cursors: Vec<u32>,
    cache: BaseCache,
    deferred: Vec<u32>,
    reread_bases: u64,
}

impl<'t> PackResolver<'t> {
    /// Prepare to resolve `table`; `cancelled` is polled between entries and deltas.
    pub fn new(
        table: &'t PackTable,
        limits: StreamLimits,
        cancelled: impl Fn() -> bool + Send + 't,
    ) -> Result<Self> {
        let count = table.entries().len();
        let mut children = vec![0u32; count];
        for entry in table.entries() {
            if let EntryKind::OfsDelta { base } = entry.kind {
                children[base_position(table, base)?] += 1;
            }
        }
        let mut child_starts = Vec::with_capacity(count + 1);
        let mut total = 0u32;
        child_starts.push(0);
        for count in &children {
            total = total
                .checked_add(*count)
                .ok_or(PackStreamError::Limit("delta children"))?;
            child_starts.push(total);
        }
        let mut fill = child_starts[..count].to_vec();
        let mut child_offsets = vec![0u64; total as usize];
        for entry in table.entries() {
            if let EntryKind::OfsDelta { base } = entry.kind {
                let slot = &mut fill[base_position(table, base)?];
                child_offsets[*slot as usize] = entry.offset;
                *slot += 1;
            }
        }
        Ok(Self {
            table,
            limits,
            cancelled: Box::new(cancelled),
            buffer: Vec::new(),
            start: 0,
            buffer_offset: 0,
            hasher: Sha1::new(),
            hashed: 0,
            next: 0,
            oids: vec![None; count],
            kinds: vec![None; count],
            depths: vec![0; count],
            by_oid: HashMap::with_capacity(count),
            child_starts,
            child_offsets,
            cursors: vec![0; count],
            cache: BaseCache::new(limits.max_cache_bytes),
            deferred: Vec::new(),
            reread_bases: 0,
        })
    }

    /// Consume the next pack bytes, reporting each object as soon as it resolves.
    pub fn feed<S>(
        &mut self,
        bytes: &[u8],
        source: &mut dyn EntrySource,
        sink: &mut S,
    ) -> Result<()>
    where
        S: FnMut(ResolvedObject<'_>) -> std::result::Result<(), BoxError>,
    {
        self.check()?;
        let content_end = self.table.size() - TRAILER_BYTES as u64;
        let fed = self.buffer_offset + self.buffer.len() as u64;
        if fed + bytes.len() as u64 > self.table.size() {
            return Err(PackStreamError::Invalid(
                "pack is longer than its first pass",
            ));
        }
        let hashable = usize::try_from(content_end.saturating_sub(self.hashed))
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        self.hasher.update(&bytes[..hashable]);
        self.hashed += hashable as u64;
        self.buffer.drain(..self.start);
        self.buffer_offset += self.start as u64;
        self.start = 0;
        self.buffer.extend_from_slice(bytes);
        let available = self.buffer_offset + self.buffer.len() as u64;
        while let Some(entry) = self.table.entries().get(self.next).copied() {
            if entry.end > available {
                break;
            }
            let from = usize::try_from(entry.offset - self.buffer_offset)
                .map_err(|_| PackStreamError::Invalid("pack entry offset overflows"))?;
            let to = usize::try_from(entry.end - self.buffer_offset)
                .map_err(|_| PackStreamError::Invalid("pack entry offset overflows"))?;
            let frame = std::mem::take(&mut self.buffer);
            let resolved = self.resolve_streamed(self.next, &entry, &frame[from..to], source, sink);
            self.buffer = frame;
            resolved?;
            self.start = to;
            self.next += 1;
            self.check()?;
        }
        Ok(())
    }

    /// Verify the trailer, resolve forward reference deltas, and build sidecars.
    pub fn finish<S>(mut self, source: &mut dyn EntrySource, sink: &mut S) -> Result<IndexedPack>
    where
        S: FnMut(ResolvedObject<'_>) -> std::result::Result<(), BoxError>,
    {
        if self.next != self.table.entries().len()
            || self.buffer_offset + self.buffer.len() as u64 != self.table.size()
        {
            return Err(PackStreamError::Invalid(
                "pack ended before its first-pass frames",
            ));
        }
        let trailer = &self.buffer[self.start..];
        let checksum = ObjectId::Sha1(self.hasher.clone().finalize().into());
        if trailer != self.table.checksum().as_bytes() || checksum != self.table.checksum() {
            return Err(PackStreamError::Invalid(
                "pack bytes changed between passes",
            ));
        }
        let mut pending = std::mem::take(&mut self.deferred);
        while !pending.is_empty() {
            self.check()?;
            let mut waiting = Vec::new();
            let mut progressed = false;
            for index in pending {
                let entry = self.table.entries()[index as usize];
                match self.known_parent(&entry)? {
                    Some(parent) if self.kinds[parent as usize].is_some() => {
                        let frame = self.reread(&entry, source)?;
                        let data_start = (entry.data_offset - entry.offset) as usize;
                        let program = inflate_entry(&frame[data_start..], entry.size)?;
                        let base = self.base_bytes(parent, source)?;
                        let (kind, data, depth) = self.apply(parent, &base, &program)?;
                        self.complete(index as usize, kind, data, depth, sink)?;
                        progressed = true;
                    }
                    _ => waiting.push(index),
                }
            }
            if !progressed {
                let missing = waiting.iter().find_map(|index| {
                    match self.table.entries()[*index as usize].kind {
                        EntryKind::RefDelta { base } if !self.by_oid.contains_key(&base) => {
                            Some(base)
                        }
                        _ => None,
                    }
                });
                return Err(match missing {
                    Some(base) => PackStreamError::MissingBase(base),
                    None => PackStreamError::Invalid("pack delta graph is unresolvable"),
                });
            }
            pending = waiting;
        }
        self.into_indexed()
    }

    fn resolve_streamed<S>(
        &mut self,
        index: usize,
        entry: &PackEntry,
        frame: &[u8],
        source: &mut dyn EntrySource,
        sink: &mut S,
    ) -> Result<()>
    where
        S: FnMut(ResolvedObject<'_>) -> std::result::Result<(), BoxError>,
    {
        if crc32fast::hash(frame) != entry.crc32 {
            return Err(PackStreamError::Invalid(
                "pack bytes changed between passes",
            ));
        }
        let data_start = (entry.data_offset - entry.offset) as usize;
        let inflated = inflate_entry(&frame[data_start..], entry.size)?;
        match entry.kind {
            EntryKind::Base(kind) => self.complete(index, kind, inflated, 0, sink),
            EntryKind::OfsDelta { base } => {
                let parent = base_position(self.table, base)? as u32;
                if self.kinds[parent as usize].is_none() {
                    // A deferred parent resolves after the stream ends.
                    self.advance_child(parent);
                    self.deferred.push(index as u32);
                    return Ok(());
                }
                let bytes = self.base_bytes(parent, source)?;
                self.advance_child(parent);
                let (kind, data, depth) = self.apply(parent, &bytes, &inflated)?;
                self.complete(index, kind, data, depth, sink)
            }
            EntryKind::RefDelta { base } => match self.by_oid.get(&base).copied() {
                Some(parent) => {
                    let bytes = self.base_bytes(parent, source)?;
                    let (kind, data, depth) = self.apply(parent, &bytes, &inflated)?;
                    self.complete(index, kind, data, depth, sink)
                }
                None => {
                    // The base may still appear later in this pack.
                    self.deferred.push(index as u32);
                    Ok(())
                }
            },
        }
    }

    fn apply(&self, parent: u32, base: &[u8], program: &[u8]) -> Result<(Kind, Vec<u8>, u32)> {
        let kind = self.kinds[parent as usize]
            .ok_or(PackStreamError::Invalid("pack delta base is unresolved"))?;
        let depth = self.depths[parent as usize]
            .checked_add(1)
            .filter(|depth| *depth <= self.limits.max_delta_depth)
            .ok_or(PackStreamError::Limit("delta depth"))?;
        let program = delta::parse(program, self.limits.max_object_bytes)?;
        let data = delta::apply(base, program, || (self.cancelled)())?;
        Ok((kind, data, depth))
    }

    fn complete<S>(
        &mut self,
        index: usize,
        kind: Kind,
        data: Vec<u8>,
        depth: u32,
        sink: &mut S,
    ) -> Result<()>
    where
        S: FnMut(ResolvedObject<'_>) -> std::result::Result<(), BoxError>,
    {
        let oid = object_id(kind, &data);
        if self.by_oid.insert(oid, index as u32).is_some() {
            return Err(PackStreamError::Duplicate(oid));
        }
        self.oids[index] = Some(oid);
        self.kinds[index] = Some(kind);
        self.depths[index] = depth;
        sink(ResolvedObject {
            index,
            oid,
            kind,
            data: &data,
        })
        .map_err(PackStreamError::Sink)?;
        if let Some(next) = self.next_use(index as u32) {
            self.cache.insert(index as u32, Bytes::from(data), next);
        }
        Ok(())
    }

    // Return a resolved entry's bytes from memory, re-reading it after eviction.
    fn base_bytes(&mut self, index: u32, source: &mut dyn EntrySource) -> Result<Bytes> {
        if let Some(bytes) = self.cache.get(index) {
            return Ok(bytes);
        }
        let entry = self.table.entries()[index as usize];
        self.reread_bases += 1;
        let frame = self.reread(&entry, source)?;
        let data_start = (entry.data_offset - entry.offset) as usize;
        let inflated = inflate_entry(&frame[data_start..], entry.size)?;
        let data = match self.parent(&entry)? {
            None => inflated,
            Some(parent) => {
                let base = self.base_bytes(parent, source)?;
                self.apply(parent, &base, &inflated)?.1
            }
        };
        let expected = self.oids[index as usize]
            .ok_or(PackStreamError::Invalid("pack delta base is unresolved"))?;
        let kind = self.kinds[index as usize]
            .ok_or(PackStreamError::Invalid("pack delta base is unresolved"))?;
        if object_id(kind, &data) != expected {
            return Err(PackStreamError::Invalid(
                "re-read pack entry changed identity",
            ));
        }
        let bytes = Bytes::from(data);
        if let Some(next) = self.next_use(index) {
            self.cache.insert(index, Arc::clone(&bytes), next);
        }
        Ok(bytes)
    }

    fn reread(&mut self, entry: &PackEntry, source: &mut dyn EntrySource) -> Result<Vec<u8>> {
        self.check()?;
        let frame = source
            .read(entry.offset, entry.end)
            .map_err(PackStreamError::Source)?;
        if frame.len() as u64 != entry.end - entry.offset || crc32fast::hash(&frame) != entry.crc32
        {
            return Err(PackStreamError::Invalid(
                "re-read pack entry does not match its frame",
            ));
        }
        Ok(frame)
    }

    fn parent(&self, entry: &PackEntry) -> Result<Option<u32>> {
        Ok(match entry.kind {
            EntryKind::Base(_) => None,
            EntryKind::OfsDelta { base } => Some(base_position(self.table, base)? as u32),
            EntryKind::RefDelta { base } => Some(
                self.by_oid
                    .get(&base)
                    .copied()
                    .ok_or(PackStreamError::MissingBase(base))?,
            ),
        })
    }

    // Like `parent`, but a reference base not yet resolved is `None`.
    fn known_parent(&self, entry: &PackEntry) -> Result<Option<u32>> {
        Ok(match entry.kind {
            EntryKind::RefDelta { base } => self.by_oid.get(&base).copied(),
            _ => self.parent(entry)?,
        })
    }

    fn next_use(&self, index: u32) -> Option<u64> {
        let index = index as usize;
        let start = self.child_starts[index] + self.cursors[index];
        (start < self.child_starts[index + 1]).then(|| self.child_offsets[start as usize])
    }

    fn advance_child(&mut self, parent: u32) {
        self.cursors[parent as usize] += 1;
        let next = self.next_use(parent);
        self.cache.retarget(parent, next);
    }

    fn check(&self) -> Result<()> {
        if (self.cancelled)() {
            return Err(PackStreamError::Cancelled);
        }
        Ok(())
    }

    fn into_indexed(self) -> Result<IndexedPack> {
        let checksum = self.table.checksum();
        let mut entries = Vec::with_capacity(self.oids.len());
        let mut kinds = Vec::with_capacity(self.oids.len());
        for (index, entry) in self.table.entries().iter().enumerate() {
            let (Some(oid), Some(kind)) = (self.oids[index], self.kinds[index]) else {
                return Err(PackStreamError::Invalid(
                    "pack contains an unresolved entry",
                ));
            };
            entries.push(PackIndexEntry {
                oid,
                crc32: entry.crc32,
                offset: entry.offset,
            });
            kinds.push((kind, None));
        }
        let label = Path::new("streamed pack index");
        let index = encode_pack_index_v2(&mut entries, label, checksum)?;
        let offsets = entries.iter().map(|entry| entry.offset).collect::<Vec<_>>();
        let reverse_index = encode_pack_reverse_index(&offsets, label, checksum)?;
        let kinds =
            crate::pack_locator::encode_pack_kind_metadata_with_external_deltas(checksum, &kinds)?;
        Ok(IndexedPack {
            index,
            reverse_index,
            kinds,
            checksum,
            object_count: u32::try_from(entries.len())
                .map_err(|_| PackStreamError::Limit("object count"))?,
            size: self.table.size(),
            max_depth: self.depths.iter().copied().max().unwrap_or_default(),
            reread_bases: self.reread_bases,
        })
    }
}

fn base_position(table: &PackTable, offset: u64) -> Result<usize> {
    table.position(offset).ok_or(PackStreamError::Invalid(
        "offset delta does not name an entry",
    ))
}
