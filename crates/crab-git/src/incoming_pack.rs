//! Private, bounded quarantine for incoming SHA-1 Git packs.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
};

use flate2::{Decompress, FlushDecompress, Status};
use gix_hash::ObjectId;
use gix_object::Kind;
use gix_pack::data::entry::Header;
use sha1::{Digest, Sha1};

use crate::delta;

mod prepared;
pub use prepared::{PreparePackError, PreparedPack};

type Result<T> = std::result::Result<T, IncomingPackError>;

/// Resource bounds enforced before accepting incoming objects.
#[derive(Clone, Copy, Debug)]
pub struct ReceiveLimits {
    pub max_pack_bytes: u64,
    pub max_objects: u32,
    pub max_object_bytes: usize,
    /// Includes inflated delta programs, reconstructed results and external bases.
    pub max_inflated_bytes: u64,
    pub max_delta_depth: u32,
}

/// A fully reconstructed Git object returned by an authorized base lookup.
pub struct BaseObject {
    pub kind: Kind,
    pub data: Vec<u8>,
}

/// Failures during pack quarantine; no canonical storage is modified.
#[derive(Debug, thiserror::Error)]
pub enum IncomingPackError {
    #[error("incoming pack I/O failed")]
    Io(#[from] io::Error),
    #[error("invalid incoming pack: {0}")]
    Invalid(&'static str),
    #[error("incoming pack exceeds {0}")]
    Limit(&'static str),
    #[error("incoming pack operation cancelled")]
    Cancelled,
    #[error("incoming pack compression is invalid")]
    Compression(#[from] flate2::DecompressError),
    #[error("incoming pack delta failed")]
    Delta(#[from] delta::DeltaError),
    #[error("base object {0} is missing")]
    MissingBase(ObjectId),
    #[error("base object lookup failed for {oid}")]
    BaseLookup {
        oid: ObjectId,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("cannot allocate incoming object buffer")]
    Allocation(#[from] std::collections::TryReserveError),
}

/// Verified object identity and its private spool location.
#[derive(Clone, Debug)]
pub struct IncomingObject {
    pub oid: ObjectId,
    pub kind: Kind,
    pub size: usize,
    offset: u64,
}

/// Verified pack objects retained in a private directory until this value is dropped.
///
/// Pack integrity and delta identities are verified. Use [`crate::receive_plan`]
/// for object syntax, graph connectivity and exact ref checks before publication;
/// pointer payloads still need storage proof. No canonical repository data is written.
pub struct IncomingPack {
    directory: tempfile::TempDir,
    objects: BTreeMap<ObjectId, IncomingObject>,
    received_objects: u32,
}

impl IncomingPack {
    /// Returns all unique objects, including any verified external thin-pack bases.
    pub fn objects(&self) -> impl Iterator<Item = &IncomingObject> {
        self.objects.values()
    }

    /// Returns the object count declared by the incoming pack, excluding added bases.
    #[must_use]
    pub fn received_objects(&self) -> u32 {
        self.received_objects
    }

    pub(crate) fn object(&self, oid: &ObjectId) -> Option<&IncomingObject> {
        self.objects.get(oid)
    }

    /// Reads an object from this quarantine; unknown identities return `None`.
    pub fn read_object(&self, oid: &ObjectId) -> Result<Option<BaseObject>> {
        self.objects
            .get(oid)
            .map(|object| {
                let mut file = File::open(self.directory.path().join("objects"))?;
                Ok(BaseObject {
                    kind: object.kind,
                    data: read_region(&mut file, object.offset, object.size)?,
                })
            })
            .transpose()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Base {
    Offset(u64),
    Oid(ObjectId),
}
struct Entry {
    pack_offset: u64,
    spool_offset: u64,
    size: usize,
    kind: Option<Kind>,
    base: Option<Base>,
}
#[derive(Clone)]
struct Resolved {
    object: IncomingObject,
    depth: u32,
}
struct Quarantine<C> {
    entries: Vec<Entry>,
    inflated: File,
    decoded: File,
    objects: BTreeMap<ObjectId, IncomingObject>,
    known: HashMap<Base, Resolved>,
    waiting: HashMap<Base, Vec<usize>>,
    ready: VecDeque<usize>,
    limits: ReceiveLimits,
    inflated_bytes: u64,
    cancelled: C,
}

/// Copies and verifies one complete pack, then resolves its delta graph in quarantine.
///
/// `directory` must already exist on the caller's temporary volume. `lookup` is
/// called only for unresolved ref-delta bases after in-pack objects are exhausted;
/// it must enforce repository visibility and its own I/O and allocation bounds.
/// The returned bytes are independently checked against the requested Git OID.
/// Call on a blocking worker; `cancelled` is checked between bounded chunks and
/// delta instructions. The reader and lookup must provide their own I/O deadlines.
pub fn quarantine<R, C, F>(
    reader: R,
    directory: &Path,
    limits: ReceiveLimits,
    cancelled: C,
    mut lookup: F,
) -> Result<IncomingPack>
where
    R: Read,
    C: Fn() -> bool,
    F: FnMut(
        &ObjectId,
    )
        -> std::result::Result<Option<BaseObject>, Box<dyn std::error::Error + Send + Sync>>,
{
    let directory = tempfile::Builder::new()
        .prefix("crab-receive-")
        .tempdir_in(directory)?;
    let mut pack = File::create_new(directory.path().join("incoming.pack"))?;
    let size = copy_pack(reader, &mut pack, limits.max_pack_bytes, &cancelled)?;
    drop(pack);
    pack = File::open(directory.path().join("incoming.pack"))?;
    verify_checksum(&mut pack, size, &cancelled)?;
    pack.rewind()?;
    let mut header = [0; 12];
    pack.read_exact(&mut header)?;
    if &header[..4] != b"PACK" || !matches!(&header[4..8], [0, 0, 0, 2] | [0, 0, 0, 3]) {
        return Err(IncomingPackError::Invalid("unsupported header"));
    }
    let count = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    if count > limits.max_objects {
        return Err(IncomingPackError::Limit("object count"));
    }
    let mut state = Quarantine {
        entries: Vec::new(),
        inflated: File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("inflated"))?,
        decoded: File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("objects"))?,
        objects: BTreeMap::new(),
        known: HashMap::new(),
        waiting: HashMap::new(),
        ready: VecDeque::new(),
        limits,
        inflated_bytes: 0,
        cancelled,
    };
    // Exclude the trailer from decompression. A valid checksum alone does not
    // prove the declared entry count or the boundary of the last zlib stream.
    let mut input = BufReader::new(pack.take(size - 20 - 12));
    let mut offset = 12;
    let mut offsets = std::collections::HashSet::new();
    for _ in 0..count {
        state.check()?;
        let entry = read_header(&mut input, offset)?;
        let kind = entry.header.as_kind();
        let base = match entry.header {
            Header::OfsDelta { base_distance } => {
                let base_offset = Header::verified_base_pack_offset(offset, base_distance)
                    .filter(|base| offsets.contains(base))
                    .ok_or(IncomingPackError::Invalid(
                        "offset delta does not point to an entry",
                    ))?;
                Some(Base::Offset(base_offset))
            }
            Header::RefDelta { base_id } => Some(Base::Oid(base_id)),
            _ => None,
        };
        let entry_size = usize::try_from(entry.decompressed_size)
            .map_err(|_| IncomingPackError::Limit("object size"))?;
        if entry_size > limits.max_object_bytes {
            return Err(IncomingPackError::Limit("object size"));
        }
        state.charge(entry.decompressed_size)?;
        let spool_offset = state.inflated.stream_position()?;
        let compressed_bytes = copy_inflated(
            &mut input,
            &mut state.inflated,
            entry_size,
            &state.cancelled,
        )?;
        let index = state.entries.len();
        if let Some(base) = base {
            state.waiting.entry(base).or_default().push(index);
        } else {
            state.ready.push_back(index);
        }
        state.entries.push(Entry {
            pack_offset: offset,
            spool_offset,
            size: entry_size,
            kind,
            base,
        });
        offsets.insert(offset);
        offset = entry
            .data_offset
            .checked_add(compressed_bytes)
            .ok_or(IncomingPackError::Limit("pack offsets"))?;
    }
    let mut extra = [0];
    if input.read(&mut extra)? != 0 || offset != size - 20 {
        return Err(IncomingPackError::Invalid(
            "entry count or trailing data mismatch",
        ));
    }
    loop {
        while let Some(index) = state.ready.pop_front() {
            state.resolve(index)?;
        }
        if state.waiting.is_empty() {
            break;
        }
        // A ref delta can refer forward within the pack. Only now is it safe
        // to request external bases without mistaking a later entry for one.
        let unresolved = state
            .waiting
            .keys()
            .filter_map(|base| match base {
                Base::Oid(oid) => Some(*oid),
                Base::Offset(_) => None,
            })
            .collect::<Vec<_>>();
        let mut found = false;
        for oid in &unresolved {
            state.check()?;
            if !state.waiting.contains_key(&Base::Oid(*oid)) {
                continue;
            }
            let object = lookup(oid)
                .map_err(|source| IncomingPackError::BaseLookup { oid: *oid, source })?;
            let Some(object) = object else { continue };
            if object.data.len() > limits.max_object_bytes {
                return Err(IncomingPackError::Limit("external base size"));
            }
            state.charge(object.data.len() as u64)?;
            if object_id(object.kind, &object.data) != *oid {
                return Err(IncomingPackError::Invalid(
                    "external base identity mismatch",
                ));
            }
            let resolved = state.store(object.kind, &object.data, 0)?;
            state.wake(Base::Oid(*oid), resolved);
            found = true;
        }
        if !found {
            return match unresolved.first() {
                Some(oid) => Err(IncomingPackError::MissingBase(*oid)),
                None => Err(IncomingPackError::Invalid("unresolvable delta graph")),
            };
        }
    }
    Ok(IncomingPack {
        directory,
        objects: state.objects,
        received_objects: count,
    })
}

impl<C: Fn() -> bool> Quarantine<C> {
    fn check(&self) -> Result<()> {
        check(&self.cancelled)
    }
    fn charge(&mut self, bytes: u64) -> Result<()> {
        self.inflated_bytes = self
            .inflated_bytes
            .checked_add(bytes)
            .filter(|total| *total <= self.limits.max_inflated_bytes)
            .ok_or(IncomingPackError::Limit("total inflated bytes"))?;
        Ok(())
    }
    fn store(&mut self, kind: Kind, data: &[u8], depth: u32) -> Result<Resolved> {
        let oid = object_id(kind, data);
        if let Some(object) = self.objects.get(&oid) {
            return Ok(Resolved {
                object: object.clone(),
                depth,
            });
        }
        if self.objects.len() >= self.limits.max_objects as usize {
            return Err(IncomingPackError::Limit(
                "object count including thin bases",
            ));
        }
        let offset = self.decoded.seek(SeekFrom::End(0))?;
        self.decoded.write_all(data)?;
        let object = IncomingObject {
            oid,
            kind,
            size: data.len(),
            offset,
        };
        self.objects.insert(oid, object.clone());
        Ok(Resolved { object, depth })
    }
    fn wake(&mut self, base: Base, resolved: Resolved) {
        self.known.insert(base, resolved);
        if let Some(children) = self.waiting.remove(&base) {
            self.ready.extend(children);
        }
    }
    fn resolve(&mut self, index: usize) -> Result<()> {
        self.check()?;
        let entry = &self.entries[index];
        let offset = entry.pack_offset;
        let data = read_region(&mut self.inflated, entry.spool_offset, entry.size)?;
        let resolved = if let Some(base) = entry.base {
            let base = self
                .known
                .get(&base)
                .cloned()
                .ok_or(IncomingPackError::Invalid("missing resolved base"))?;
            let depth = base
                .depth
                .checked_add(1)
                .filter(|depth| *depth <= self.limits.max_delta_depth)
                .ok_or(IncomingPackError::Limit("delta depth"))?;
            let program = delta::parse(&data, self.limits.max_object_bytes)?;
            self.charge(program.result_size as u64)?;
            let bytes = read_region(&mut self.decoded, base.object.offset, base.object.size)?;
            let reconstructed = delta::apply(&bytes, program, &self.cancelled)?;
            self.store(base.object.kind, &reconstructed, depth)?
        } else {
            let kind = entry
                .kind
                .ok_or(IncomingPackError::Invalid("missing object kind"))?;
            self.store(kind, &data, 0)?
        };
        self.wake(Base::Offset(offset), resolved.clone());
        self.wake(Base::Oid(resolved.object.oid), resolved);
        Ok(())
    }
}

fn read_header(input: &mut impl Read, offset: u64) -> Result<gix_pack::data::Entry> {
    struct Capture<'a, R> {
        input: &'a mut R,
        bytes: Vec<u8>,
    }
    impl<R: Read> Read for Capture<'_, R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            // SHA-1 entry headers fit in 30 bytes. Bound upstream varint parsing
            // and compare its encoding so an overflowing OFS distance cannot wrap.
            let remaining = 30usize.saturating_sub(self.bytes.len());
            if remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pack header too long",
                ));
            }
            let length = buffer.len().min(remaining);
            let count = self.input.read(&mut buffer[..length])?;
            self.bytes.extend_from_slice(&buffer[..count]);
            Ok(count)
        }
    }
    let mut captured = Capture {
        input,
        bytes: Vec::with_capacity(30),
    };
    let entry = gix_pack::data::Entry::from_read(&mut captured, offset, 20)?;
    let mut canonical = Vec::with_capacity(30);
    entry
        .header
        .write_to(entry.decompressed_size, &mut canonical)?;
    if canonical != captured.bytes {
        return Err(IncomingPackError::Invalid("non-canonical pack header"));
    }
    Ok(entry)
}

fn check(cancelled: &impl Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(IncomingPackError::Cancelled)
    } else {
        Ok(())
    }
}
fn read_region(file: &mut File, offset: u64, size: usize) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    data.try_reserve_exact(size)?;
    data.resize(size, 0);
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut data)?;
    Ok(data)
}
pub(crate) fn object_id(kind: Kind, data: &[u8]) -> ObjectId {
    let mut hash = Sha1::new();
    hash.update(gix_object::encode::loose_header(kind, data.len() as u64));
    hash.update(data);
    ObjectId::Sha1(hash.finalize().into())
}
fn copy_pack(
    mut input: impl Read,
    output: &mut File,
    maximum: u64,
    cancelled: &impl Fn() -> bool,
) -> Result<u64> {
    let mut size = 0u64;
    let mut buffer = [0; 64 * 1024];
    loop {
        check(cancelled)?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .filter(|size| *size <= maximum)
            .ok_or(IncomingPackError::Limit("pack bytes"))?;
        output.write_all(&buffer[..count])?;
    }
    if size < 32 {
        return Err(IncomingPackError::Invalid("truncated header or checksum"));
    }
    Ok(size)
}
fn verify_checksum(file: &mut File, size: u64, cancelled: &impl Fn() -> bool) -> Result<()> {
    let mut remaining = size - 20;
    let mut hash = Sha1::new();
    let mut buffer = [0; 64 * 1024];
    while remaining > 0 {
        check(cancelled)?;
        let count = usize::try_from(remaining.min(buffer.len() as u64))
            .map_err(|_| IncomingPackError::Limit("pack bytes"))?;
        file.read_exact(&mut buffer[..count])?;
        hash.update(&buffer[..count]);
        remaining -= count as u64;
    }
    let mut trailer = [0; 20];
    file.read_exact(&mut trailer)?;
    if hash.finalize()[..] != trailer {
        return Err(IncomingPackError::Invalid("pack checksum mismatch"));
    }
    Ok(())
}
fn copy_inflated(
    input: &mut impl BufRead,
    output: &mut File,
    expected: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<u64> {
    let mut decoder = Decompress::new(true);
    let mut remaining = expected;
    let mut buffer = [0; 64 * 1024];
    loop {
        check(cancelled)?;
        let compressed = input.fill_buf()?;
        if compressed.is_empty() {
            return Err(IncomingPackError::Invalid("truncated zlib stream"));
        }
        let before_in = decoder.total_in();
        let before_out = decoder.total_out();
        let status = decoder.decompress(compressed, &mut buffer, FlushDecompress::None)?;
        let consumed = (decoder.total_in() - before_in) as usize;
        let produced = (decoder.total_out() - before_out) as usize;
        input.consume(consumed);
        remaining = remaining
            .checked_sub(produced)
            .ok_or(IncomingPackError::Invalid(
                "inflated entry exceeds declared size",
            ))?;
        output.write_all(&buffer[..produced])?;
        if status == Status::StreamEnd {
            if remaining != 0 {
                return Err(IncomingPackError::Invalid("truncated inflated entry"));
            }
            return Ok(decoder.total_in());
        }
        if consumed == 0 && produced == 0 {
            return Err(IncomingPackError::Invalid("zlib stream made no progress"));
        }
    }
}

#[cfg(test)]
mod tests;
