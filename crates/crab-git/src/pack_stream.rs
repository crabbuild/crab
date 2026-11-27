//! Two-pass, filesystem-free indexing of a Git pack streamed from a remote.
//!
//! [`PackScanner`] consumes a pack as it arrives and verifies its framing,
//! entry boundaries, per-entry CRC-32s and trailer checksum without retaining
//! object data. [`PackResolver`] consumes the same bytes again in order and
//! reconstructs every object. OFS deltas always name an earlier entry, so the
//! scan table tells the resolver exactly when each base is used next; bases are
//! cached until then, the farthest next use is evicted under memory pressure,
//! and evicted bases are re-read through [`EntrySource`]. Object identities are
//! computed from reconstructed bytes, never trusted from the source.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use flate2::{Decompress, FlushDecompress, Status};
use gix_hash::ObjectId;
use gix_object::Kind;
use sha1::{Digest, Sha1};

use crate::delta;

mod resolve;
#[cfg(test)]
mod tests;
mod window;

pub use resolve::{EntrySource, IndexedPack, PackResolver, ResolvedObject};
pub use window::WindowedSource;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, PackStreamError>;

const PACK_HEADER_BYTES: usize = 12;
const TRAILER_BYTES: usize = 20;
// SHA-1 entry headers fit in 30 bytes: a ten-byte size varint plus a 20-byte base.
const MAX_ENTRY_HEADER_BYTES: usize = 30;
const INFLATE_CHUNK: usize = 64 * 1024;

/// Resource bounds for one streamed pack.
#[derive(Clone, Copy, Debug)]
pub struct StreamLimits {
    pub max_pack_bytes: u64,
    pub max_objects: u32,
    /// Bounds each entry's inflated data and each reconstructed object.
    pub max_object_bytes: usize,
    /// Bounds the sum of all inflated entry data.
    pub max_inflated_bytes: u64,
    pub max_delta_depth: u32,
    /// Bounds reconstructed delta bases retained between their uses.
    pub max_cache_bytes: usize,
}

/// Failures while scanning or resolving a streamed pack.
#[derive(Debug, thiserror::Error)]
pub enum PackStreamError {
    #[error("invalid pack: {0}")]
    Invalid(&'static str),
    #[error("pack exceeds its {0} limit")]
    Limit(&'static str),
    #[error("pack contains duplicate object {0}")]
    Duplicate(ObjectId),
    #[error("pack delta base {0} is not in the pack")]
    MissingBase(ObjectId),
    #[error("pack entry inflation failed")]
    Inflate(#[from] flate2::DecompressError),
    #[error("pack delta is invalid")]
    Delta(#[from] delta::DeltaError),
    #[error("pack entry re-read failed")]
    Source(#[source] BoxError),
    #[error("pack object sink rejected an object")]
    Sink(#[source] BoxError),
    #[error("pack index construction failed")]
    Index(#[from] crate::pack_locator::PackLocatorError),
    #[error("pack indexing cancelled")]
    Cancelled,
}

/// How one entry's data is encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A complete object of this kind.
    Base(Kind),
    /// A delta against the entry starting at this earlier pack offset.
    OfsDelta { base: u64 },
    /// A delta against the object with this identity.
    RefDelta { base: ObjectId },
}

/// The verified frame of one pack entry.
#[derive(Clone, Copy, Debug)]
pub struct PackEntry {
    /// Offset of the entry header.
    pub offset: u64,
    /// Offset of the zlib data following the header.
    pub data_offset: u64,
    /// Offset one past the entry's last compressed byte.
    pub end: u64,
    pub kind: EntryKind,
    /// Declared inflated size of the entry data (a delta program for deltas).
    pub size: u64,
    /// CRC-32 over the header and compressed data, as stored in pack indexes.
    pub crc32: u32,
}

/// Frames of a complete, checksum-verified pack in offset order.
#[derive(Debug)]
pub struct PackTable {
    entries: Vec<PackEntry>,
    checksum: ObjectId,
    size: u64,
}

impl PackTable {
    /// Entries in increasing pack-offset order.
    #[must_use]
    pub fn entries(&self) -> &[PackEntry] {
        &self.entries
    }

    /// The pack trailer's SHA-1 checksum.
    #[must_use]
    pub fn checksum(&self) -> ObjectId {
        self.checksum
    }

    /// The complete pack length, including its trailer.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    fn position(&self, offset: u64) -> Option<usize> {
        self.entries
            .binary_search_by_key(&offset, |entry| entry.offset)
            .ok()
    }
}

enum ScanState {
    Header,
    EntryHeader,
    Data,
    Trailer,
    Done,
}

struct Inflating {
    entry: PackEntry,
    inflate: Decompress,
    crc: crc32fast::Hasher,
}

/// First pass: verify framing while the pack streams through, retaining no object data.
pub struct PackScanner {
    limits: StreamLimits,
    state: ScanState,
    // Unconsumed input is `pending[start..]`; compaction happens once per feed.
    pending: Vec<u8>,
    start: usize,
    // Absolute pack offset of `pending[start]`.
    position: u64,
    hasher: Sha1,
    declared: u32,
    entries: Vec<PackEntry>,
    inflated: u64,
    current: Option<Inflating>,
    scratch: Vec<u8>,
}

impl PackScanner {
    #[must_use]
    pub fn new(limits: StreamLimits) -> Self {
        Self {
            limits,
            state: ScanState::Header,
            pending: Vec::new(),
            start: 0,
            position: 0,
            hasher: Sha1::new(),
            declared: 0,
            entries: Vec::new(),
            inflated: 0,
            current: None,
            scratch: vec![0; INFLATE_CHUNK],
        }
    }

    /// Total bytes fed so far.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.position + self.available().len() as u64
    }

    /// Entries completed so far.
    #[must_use]
    pub fn entries(&self) -> usize {
        self.entries.len()
    }

    /// Consume the next bytes of the pack.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        if self.bytes().saturating_add(bytes.len() as u64) > self.limits.max_pack_bytes {
            return Err(PackStreamError::Limit("pack bytes"));
        }
        self.pending.drain(..self.start);
        self.start = 0;
        self.pending.extend_from_slice(bytes);
        while self.step()? {}
        Ok(())
    }

    /// Finish the scan; the pack must have ended exactly at its trailer.
    pub fn finish(self) -> Result<PackTable> {
        if !matches!(self.state, ScanState::Done) || !self.available().is_empty() {
            return Err(PackStreamError::Invalid("pack ended before its trailer"));
        }
        let checksum = ObjectId::Sha1(self.hasher.finalize().into());
        Ok(PackTable {
            entries: self.entries,
            checksum,
            size: self.position,
        })
    }

    fn available(&self) -> &[u8] {
        &self.pending[self.start..]
    }

    // Advance by one unit of work; false means more input is needed.
    fn step(&mut self) -> Result<bool> {
        match self.state {
            ScanState::Header => {
                let Some(header) = self.available().get(..PACK_HEADER_BYTES) else {
                    return Ok(false);
                };
                if &header[..4] != b"PACK" || !matches!(&header[4..8], [0, 0, 0, 2] | [0, 0, 0, 3])
                {
                    return Err(PackStreamError::Invalid("unsupported pack header"));
                }
                self.declared = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
                if self.declared > self.limits.max_objects {
                    return Err(PackStreamError::Limit("object count"));
                }
                self.entries.reserve(self.declared as usize);
                self.consume(PACK_HEADER_BYTES);
                self.state = if self.declared == 0 {
                    ScanState::Trailer
                } else {
                    ScanState::EntryHeader
                };
                Ok(true)
            }
            ScanState::EntryHeader => {
                let Some((kind, size, length)) =
                    parse_entry_header(self.available(), self.position)?
                else {
                    return Ok(false);
                };
                if let EntryKind::OfsDelta { base } = kind
                    && self
                        .entries
                        .binary_search_by_key(&base, |entry| entry.offset)
                        .is_err()
                {
                    return Err(PackStreamError::Invalid(
                        "offset delta does not name an earlier entry",
                    ));
                }
                let declared = usize::try_from(size)
                    .ok()
                    .filter(|size| *size <= self.limits.max_object_bytes)
                    .ok_or(PackStreamError::Limit("object bytes"))?;
                self.inflated = self
                    .inflated
                    .checked_add(declared as u64)
                    .filter(|total| *total <= self.limits.max_inflated_bytes)
                    .ok_or(PackStreamError::Limit("inflated bytes"))?;
                let offset = self.position;
                let mut crc = crc32fast::Hasher::new();
                crc.update(&self.available()[..length]);
                self.consume(length);
                self.current = Some(Inflating {
                    entry: PackEntry {
                        offset,
                        data_offset: offset + length as u64,
                        end: 0,
                        kind,
                        size,
                        crc32: 0,
                    },
                    inflate: Decompress::new(true),
                    crc,
                });
                self.state = ScanState::Data;
                Ok(true)
            }
            ScanState::Data => self.inflate(),
            ScanState::Trailer => {
                let available = self.available();
                if available.len() < TRAILER_BYTES {
                    return Ok(false);
                }
                if available.len() > TRAILER_BYTES {
                    return Err(PackStreamError::Invalid("pack has data after its trailer"));
                }
                let expected = self.hasher.clone().finalize();
                if available != &expected[..] {
                    return Err(PackStreamError::Invalid("pack checksum does not match"));
                }
                self.position += TRAILER_BYTES as u64;
                self.start = self.pending.len();
                self.state = ScanState::Done;
                Ok(false)
            }
            ScanState::Done => {
                if self.available().is_empty() {
                    Ok(false)
                } else {
                    Err(PackStreamError::Invalid("pack has data after its trailer"))
                }
            }
        }
    }

    fn inflate(&mut self) -> Result<bool> {
        if self.available().is_empty() {
            return Ok(false);
        }
        let current = self
            .current
            .as_mut()
            .ok_or(PackStreamError::Invalid("pack entry state is missing"))?;
        let input = &self.pending[self.start..];
        let before_in = current.inflate.total_in();
        let before_out = current.inflate.total_out();
        let status = current
            .inflate
            .decompress(input, &mut self.scratch, FlushDecompress::None)?;
        let consumed = usize::try_from(current.inflate.total_in() - before_in)
            .map_err(|_| PackStreamError::Invalid("pack entry offset overflows"))?;
        let produced = current.inflate.total_out() - before_out;
        if current.inflate.total_out() > current.entry.size {
            return Err(PackStreamError::Invalid(
                "pack entry inflates past its size",
            ));
        }
        current.crc.update(&input[..consumed]);
        let finished = matches!(status, Status::StreamEnd);
        if consumed == 0 && produced == 0 && !finished {
            return Ok(false);
        }
        if finished {
            if current.inflate.total_out() != current.entry.size {
                return Err(PackStreamError::Invalid("pack entry size does not match"));
            }
            let Inflating { mut entry, crc, .. } = self
                .current
                .take()
                .ok_or(PackStreamError::Invalid("pack entry state is missing"))?;
            entry.end = self.position + consumed as u64;
            entry.crc32 = crc.finalize();
            self.consume(consumed);
            self.entries.push(entry);
            self.state = if self.entries.len() == self.declared as usize {
                ScanState::Trailer
            } else {
                ScanState::EntryHeader
            };
        } else {
            self.consume(consumed);
        }
        Ok(true)
    }

    fn consume(&mut self, length: usize) {
        self.hasher
            .update(&self.pending[self.start..self.start + length]);
        self.start += length;
        self.position += length as u64;
    }
}

/// Parse one canonical entry header, or `None` when more bytes are needed.
fn parse_entry_header(bytes: &[u8], offset: u64) -> Result<Option<(EntryKind, u64, usize)>> {
    let mut cursor = 0usize;
    let next = |cursor: &mut usize| -> Option<u8> {
        let byte = bytes.get(*cursor).copied();
        *cursor += 1;
        byte
    };
    let Some(first) = next(&mut cursor) else {
        return Ok(None);
    };
    let type_id = (first >> 4) & 0b111;
    let mut size = u64::from(first & 0x0f);
    let mut shift = 4u32;
    let mut byte = first;
    while byte & 0x80 != 0 {
        let Some(value) = next(&mut cursor) else {
            return Ok(None);
        };
        if shift > 57 {
            return Err(PackStreamError::Invalid("pack entry size overflows"));
        }
        byte = value;
        size |= u64::from(byte & 0x7f) << shift;
        shift += 7;
    }
    // A zero final group is a non-canonical, padded size encoding.
    if cursor > 1 && byte == 0 {
        return Err(PackStreamError::Invalid("non-canonical pack entry header"));
    }
    let kind = match type_id {
        1 => EntryKind::Base(Kind::Commit),
        2 => EntryKind::Base(Kind::Tree),
        3 => EntryKind::Base(Kind::Blob),
        4 => EntryKind::Base(Kind::Tag),
        6 => {
            let Some(mut value) = next(&mut cursor) else {
                return Ok(None);
            };
            let mut distance = u64::from(value & 0x7f);
            while value & 0x80 != 0 {
                let Some(following) = next(&mut cursor) else {
                    return Ok(None);
                };
                value = following;
                distance = distance
                    .checked_add(1)
                    .and_then(|distance| distance.checked_mul(128))
                    .map(|distance| distance | u64::from(value & 0x7f))
                    .ok_or(PackStreamError::Invalid("offset delta distance overflows"))?;
            }
            let base = offset
                .checked_sub(distance)
                .filter(|base| distance > 0 && *base >= PACK_HEADER_BYTES as u64)
                .ok_or(PackStreamError::Invalid(
                    "offset delta distance is out of range",
                ))?;
            EntryKind::OfsDelta { base }
        }
        7 => {
            let end = cursor + 20;
            let Some(oid) = bytes.get(cursor..end) else {
                return Ok(None);
            };
            cursor = end;
            let base: [u8; 20] = oid
                .try_into()
                .map_err(|_| PackStreamError::Invalid("reference delta base is truncated"))?;
            EntryKind::RefDelta {
                base: ObjectId::Sha1(base),
            }
        }
        _ => return Err(PackStreamError::Invalid("unsupported pack entry type")),
    };
    if cursor > MAX_ENTRY_HEADER_BYTES {
        return Err(PackStreamError::Invalid("pack entry header is too long"));
    }
    Ok(Some((kind, size, cursor)))
}

/// Inflate one complete entry's zlib data, which must end exactly at its frame.
fn inflate_entry(compressed: &[u8], size: u64) -> Result<Vec<u8>> {
    let size = usize::try_from(size).map_err(|_| PackStreamError::Limit("object bytes"))?;
    let mut output = Vec::new();
    // Empty objects still need room for zlib to report its stream end.
    output
        .try_reserve_exact(size.max(1))
        .map_err(|_| PackStreamError::Limit("object bytes"))?;
    let mut inflate = Decompress::new(true);
    loop {
        let (before_in, before_out) = (inflate.total_in(), inflate.total_out());
        let start = usize::try_from(before_in)
            .map_err(|_| PackStreamError::Invalid("pack entry offset overflows"))?;
        let status = inflate.decompress_vec(
            compressed.get(start..).unwrap_or_default(),
            &mut output,
            FlushDecompress::Finish,
        )?;
        if matches!(status, Status::StreamEnd) {
            break;
        }
        // A fixed-capacity output that stops progressing is either truncated
        // input or data longer than the declared size.
        if inflate.total_in() == before_in && inflate.total_out() == before_out {
            return Err(PackStreamError::Invalid(
                "pack entry does not match its frame",
            ));
        }
    }
    if output.len() != size || inflate.total_in() != compressed.len() as u64 {
        return Err(PackStreamError::Invalid(
            "pack entry does not match its frame",
        ));
    }
    Ok(output)
}

/// Return the Git object identity of reconstructed bytes.
fn object_id(kind: Kind, data: &[u8]) -> ObjectId {
    crate::incoming_pack::object_id(kind, data)
}

/// Shared ownership for cached reconstructed bytes.
type Bytes = Arc<[u8]>;

/// Exact-next-use cache of reconstructed delta bases.
struct BaseCache {
    budget: usize,
    used: usize,
    entries: HashMap<u32, (Bytes, u64)>,
    // (next use offset, entry index): the last element is evicted first.
    order: BTreeSet<(u64, u32)>,
}

impl BaseCache {
    fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            entries: HashMap::new(),
            order: BTreeSet::new(),
        }
    }

    fn get(&self, index: u32) -> Option<Bytes> {
        self.entries.get(&index).map(|(bytes, _)| Arc::clone(bytes))
    }

    /// Retain `bytes` until `next_use`, evicting bases needed later first.
    fn insert(&mut self, index: u32, bytes: Bytes, next_use: u64) {
        self.remove(index);
        let length = bytes.len();
        if length > self.budget {
            return;
        }
        while self.used + length > self.budget {
            let Some(&(farthest, victim)) = self.order.last() else {
                return;
            };
            if farthest <= next_use {
                // The new base is the one needed farthest in the future.
                return;
            }
            self.remove(victim);
        }
        self.used += length;
        self.order.insert((next_use, index));
        self.entries.insert(index, (bytes, next_use));
    }

    /// Move a cached base to its next use, or drop it after its last use.
    fn retarget(&mut self, index: u32, next_use: Option<u64>) {
        let Some(next) = next_use else {
            self.remove(index);
            return;
        };
        if let Some((_, current)) = self.entries.get_mut(&index) {
            self.order.remove(&(*current, index));
            *current = next;
            self.order.insert((next, index));
        }
    }

    fn remove(&mut self, index: u32) {
        if let Some((bytes, next_use)) = self.entries.remove(&index) {
            self.used -= bytes.len();
            self.order.remove(&(next_use, index));
        }
    }
}
