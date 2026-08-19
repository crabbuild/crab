//! Bounded verified object ranges derived from standard Git pack indexes.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use sha1::{Digest, Sha1};

const PACK_HEADER_LEN: u64 = 12;
const SHA1_LEN: usize = 20;
const MIN_PACK_LEN: u64 = PACK_HEADER_LEN + SHA1_LEN as u64;
const REVERSE_HEADER_LEN: usize = 12;
const REVERSE_ENTRY_LEN: usize = 4;
const REVERSE_TRAILER_LEN: usize = SHA1_LEN * 2;

/// A Git object and its complete packed-entry range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackObjectLocation {
    /// Object ID stored at this pack location.
    pub oid: gix_hash::ObjectId,
    /// Offset of the packed entry header.
    pub pack_offset: u64,
    /// Complete packed entry length, including header, delta base, and zlib bytes.
    pub entry_len: u64,
    /// CRC32 over the complete packed entry.
    pub crc32: u32,
}

/// Errors returned while deriving or validating pack object locations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PackLocatorError {
    /// The pack is too short to contain its header and trailer.
    #[error("git pack length {pack_len} is smaller than the {minimum} byte minimum")]
    InvalidPackLength { pack_len: u64, minimum: u64 },

    /// The pack index could not be opened.
    #[error("failed to open git pack index {path}")]
    IndexOpen {
        path: PathBuf,
        #[source]
        source: gix_pack::index::init::Error,
    },

    /// The pack index checksum did not match its contents.
    #[error("git pack index checksum failed for {path}")]
    IndexChecksum {
        path: PathBuf,
        #[source]
        source: gix_pack::index::verify::checksum::Error,
    },

    /// Locator construction requires a version 2 pack index with CRC32 values.
    #[error("git pack index {path} has unsupported version {version:?}")]
    UnsupportedIndexVersion {
        path: PathBuf,
        version: gix_pack::index::Version,
    },

    /// A version 2 index entry did not contain its required CRC32.
    #[error("git pack index {path} has no CRC32 for object {oid}")]
    MissingCrc {
        path: PathBuf,
        oid: gix_hash::ObjectId,
    },

    /// An object offset does not fall inside the pack data region.
    #[error(
        "git pack index {path} contains offset {offset} outside data range {PACK_HEADER_LEN}..{pack_data_end}"
    )]
    InvalidOffset {
        path: PathBuf,
        offset: u64,
        pack_data_end: u64,
    },

    /// Two index entries point to the same packed entry.
    #[error("git pack index {path} contains duplicate pack offset {offset}")]
    DuplicateOffset { path: PathBuf, offset: u64 },

    /// Checked range arithmetic overflowed.
    #[error("git pack range arithmetic overflowed for {path}")]
    Overflow { path: PathBuf },

    /// Reverse-index filesystem I/O failed.
    #[error("git reverse-index I/O failed for {path}")]
    ReverseIndexIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Reverse-index bytes were structurally invalid or disagreed with the index.
    #[error("invalid git reverse index {path}: {reason}")]
    InvalidReverseIndex { path: PathBuf, reason: String },

    /// The reverse-index checksum did not match its contents.
    #[error("git reverse-index checksum mismatch for {path}")]
    ReverseIndexChecksum { path: PathBuf },
}

/// Iterator over verified locations in increasing pack-offset order.
pub struct PackLocationIter {
    index: gix_pack::index::File,
    reverse: Vec<u8>,
    pack_data_end: u64,
    cursor: u32,
    object_count: u32,
    idx_path: PathBuf,
    rev_path: PathBuf,
}

impl PackLocationIter {
    /// Open and fully validate an index/reverse-index pair for one pack length.
    pub fn open(idx_path: &Path, rev_path: &Path, pack_len: u64) -> Result<Self, PackLocatorError> {
        if pack_len < MIN_PACK_LEN {
            return Err(PackLocatorError::InvalidPackLength {
                pack_len,
                minimum: MIN_PACK_LEN,
            });
        }
        let index = open_index(idx_path)?;
        let reverse =
            std::fs::read(rev_path).map_err(|source| PackLocatorError::ReverseIndexIo {
                path: rev_path.to_owned(),
                source,
            })?;
        let object_count = index.num_objects();
        let pack_data_end =
            pack_len
                .checked_sub(SHA1_LEN as u64)
                .ok_or_else(|| PackLocatorError::Overflow {
                    path: idx_path.to_owned(),
                })?;
        validate_reverse(
            &index,
            idx_path,
            rev_path,
            &reverse,
            object_count,
            pack_data_end,
        )?;
        Ok(Self {
            index,
            reverse,
            pack_data_end,
            cursor: 0,
            object_count,
            idx_path: idx_path.to_owned(),
            rev_path: rev_path.to_owned(),
        })
    }

    /// Git SHA-1 stored in the index for the corresponding pack.
    #[must_use]
    pub fn pack_checksum(&self) -> gix_hash::ObjectId {
        self.index.pack_checksum()
    }

    /// Number of verified objects represented by the pair.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        u64::from(self.object_count)
    }

    fn location(&self, reverse_position: u32) -> Result<PackObjectLocation, PackLocatorError> {
        let index_position = reverse_position_at(&self.reverse, reverse_position, &self.rev_path)?;
        let pack_offset = self.index.pack_offset_at_index(index_position);
        let entry_end = if reverse_position + 1 == self.object_count {
            self.pack_data_end
        } else {
            let next = reverse_position_at(&self.reverse, reverse_position + 1, &self.rev_path)?;
            self.index.pack_offset_at_index(next)
        };
        let entry_len =
            entry_end
                .checked_sub(pack_offset)
                .ok_or_else(|| PackLocatorError::Overflow {
                    path: self.idx_path.clone(),
                })?;
        let oid = self.index.oid_at_index(index_position).to_owned();
        let crc32 = self.index.crc32_at_index(index_position).ok_or_else(|| {
            PackLocatorError::MissingCrc {
                path: self.idx_path.clone(),
                oid: oid.to_owned(),
            }
        })?;
        Ok(PackObjectLocation {
            oid,
            pack_offset,
            entry_len,
            crc32,
        })
    }
}

impl Iterator for PackLocationIter {
    type Item = Result<PackObjectLocation, PackLocatorError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor == self.object_count {
            return None;
        }
        let cursor = self.cursor;
        self.cursor += 1;
        Some(self.location(cursor))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.object_count - self.cursor) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for PackLocationIter {}

/// Write a standard Git reverse index using only one `u32` per object.
pub fn write_pack_reverse_index(idx_path: &Path, rev_path: &Path) -> Result<(), PackLocatorError> {
    let index = open_index(idx_path)?;
    let mut positions: Vec<u32> = (0..index.num_objects()).collect();
    positions.sort_unstable_by_key(|position| index.pack_offset_at_index(*position));
    let mut previous = None;
    for position in &positions {
        let offset = index.pack_offset_at_index(*position);
        if offset < PACK_HEADER_LEN {
            return Err(PackLocatorError::InvalidOffset {
                path: idx_path.to_owned(),
                offset,
                pack_data_end: u64::MAX,
            });
        }
        if previous == Some(offset) {
            return Err(PackLocatorError::DuplicateOffset {
                path: idx_path.to_owned(),
                offset,
            });
        }
        previous = Some(offset);
    }

    let parent = rev_path.parent().unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|source| {
        PackLocatorError::ReverseIndexIo {
            path: rev_path.to_owned(),
            source,
        }
    })?;
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        let mut checksum = Sha1::new();
        write_hashed(&mut writer, &mut checksum, b"RIDX", rev_path)?;
        write_hashed(&mut writer, &mut checksum, &1_u32.to_be_bytes(), rev_path)?;
        write_hashed(&mut writer, &mut checksum, &1_u32.to_be_bytes(), rev_path)?;
        for position in positions {
            write_hashed(
                &mut writer,
                &mut checksum,
                &position.to_be_bytes(),
                rev_path,
            )?;
        }
        write_hashed(
            &mut writer,
            &mut checksum,
            index.pack_checksum().as_bytes(),
            rev_path,
        )?;
        writer.write_all(&checksum.finalize()).map_err(|source| {
            PackLocatorError::ReverseIndexIo {
                path: rev_path.to_owned(),
                source,
            }
        })?;
        writer
            .flush()
            .map_err(|source| PackLocatorError::ReverseIndexIo {
                path: rev_path.to_owned(),
                source,
            })?;
    }
    temp.persist(rev_path)
        .map_err(|error| PackLocatorError::ReverseIndexIo {
            path: rev_path.to_owned(),
            source: error.error,
        })?;
    Ok(())
}

fn open_index(idx_path: &Path) -> Result<gix_pack::index::File, PackLocatorError> {
    let index = gix_pack::index::File::at(idx_path, gix_hash::Kind::Sha1).map_err(|source| {
        PackLocatorError::IndexOpen {
            path: idx_path.to_owned(),
            source,
        }
    })?;
    let mut progress = gix_features::progress::Discard;
    index
        .verify_checksum(&mut progress, &AtomicBool::new(false))
        .map_err(|source| PackLocatorError::IndexChecksum {
            path: idx_path.to_owned(),
            source,
        })?;
    if index.version() != gix_pack::index::Version::V2 {
        return Err(PackLocatorError::UnsupportedIndexVersion {
            path: idx_path.to_owned(),
            version: index.version(),
        });
    }
    Ok(index)
}

fn validate_reverse(
    index: &gix_pack::index::File,
    idx_path: &Path,
    rev_path: &Path,
    bytes: &[u8],
    object_count: u32,
    pack_data_end: u64,
) -> Result<(), PackLocatorError> {
    let table_len = usize::try_from(object_count)
        .ok()
        .and_then(|count| count.checked_mul(REVERSE_ENTRY_LEN))
        .ok_or_else(|| PackLocatorError::Overflow {
            path: rev_path.to_owned(),
        })?;
    let expected_len = REVERSE_HEADER_LEN
        .checked_add(table_len)
        .and_then(|len| len.checked_add(REVERSE_TRAILER_LEN))
        .ok_or_else(|| PackLocatorError::Overflow {
            path: rev_path.to_owned(),
        })?;
    if bytes.len() != expected_len {
        return Err(invalid_reverse(
            rev_path,
            format!(
                "length {} does not match expected {expected_len}",
                bytes.len()
            ),
        ));
    }
    if bytes.get(..4) != Some(b"RIDX") {
        return Err(invalid_reverse(rev_path, "missing RIDX signature"));
    }
    if read_u32(bytes, 4, rev_path)? != 1 {
        return Err(invalid_reverse(rev_path, "unsupported version"));
    }
    if read_u32(bytes, 8, rev_path)? != 1 {
        return Err(invalid_reverse(rev_path, "unsupported hash identifier"));
    }

    let checksum_start = expected_len - SHA1_LEN;
    let computed = Sha1::digest(&bytes[..checksum_start]);
    if computed.as_slice() != &bytes[checksum_start..] {
        return Err(PackLocatorError::ReverseIndexChecksum {
            path: rev_path.to_owned(),
        });
    }
    let pack_checksum_start = checksum_start - SHA1_LEN;
    if index.pack_checksum().as_bytes() != &bytes[pack_checksum_start..checksum_start] {
        return Err(invalid_reverse(
            rev_path,
            "pack checksum does not match index",
        ));
    }

    let bitset_len = usize::try_from(object_count)
        .ok()
        .and_then(|count| count.checked_add(7))
        .map(|count| count / 8)
        .ok_or_else(|| PackLocatorError::Overflow {
            path: rev_path.to_owned(),
        })?;
    let mut seen = vec![0_u8; bitset_len];
    let mut previous_offset = None;
    for reverse_position in 0..object_count {
        let index_position = reverse_position_at(bytes, reverse_position, rev_path)?;
        if index_position >= object_count {
            return Err(invalid_reverse(
                rev_path,
                "object position is outside the index",
            ));
        }
        let byte = (index_position / 8) as usize;
        let mask = 1_u8 << (index_position % 8);
        if seen[byte] & mask != 0 {
            return Err(invalid_reverse(
                rev_path,
                "object positions are not a complete permutation",
            ));
        }
        seen[byte] |= mask;

        let offset = index.pack_offset_at_index(index_position);
        if offset < PACK_HEADER_LEN || offset >= pack_data_end {
            return Err(PackLocatorError::InvalidOffset {
                path: idx_path.to_owned(),
                offset,
                pack_data_end,
            });
        }
        if previous_offset.is_some_and(|previous| previous >= offset) {
            return Err(if previous_offset == Some(offset) {
                PackLocatorError::DuplicateOffset {
                    path: idx_path.to_owned(),
                    offset,
                }
            } else {
                invalid_reverse(rev_path, "object positions are not in pack-offset order")
            });
        }
        let oid = index.oid_at_index(index_position).to_owned();
        if index.crc32_at_index(index_position).is_none() {
            return Err(PackLocatorError::MissingCrc {
                path: idx_path.to_owned(),
                oid,
            });
        }
        previous_offset = Some(offset);
    }
    Ok(())
}

fn reverse_position_at(
    bytes: &[u8],
    reverse_position: u32,
    rev_path: &Path,
) -> Result<u32, PackLocatorError> {
    let start = usize::try_from(reverse_position)
        .ok()
        .and_then(|position| position.checked_mul(REVERSE_ENTRY_LEN))
        .and_then(|offset| REVERSE_HEADER_LEN.checked_add(offset))
        .ok_or_else(|| PackLocatorError::Overflow {
            path: rev_path.to_owned(),
        })?;
    read_u32(bytes, start, rev_path)
}

fn read_u32(bytes: &[u8], start: usize, path: &Path) -> Result<u32, PackLocatorError> {
    let end = start
        .checked_add(REVERSE_ENTRY_LEN)
        .ok_or_else(|| PackLocatorError::Overflow {
            path: path.to_owned(),
        })?;
    let value = bytes
        .get(start..end)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| invalid_reverse(path, "truncated u32 field"))?;
    Ok(u32::from_be_bytes(value))
}

fn write_hashed(
    writer: &mut impl Write,
    checksum: &mut Sha1,
    bytes: &[u8],
    path: &Path,
) -> Result<(), PackLocatorError> {
    writer
        .write_all(bytes)
        .map_err(|source| PackLocatorError::ReverseIndexIo {
            path: path.to_owned(),
            source,
        })?;
    checksum.update(bytes);
    Ok(())
}

fn invalid_reverse(path: &Path, reason: impl Into<String>) -> PackLocatorError {
    PackLocatorError::InvalidReverseIndex {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use sha1::{Digest, Sha1};

    use super::{PackLocationIter, PackLocatorError, write_pack_reverse_index};

    struct PackFixture {
        _temp: tempfile::TempDir,
        pack: PathBuf,
        idx: PathBuf,
        rev: PathBuf,
    }

    impl PackFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().expect("temporary pack fixture");
            let git_dir = temp.path().join("repo.git");
            git(&["init", "--bare", path_str(&git_dir)], None);
            let first = hash_object(&git_dir, b"first object\n");
            let second = hash_object(&git_dir, b"second object with more bytes\n");
            let third = hash_object(
                &git_dir,
                b"third object with still more bytes than second\n",
            );
            let object_list = format!("{first}\n{second}\n{third}\n");
            let base = temp.path().join("fixture");
            let output = git(
                &[
                    "--git-dir",
                    path_str(&git_dir),
                    "pack-objects",
                    "--index-version=2",
                    path_str(&base),
                ],
                Some(object_list.as_bytes()),
            );
            let pack_hash = String::from_utf8(output)
                .expect("pack hash is UTF-8")
                .trim()
                .to_owned();
            let pack = temp.path().join(format!("fixture-{pack_hash}.pack"));
            let idx = temp.path().join("fixture.idx");
            git(&["index-pack", "-o", path_str(&idx), path_str(&pack)], None);
            let rev = idx.with_extension("rev");
            write_pack_reverse_index(&idx, &rev).expect("write reverse index");
            assert!(pack.exists());
            assert!(idx.exists());
            assert!(rev.exists());
            Self {
                _temp: temp,
                pack,
                idx,
                rev,
            }
        }

        fn pack_len(&self) -> u64 {
            fs::metadata(&self.pack).expect("pack metadata").len()
        }
    }

    fn path_str(path: &Path) -> &str {
        path.to_str().expect("test paths are UTF-8")
    }

    fn git(args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
        let mut child = Command::new("git")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn git");
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(input)
                .expect("write git stdin");
        }
        let output = child.wait_with_output().expect("wait for git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn hash_object(git_dir: &Path, bytes: &[u8]) -> String {
        String::from_utf8(git(
            &[
                "--git-dir",
                path_str(git_dir),
                "hash-object",
                "-w",
                "--stdin",
            ],
            Some(bytes),
        ))
        .expect("object hash is UTF-8")
        .trim()
        .to_owned()
    }

    #[test]
    fn streamed_locations_cover_the_pack_data_region() {
        let fixture = PackFixture::new();
        let locations = PackLocationIter::open(&fixture.idx, &fixture.rev, fixture.pack_len())
            .expect("open locations")
            .collect::<Result<Vec<_>, _>>()
            .expect("stream locations");

        assert_eq!(locations.len(), 3);
        assert!(
            locations
                .windows(2)
                .all(|pair| { pair[0].pack_offset + pair[0].entry_len == pair[1].pack_offset })
        );
        let last = locations.last().expect("last location");
        assert_eq!(last.pack_offset + last.entry_len, fixture.pack_len() - 20);
    }

    #[test]
    fn generated_reverse_index_is_standard_and_verified() {
        let fixture = PackFixture::new();
        let generated = fixture.rev.with_file_name("generated.rev");

        write_pack_reverse_index(&fixture.idx, &generated).expect("write reverse index");
        let generated_locations =
            PackLocationIter::open(&fixture.idx, &generated, fixture.pack_len())
                .expect("open generated reverse index")
                .collect::<Result<Vec<_>, _>>()
                .expect("stream generated locations");
        let git_locations = PackLocationIter::open(&fixture.idx, &fixture.rev, fixture.pack_len())
            .expect("open git reverse index")
            .collect::<Result<Vec<_>, _>>()
            .expect("stream git locations");
        assert_eq!(generated_locations, git_locations);
    }

    #[test]
    fn pack_length_before_header_and_trailer_is_rejected() {
        let fixture = PackFixture::new();
        let error = PackLocationIter::open(&fixture.idx, &fixture.rev, 31)
            .err()
            .expect("short pack must fail");
        assert!(matches!(error, PackLocatorError::InvalidPackLength { .. }));
    }

    #[test]
    fn reverse_duplicate_position_is_rejected_with_valid_checksum() {
        let fixture = PackFixture::new();
        let mut bytes = fs::read(&fixture.rev).expect("read rev");
        let duplicate = <[u8; 4]>::try_from(&bytes[12..16]).expect("reverse position");
        bytes[16..20].copy_from_slice(&duplicate);
        let checksum_start = bytes.len() - 20;
        let checksum = Sha1::digest(&bytes[..checksum_start]);
        bytes[checksum_start..].copy_from_slice(&checksum);
        let corrupt_rev = fixture.rev.with_file_name("duplicate.rev");
        fs::write(&corrupt_rev, bytes).expect("write corrupt rev");

        let error = PackLocationIter::open(&fixture.idx, &corrupt_rev, fixture.pack_len())
            .err()
            .expect("duplicate reverse position must fail");
        assert!(matches!(
            error,
            PackLocatorError::InvalidReverseIndex { .. }
        ));
    }

    #[test]
    fn reverse_checksum_mismatch_is_rejected() {
        let fixture = PackFixture::new();
        let mut bytes = fs::read(&fixture.rev).expect("read rev");
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        let corrupt_rev = fixture.rev.with_file_name("checksum.rev");
        fs::write(&corrupt_rev, bytes).expect("write corrupt rev");

        let error = PackLocationIter::open(&fixture.idx, &corrupt_rev, fixture.pack_len())
            .err()
            .expect("corrupt reverse checksum must fail");
        assert!(matches!(
            error,
            PackLocatorError::ReverseIndexChecksum { .. }
        ));
    }
}
