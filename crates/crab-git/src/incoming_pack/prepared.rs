//! Self-contained pack artifacts derived only from verified quarantine spools.

use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use flate2::{Compression, read::ZlibEncoder};
use gix_hash::ObjectId;
use sha1::{Digest, Sha1};

use super::IncomingPack;
use crate::{
    PackLocationIter, PackLocatorError, encode_pack_kind_metadata, write_pack_reverse_index,
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
    #[error("normalized pack indexing failed")]
    Index(#[from] gix_pack::bundle::write::Error),
    #[error("normalized pack locator validation failed")]
    Locator(#[from] PackLocatorError),
    #[error("normalized pack disagrees with quarantine: {0}")]
    Mismatch(&'static str),
}

/// Self-contained pack, standard indexes and Crab kind sidecar in private storage.
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
}

impl PreparedPack {
    /// Returns the self-contained pack path.
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

    /// Returns the unique object count, including resolved thin-pack bases.
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
        check(cancelled)?;
        if self.objects.is_empty() {
            return Ok(None);
        }
        let object_count = u32::try_from(self.objects.len())
            .map_err(|_| PreparePackError::Mismatch("object count"))?;
        let directory = tempfile::Builder::new()
            .prefix("crab-prepared-")
            .tempdir_in(directory)?;
        let input = directory.path().join("normalized.pack");
        let (size, git_sha1) = self.write_normalized(&input, max_pack_bytes, cancelled)?;

        // Gitoxide's importer may allocate from pack headers. Only our bounded,
        // reconstructed full entries reach it; raw incoming packs never do.
        let outcome = gix_pack::Bundle::write_to_directory(
            &mut BufReader::new(File::open(&input)?),
            Some(directory.path()),
            &mut gix_features::progress::Discard,
            cancelled,
            None::<gix_object::find::Never>,
            gix_pack::bundle::write::Options {
                thread_limit: Some(1),
                iteration_mode: gix_pack::data::input::Mode::Verify,
                index_version: gix_pack::index::Version::V2,
                object_hash: gix_hash::Kind::Sha1,
            },
        )?;
        check(cancelled)?;
        if outcome.index.num_objects != object_count || outcome.index.data_hash != git_sha1 {
            return Err(PreparePackError::Mismatch("pack identity or count"));
        }
        let pack = outcome
            .data_path
            .ok_or(PreparePackError::Mismatch("missing pack"))?;
        let index = outcome
            .index_path
            .ok_or(PreparePackError::Mismatch("missing index"))?;
        std::fs::remove_file(input)?;
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
        }))
    }

    fn write_normalized(
        &self,
        path: &Path,
        max_pack_bytes: u64,
        cancelled: &AtomicBool,
    ) -> Result<(u64, ObjectId)> {
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
        for object in self.objects.values() {
            check(cancelled)?;
            header.clear();
            let kind = match object.kind {
                gix_object::Kind::Commit => gix_pack::data::entry::Header::Commit,
                gix_object::Kind::Tree => gix_pack::data::entry::Header::Tree,
                gix_object::Kind::Blob => gix_pack::data::entry::Header::Blob,
                gix_object::Kind::Tag => gix_pack::data::entry::Header::Tag,
            };
            kind.write_to(object.size as u64, &mut header)?;
            write_chunk(&mut out, &mut checksum, &mut size, max_body, &header)?;
            decoded.seek(SeekFrom::Start(object.offset))?;
            let mut encoder = ZlibEncoder::new(
                gix_features::interrupt::Read {
                    inner: (&mut decoded).take(object.size as u64),
                    should_interrupt: cancelled,
                },
                Compression::default(),
            );
            loop {
                check(cancelled)?;
                let count = encoder.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                write_chunk(
                    &mut out,
                    &mut checksum,
                    &mut size,
                    max_body,
                    &buffer[..count],
                )?;
            }
            if encoder.into_inner().inner.limit() != 0 {
                return Err(PreparePackError::Mismatch("truncated object spool"));
            }
        }
        let git_sha1 = ObjectId::from(<[u8; 20]>::from(checksum.finalize()));
        out.write_all(git_sha1.as_bytes())?;
        out.flush()?;
        Ok((size + 20, git_sha1))
    }
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
