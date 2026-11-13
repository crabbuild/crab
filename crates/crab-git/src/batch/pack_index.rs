use super::super::*;
use std::io::SeekFrom;

pub(super) struct Location {
    pub file: File,
    pub path: PathBuf,
    pub offset: u64,
    pub end: u64,
}

pub(super) fn find(
    directory: &Path,
    oid: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Location>> {
    let entries = match std::fs::read_dir(directory.join("pack")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    for entry in entries {
        check_cancelled(cancelled)?;
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "idx") {
            continue;
        }
        if let Some(location) = at(&path, oid, cancelled)? {
            return Ok(Some(location));
        }
    }
    Ok(None)
}

pub(super) fn at(
    path: &Path,
    oid: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<Location>> {
    check_cancelled(cancelled)?;
    let mut index = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !index.metadata()?.is_file() {
        return Err(invalid("Git pack index is not a regular file"));
    }
    let Some((offset, count, checksum)) = lookup(&mut index, oid, cancelled)? else {
        return Ok(None);
    };
    let path = path.with_extension("pack");
    let mut file = File::open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() < 12 + oid.len() as u64 {
        return Err(invalid("Git pack is not a complete regular file"));
    }
    let mut header = [0; 12];
    file.read_exact(&mut header)?;
    if &header[..4] != b"PACK"
        || !matches!(&header[4..8], [0, 0, 0, 2] | [0, 0, 0, 3])
        || u32::from_be_bytes(header[8..].try_into().map_err(io::Error::other)?) != count
    {
        return Err(invalid("Git pack header differs from its index"));
    }
    let end = metadata.len() - oid.len() as u64;
    let mut trailer = vec![0; oid.len()];
    read_at(&mut file, end, &mut trailer)?;
    if trailer != checksum || offset < 12 || offset >= end {
        return Err(invalid("Git pack does not match its index"));
    }
    Ok(Some(Location {
        file,
        path,
        offset,
        end,
    }))
}

fn lookup(
    index: &mut File,
    oid: &[u8],
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<(u64, u32, Vec<u8>)>> {
    let length = index.metadata()?.len();
    let mut signature = [0; 8];
    index.read_exact(&mut signature)?;
    let version_two = &signature[..4] == b"\xfftOc";
    if version_two && signature[4..] != 2_u32.to_be_bytes() {
        return Err(invalid("unsupported Git pack index version"));
    }
    let start = if version_two { 8_u64 } else { 0 };
    let mut bytes = [0; 1024];
    read_at(index, start, &mut bytes)?;
    let mut fanout = [0_u32; 256];
    let mut previous = 0;
    for (slot, bytes) in fanout.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *slot = u32::from_be_bytes(*bytes);
        if *slot < previous {
            return Err(invalid("Git pack index fanout is not ordered"));
        }
        previous = *slot;
    }
    let count = u64::from(fanout[255]);
    let hash_bytes = oid.len() as u64;
    let records = start + 1024;
    let minimum = records + count * (hash_bytes + if version_two { 8 } else { 4 }) + 2 * hash_bytes;
    if length < minimum
        || (!version_two && length != minimum)
        || (version_two
            && (!(length - minimum).is_multiple_of(8) || (length - minimum) / 8 > count))
    {
        return Err(invalid(
            "Git pack index length differs from its object count",
        ));
    }
    let bucket = usize::from(oid[0]);
    let mut low = if bucket == 0 { 0 } else { fanout[bucket - 1] };
    let mut high = fanout[bucket];
    let mut candidate = vec![0; oid.len()];
    while low < high {
        check_cancelled(cancelled)?;
        let position = low + (high - low) / 2;
        let name_at = if version_two {
            records + u64::from(position) * hash_bytes
        } else {
            records + u64::from(position) * (hash_bytes + 4) + 4
        };
        read_at(index, name_at, &mut candidate)?;
        match candidate.as_slice().cmp(oid) {
            std::cmp::Ordering::Less => low = position + 1,
            std::cmp::Ordering::Greater => high = position,
            std::cmp::Ordering::Equal => {
                let offset_at = if version_two {
                    records + count * (hash_bytes + 4) + u64::from(position) * 4
                } else {
                    records + u64::from(position) * (hash_bytes + 4)
                };
                let mut encoded = [0; 4];
                read_at(index, offset_at, &mut encoded)?;
                let encoded = u32::from_be_bytes(encoded);
                let offset = if version_two && encoded & 0x8000_0000 != 0 {
                    let slot = u64::from(encoded & 0x7fff_ffff);
                    if slot >= (length - minimum) / 8 {
                        return Err(invalid("Git pack index large offset is out of bounds"));
                    }
                    let mut large = [0; 8];
                    read_at(index, minimum - 2 * hash_bytes + slot * 8, &mut large)?;
                    u64::from_be_bytes(large)
                } else {
                    u64::from(encoded)
                };
                let mut checksum = vec![0; oid.len()];
                read_at(index, length - 2 * hash_bytes, &mut checksum)?;
                return Ok(Some((offset, fanout[255], checksum)));
            }
        }
    }
    Ok(None)
}

fn read_at(file: &mut File, position: u64, bytes: &mut [u8]) -> io::Result<()> {
    file.seek(SeekFrom::Start(position))?;
    file.read_exact(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack_locator::{PackIndexEntry, write_pack_index_v2};

    #[test]
    fn index_windows_preserve_large_offsets_from_the_shared_writer() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("large.idx");
        let checksum = gix_hash::ObjectId::Sha1([0xcc; 20]);
        let mut entries = [
            PackIndexEntry {
                oid: gix_hash::ObjectId::Sha1([0x11; 20]),
                crc32: 0,
                offset: 12,
            },
            PackIndexEntry {
                oid: gix_hash::ObjectId::Sha1([0xee; 20]),
                crc32: 0,
                offset: 0x8000_1234,
            },
        ];
        write_pack_index_v2(&mut entries, &path, checksum).map_err(io::Error::other)?;
        let reference =
            gix_pack::index::File::at(&path, gix_hash::Kind::Sha1).map_err(io::Error::other)?;
        for entry in entries {
            let mut file = File::open(&path)?;
            let actual = lookup(&mut file, entry.oid.as_bytes(), &|| false)?
                .ok_or_else(|| invalid("expected an indexed object"))?;
            let position = reference
                .lookup(entry.oid)
                .ok_or_else(|| invalid("reference index missed object"))?;
            assert_eq!(
                actual,
                (
                    reference.pack_offset_at_index(position),
                    2,
                    checksum.as_bytes().to_vec()
                )
            );
        }
        Ok(())
    }
}
