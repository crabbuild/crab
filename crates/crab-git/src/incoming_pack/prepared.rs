//! Pack artifacts derived from verified quarantine spools and explicit bases.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{self, BufWriter, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use flate2::{Compression, read::ZlibEncoder};
use gix_hash::ObjectId;
use sha1::{Digest, Sha1};

use super::{IncomingPack, object_id};
use crate::{
    PackLocationIter, PackLocatorError, encode_pack_kind_metadata,
    pack_locator::{PackIndexEntry, write_pack_index_v2},
    write_pack_reverse_index,
};

type Result<T> = std::result::Result<T, PreparePackError>;

/// Failures preparing private pack artifacts; canonical storage is never changed.
#[derive(Debug, thiserror::Error)]
pub enum PreparePackError {
    #[error("pack preparation I/O failed")]
    Io(#[from] io::Error),
    #[error("normalized pack exceeds its byte limit")]
    Limit,
    #[error("pack preparation cancelled")]
    Cancelled,
    #[error("normalized pack locator validation failed")]
    Locator(#[from] PackLocatorError),
    #[error("cannot allocate {requested} bytes while preparing a normalized pack")]
    Allocation {
        requested: usize,
        #[source]
        source: std::collections::TryReserveError,
    },
    #[error("normalized pack disagrees with quarantine: {0}")]
    Mismatch(&'static str),
}

/// Verified logical base for one generated cross-pack REF_DELTA entry.
#[derive(Clone, Debug)]
pub struct ExternalDeltaBase {
    oid: ObjectId,
    kind: gix_object::Kind,
    data: Vec<u8>,
    depth: u32,
}

impl ExternalDeltaBase {
    /// Bind decoded base bytes and their known delta depth to an object identity.
    #[must_use]
    pub fn new(oid: ObjectId, kind: gix_object::Kind, data: Vec<u8>, depth: u32) -> Self {
        Self {
            oid,
            kind,
            data,
            depth,
        }
    }
}

/// Pack, standard indexes and Crab kind sidecar in private storage.
///
/// Paths remain valid until this owner is dropped. Preparation proves identities
/// and index consistency, not graph connectivity, pointer payloads or publication.
#[derive(Debug)]
pub struct PreparedPack {
    _directory: tempfile::TempDir,
    pack: PathBuf,
    index: PathBuf,
    reverse: PathBuf,
    kinds: PathBuf,
    size: u64,
    object_count: u32,
    git_sha1: ObjectId,
    content_hash: blake3::Hash,
    delta_depths: BTreeMap<ObjectId, u32>,
    external_delta_count: u32,
}

struct WrittenPack {
    size: u64,
    git_sha1: ObjectId,
    index_entries: Vec<PackIndexEntry>,
    delta_depths: BTreeMap<ObjectId, u32>,
    external_delta_count: u32,
}

impl PreparedPack {
    /// Returns the prepared pack path.
    pub fn pack_path(&self) -> &Path {
        &self.pack
    }

    /// Returns the standard Git v2 pack index path.
    pub fn index_path(&self) -> &Path {
        &self.index
    }

    /// Returns the standard Git reverse index path.
    pub fn reverse_path(&self) -> &Path {
        &self.reverse
    }

    /// Returns the checksummed Crab object-kind sidecar path.
    pub fn kinds_path(&self) -> &Path {
        &self.kinds
    }

    /// Returns the complete pack size including its SHA-1 trailer.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the indexed target object count; external delta bases are excluded.
    #[must_use]
    pub fn object_count(&self) -> u32 {
        self.object_count
    }

    /// Returns the Git SHA-1 pack trailer.
    #[must_use]
    pub fn git_sha1(&self) -> ObjectId {
        self.git_sha1
    }

    /// Returns the Blake3 identity of the complete pack, including its trailer.
    #[must_use]
    pub fn content_hash(&self) -> blake3::Hash {
        self.content_hash
    }

    /// Returns the verified delta depth used for one packed object.
    #[must_use]
    pub fn delta_depth(&self, oid: &ObjectId) -> Option<u32> {
        self.delta_depths.get(oid).copied()
    }

    /// Returns the number of entries that reference bases outside this pack.
    #[must_use]
    pub fn external_delta_count(&self) -> u32 {
        self.external_delta_count
    }
}

impl IncomingPack {
    /// Prepares self-contained artifacts without Git, an object database or storage writes.
    ///
    /// Empty quarantines return `None`. Each unique object, including thin bases,
    /// becomes a full zlib entry in OID order; this trades delta compression for
    /// independent readability. `max_pack_bytes` bounds the output, not the input.
    /// Use a blocking worker and an existing temporary volume directory. Peak
    /// additional disk is two bounded packs plus O(object count) index sidecars.
    /// Indexing uses one worker and allocations bounded by quarantine's object
    /// count and maximum object size. Cancellation is checked during streaming
    /// and indexing, and between the bounded sidecar operations.
    pub fn prepare(
        &self,
        directory: &Path,
        max_pack_bytes: u64,
        cancelled: &AtomicBool,
    ) -> Result<Option<PreparedPack>> {
        self.prepare_inner(
            directory,
            max_pack_bytes,
            cancelled,
            &BTreeMap::new(),
            &BTreeMap::new(),
            0,
            0,
        )
    }

    /// Prepares a self-contained pack with bounded deltas between generated objects.
    ///
    /// Every `(object, base)` pair is only used when both objects are present,
    /// have the same kind, fit `max_delta_object_bytes`, and keep the resulting
    /// chain within `max_delta_depth`. Other objects remain full entries. The
    /// prepared pack is independently indexed and identity-checked exactly like
    /// [`Self::prepare`]. Callers must supply relationships derived while
    /// constructing trusted objects, rather than similarity guesses.
    pub fn prepare_with_delta_bases(
        &self,
        directory: &Path,
        max_pack_bytes: u64,
        cancelled: &AtomicBool,
        delta_bases: &BTreeMap<ObjectId, ObjectId>,
        max_delta_depth: u32,
        max_delta_object_bytes: usize,
    ) -> Result<Option<PreparedPack>> {
        self.prepare_inner(
            directory,
            max_pack_bytes,
            cancelled,
            delta_bases,
            &BTreeMap::new(),
            max_delta_depth,
            max_delta_object_bytes,
        )
    }

    /// Prepares a pack with bounded in-pack OFS_DELTA and cross-pack REF_DELTA entries.
    ///
    /// External bases are accepted only when their kind, decoded bytes, identity,
    /// and known depth match the requested relationship. The resulting pack may
    /// require those base object IDs from the surrounding Git object database.
    /// All target identities and standard sidecars remain independently checked.
    pub fn prepare_with_external_delta_bases(
        &self,
        directory: &Path,
        max_pack_bytes: u64,
        cancelled: &AtomicBool,
        delta_bases: &BTreeMap<ObjectId, ObjectId>,
        external_delta_bases: &BTreeMap<ObjectId, ExternalDeltaBase>,
        max_delta_depth: u32,
        max_delta_object_bytes: usize,
    ) -> Result<Option<PreparedPack>> {
        self.prepare_inner(
            directory,
            max_pack_bytes,
            cancelled,
            delta_bases,
            external_delta_bases,
            max_delta_depth,
            max_delta_object_bytes,
        )
    }

    fn prepare_inner(
        &self,
        directory: &Path,
        max_pack_bytes: u64,
        cancelled: &AtomicBool,
        delta_bases: &BTreeMap<ObjectId, ObjectId>,
        external_delta_bases: &BTreeMap<ObjectId, ExternalDeltaBase>,
        max_delta_depth: u32,
        max_delta_object_bytes: usize,
    ) -> Result<Option<PreparedPack>> {
        check(cancelled)?;
        if self.objects.is_empty() {
            return Ok(None);
        }
        let object_count = u32::try_from(self.objects.len())
            .map_err(|_| PreparePackError::Mismatch("object count"))?;
        let directory = tempfile::Builder::new()
            .prefix("crab-prepared-")
            .tempdir_in(directory)?;
        let staging_pack = directory.path().join("normalized.pack");
        let WrittenPack {
            size,
            git_sha1,
            mut index_entries,
            delta_depths,
            external_delta_count,
        } = self.write_normalized(
            &staging_pack,
            max_pack_bytes,
            cancelled,
            delta_bases,
            external_delta_bases,
            max_delta_depth,
            max_delta_object_bytes,
        )?;

        check(cancelled)?;
        let pack = directory.path().join(format!("pack-{git_sha1}.pack"));
        std::fs::rename(staging_pack, &pack)?;
        let index = pack.with_extension("idx");
        write_pack_index_v2(&mut index_entries, &index, git_sha1)?;
        let reverse = pack.with_extension("rev");
        write_pack_reverse_index(&index, &reverse)?;
        check(cancelled)?;
        let locations = PackLocationIter::open(&index, &reverse, size)?;
        if locations.pack_checksum() != git_sha1
            || !locations
                .sorted_object_ids()
                .eq(self.objects.keys().copied())
        {
            return Err(PreparePackError::Mismatch("indexed object identities"));
        }
        let mut kinds = Vec::with_capacity(self.objects.len());
        for location in locations {
            check(cancelled)?;
            let location = location?;
            kinds.push(
                self.objects
                    .get(&location.oid)
                    .ok_or(PreparePackError::Mismatch("unknown indexed object"))?
                    .kind,
            );
        }
        let kinds_path = pack.with_extension("kinds");
        std::fs::write(&kinds_path, encode_pack_kind_metadata(git_sha1, &kinds)?)?;
        let mut content_hash = blake3::Hasher::new();
        let mut file = File::open(&pack)?;
        let mut buffer = [0; 64 * 1024];
        loop {
            check(cancelled)?;
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            content_hash.update(&buffer[..count]);
        }
        Ok(Some(PreparedPack {
            _directory: directory,
            pack,
            index,
            reverse,
            kinds: kinds_path,
            size,
            object_count,
            git_sha1,
            content_hash: content_hash.finalize(),
            delta_depths,
            external_delta_count,
        }))
    }

    fn write_normalized(
        &self,
        path: &Path,
        max_pack_bytes: u64,
        cancelled: &AtomicBool,
        delta_bases: &BTreeMap<ObjectId, ObjectId>,
        external_delta_bases: &BTreeMap<ObjectId, ExternalDeltaBase>,
        max_delta_depth: u32,
        max_delta_object_bytes: usize,
    ) -> Result<WrittenPack> {
        let max_body = max_pack_bytes
            .checked_sub(20)
            .ok_or(PreparePackError::Limit)?;
        let mut out = BufWriter::new(File::create_new(path)?);
        let mut checksum = Sha1::new();
        let mut size = 0;
        let mut header = b"PACK\0\0\0\x02".to_vec();
        let count = u32::try_from(self.objects.len())
            .map_err(|_| PreparePackError::Mismatch("object count"))?;
        header.extend_from_slice(&count.to_be_bytes());
        write_chunk(&mut out, &mut checksum, &mut size, max_body, &header)?;
        let mut decoded = File::open(self.directory.path().join("objects"))?;
        let mut buffer = [0; 64 * 1024];
        let mut written = HashMap::<ObjectId, (u64, u32)>::with_capacity(self.objects.len());
        let mut index_entries = Vec::with_capacity(self.objects.len());
        let mut external_delta_count = 0_u32;
        for oid in ordered_objects(&self.objects, delta_bases) {
            check(cancelled)?;
            let object = self
                .objects
                .get(&oid)
                .ok_or(PreparePackError::Mismatch("ordered object disappeared"))?;
            let entry_offset = size;
            let delta = prepare_delta(
                &mut decoded,
                object,
                delta_bases,
                external_delta_bases,
                &self.objects,
                &written,
                max_delta_depth,
                max_delta_object_bytes,
            )?;
            header.clear();
            let depth = if let Some(delta) = &delta {
                match delta.base {
                    PreparedDeltaBase::Offset(base_offset) => {
                        gix_pack::data::entry::Header::OfsDelta {
                            base_distance: entry_offset
                                .checked_sub(base_offset)
                                .ok_or(PreparePackError::Mismatch("delta base order"))?,
                        }
                        .write_to(delta.bytes.len() as u64, &mut header)?;
                    }
                    PreparedDeltaBase::Oid(base_id) => {
                        gix_pack::data::entry::Header::RefDelta { base_id }
                            .write_to(delta.bytes.len() as u64, &mut header)?;
                        external_delta_count = external_delta_count.saturating_add(1);
                    }
                }
                delta.depth
            } else {
                full_header(object.kind).write_to(object.size as u64, &mut header)?;
                0
            };
            let mut entry_crc = crc32fast::Hasher::new();
            write_entry_chunk(
                &mut out,
                &mut checksum,
                &mut entry_crc,
                &mut size,
                max_body,
                &header,
            )?;
            match delta {
                Some(delta) => {
                    write_compressed(
                        Cursor::new(delta.bytes),
                        cancelled,
                        &mut buffer,
                        &mut out,
                        &mut checksum,
                        &mut entry_crc,
                        &mut size,
                        max_body,
                    )?;
                }
                None => {
                    decoded.seek(SeekFrom::Start(object.offset))?;
                    let source = ObjectHashReader::new(
                        (&mut decoded).take(object.size as u64),
                        object.kind,
                        object.size,
                    );
                    let source = write_compressed(
                        source,
                        cancelled,
                        &mut buffer,
                        &mut out,
                        &mut checksum,
                        &mut entry_crc,
                        &mut size,
                        max_body,
                    )?;
                    if source.inner.limit() != 0 {
                        return Err(PreparePackError::Mismatch("truncated object spool"));
                    }
                    if source.object_id() != object.oid {
                        return Err(PreparePackError::Mismatch("indexed object identities"));
                    }
                }
            }
            index_entries.push(PackIndexEntry {
                oid,
                crc32: entry_crc.finalize(),
                offset: entry_offset,
            });
            written.insert(oid, (entry_offset, depth));
        }
        let git_sha1 = ObjectId::from(<[u8; 20]>::from(checksum.finalize()));
        out.write_all(git_sha1.as_bytes())?;
        out.flush()?;
        Ok(WrittenPack {
            size: size + 20,
            git_sha1,
            index_entries,
            delta_depths: written
                .into_iter()
                .map(|(oid, (_, depth))| (oid, depth))
                .collect(),
            external_delta_count,
        })
    }
}

#[derive(Clone, Copy)]
enum PreparedDeltaBase {
    Offset(u64),
    Oid(ObjectId),
}

struct PreparedDelta {
    base: PreparedDeltaBase,
    depth: u32,
    bytes: Vec<u8>,
}

fn full_header(kind: gix_object::Kind) -> gix_pack::data::entry::Header {
    match kind {
        gix_object::Kind::Commit => gix_pack::data::entry::Header::Commit,
        gix_object::Kind::Tree => gix_pack::data::entry::Header::Tree,
        gix_object::Kind::Blob => gix_pack::data::entry::Header::Blob,
        gix_object::Kind::Tag => gix_pack::data::entry::Header::Tag,
    }
}

fn ordered_objects(
    objects: &BTreeMap<ObjectId, super::IncomingObject>,
    delta_bases: &BTreeMap<ObjectId, ObjectId>,
) -> Vec<ObjectId> {
    let mut ordered = Vec::with_capacity(objects.len());
    let mut complete = HashSet::with_capacity(objects.len());
    let mut visiting = HashSet::new();
    for oid in objects.keys().copied() {
        let mut stack = vec![(oid, false)];
        while let Some((current, finish)) = stack.pop() {
            if complete.contains(&current) {
                continue;
            }
            if finish {
                visiting.remove(&current);
                complete.insert(current);
                ordered.push(current);
                continue;
            }
            if !visiting.insert(current) {
                continue;
            }
            stack.push((current, true));
            if let Some(base) = delta_bases
                .get(&current)
                .filter(|base| objects.contains_key(*base))
                .copied()
                && !complete.contains(&base)
                && !visiting.contains(&base)
            {
                stack.push((base, false));
            }
        }
    }
    ordered
}

fn prepare_delta(
    decoded: &mut File,
    object: &super::IncomingObject,
    delta_bases: &BTreeMap<ObjectId, ObjectId>,
    external_delta_bases: &BTreeMap<ObjectId, ExternalDeltaBase>,
    objects: &BTreeMap<ObjectId, super::IncomingObject>,
    written: &HashMap<ObjectId, (u64, u32)>,
    max_delta_depth: u32,
    max_delta_object_bytes: usize,
) -> Result<Option<PreparedDelta>> {
    if max_delta_depth == 0 || object.size > max_delta_object_bytes {
        return Ok(None);
    }
    let Some(base_oid) = delta_bases.get(&object.oid) else {
        return Ok(None);
    };
    let (base_bytes, base, base_depth) = if let Some(base) = objects.get(base_oid) {
        let Some((base_offset, base_depth)) = written.get(base_oid).copied() else {
            return Ok(None);
        };
        if base.kind != object.kind
            || base.size > max_delta_object_bytes
            || base_depth >= max_delta_depth
        {
            return Ok(None);
        }
        (
            Cow::Owned(read_object_bytes(decoded, base)?),
            PreparedDeltaBase::Offset(base_offset),
            base_depth,
        )
    } else {
        let Some(base) = external_delta_bases.get(&object.oid) else {
            return Ok(None);
        };
        if base.oid != *base_oid
            || base.kind != object.kind
            || base.data.len() > max_delta_object_bytes
            || base.depth >= max_delta_depth
            || object_id(base.kind, &base.data) != base.oid
        {
            return Ok(None);
        }
        (
            Cow::Borrowed(base.data.as_slice()),
            PreparedDeltaBase::Oid(base.oid),
            base.depth,
        )
    };
    let object_bytes = read_object_bytes(decoded, object)?;
    let Some(bytes) = prefix_suffix_delta(&base_bytes, &object_bytes)? else {
        return Ok(None);
    };
    Ok(Some(PreparedDelta {
        base,
        depth: base_depth + 1,
        bytes,
    }))
}

fn read_object_bytes(decoded: &mut File, object: &super::IncomingObject) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(object.size)
        .map_err(|source| PreparePackError::Allocation {
            requested: object.size,
            source,
        })?;
    bytes.resize(object.size, 0);
    decoded.seek(SeekFrom::Start(object.offset))?;
    decoded.read_exact(&mut bytes)?;
    if object_id(object.kind, &bytes) != object.oid {
        return Err(PreparePackError::Mismatch("indexed object identities"));
    }
    Ok(bytes)
}

struct ObjectHashReader<R> {
    inner: R,
    hash: Sha1,
}

impl<R> ObjectHashReader<R> {
    fn new(inner: R, kind: gix_object::Kind, size: usize) -> Self {
        let mut hash = Sha1::new();
        hash.update(gix_object::encode::loose_header(kind, size as u64));
        Self { inner, hash }
    }

    fn object_id(self) -> ObjectId {
        ObjectId::Sha1(self.hash.finalize().into())
    }
}

impl<R: Read> Read for ObjectHashReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hash.update(&buffer[..count]);
        Ok(count)
    }
}

fn prefix_suffix_delta(base: &[u8], target: &[u8]) -> Result<Option<Vec<u8>>> {
    let prefix = base
        .iter()
        .zip(target)
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = base[prefix..]
        .iter()
        .rev()
        .zip(target[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    let mut delta = Vec::new();
    delta
        .try_reserve_exact(target.len().saturating_add(32))
        .map_err(|source| PreparePackError::Allocation {
            requested: target.len().saturating_add(32),
            source,
        })?;
    encode_delta_size(base.len(), &mut delta);
    encode_delta_size(target.len(), &mut delta);
    append_delta_copy(0, prefix, &mut delta)?;
    append_delta_insert(
        &target[prefix..target.len().saturating_sub(suffix)],
        &mut delta,
    );
    append_delta_copy(base.len().saturating_sub(suffix), suffix, &mut delta)?;
    if delta.len().saturating_add(16) >= target.len() {
        return Ok(None);
    }
    Ok(Some(delta))
}

fn encode_delta_size(mut value: usize, output: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return;
        }
    }
}

fn append_delta_insert(mut bytes: &[u8], output: &mut Vec<u8>) {
    while !bytes.is_empty() {
        let count = bytes.len().min(0x7f);
        output.push(count as u8);
        output.extend_from_slice(&bytes[..count]);
        bytes = &bytes[count..];
    }
}

fn append_delta_copy(mut offset: usize, mut size: usize, output: &mut Vec<u8>) -> Result<()> {
    while size > 0 {
        let count = size.min(0x00ff_ffff);
        let encoded_offset = u32::try_from(offset).map_err(|_| PreparePackError::Limit)?;
        let encoded_count = u32::try_from(count).map_err(|_| PreparePackError::Limit)?;
        let mut command = 0x80;
        let mut arguments = Vec::with_capacity(7);
        for (mask, shift) in [(0x01, 0), (0x02, 8), (0x04, 16), (0x08, 24)] {
            let byte = (encoded_offset >> shift) as u8;
            if byte != 0 {
                command |= mask;
                arguments.push(byte);
            }
        }
        for (mask, shift) in [(0x10, 0), (0x20, 8), (0x40, 16)] {
            let byte = (encoded_count >> shift) as u8;
            if byte != 0 {
                command |= mask;
                arguments.push(byte);
            }
        }
        output.push(command);
        output.extend_from_slice(&arguments);
        size -= count;
        offset = offset.checked_add(count).ok_or(PreparePackError::Limit)?;
    }
    Ok(())
}

fn write_compressed<R: Read>(
    source: R,
    cancelled: &AtomicBool,
    buffer: &mut [u8],
    out: &mut impl Write,
    checksum: &mut Sha1,
    entry_crc: &mut crc32fast::Hasher,
    size: &mut u64,
    limit: u64,
) -> Result<R> {
    let mut encoder = ZlibEncoder::new(
        gix_features::interrupt::Read {
            inner: source,
            should_interrupt: cancelled,
        },
        Compression::default(),
    );
    loop {
        check(cancelled)?;
        let count = encoder.read(buffer)?;
        if count == 0 {
            break;
        }
        write_entry_chunk(out, checksum, entry_crc, size, limit, &buffer[..count])?;
    }
    Ok(encoder.into_inner().inner)
}

fn write_entry_chunk(
    out: &mut impl Write,
    checksum: &mut Sha1,
    entry_crc: &mut crc32fast::Hasher,
    size: &mut u64,
    limit: u64,
    bytes: &[u8],
) -> Result<()> {
    write_chunk(out, checksum, size, limit, bytes)?;
    entry_crc.update(bytes);
    Ok(())
}

fn check(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(PreparePackError::Cancelled);
    }
    Ok(())
}

fn write_chunk(
    out: &mut impl Write,
    checksum: &mut Sha1,
    size: &mut u64,
    limit: u64,
    bytes: &[u8],
) -> Result<()> {
    let next = size
        .checked_add(bytes.len() as u64)
        .filter(|next| *next <= limit)
        .ok_or(PreparePackError::Limit)?;
    out.write_all(bytes)?;
    checksum.update(bytes);
    *size = next;
    Ok(())
}
