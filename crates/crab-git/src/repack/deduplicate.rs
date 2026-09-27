use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};

use gix_pack::data::entry::Header;
use sha1::{Digest, Sha1};

use super::{RepackError, RepackSource, io_error};
use crate::pack_locator::{PackLocationIter, PackObjectLocation};

pub(super) fn write_body(
    sources: &[RepackSource],
    output: &mut impl Write,
    sha1: &mut Sha1,
    content: &mut blake3::Hasher,
) -> Result<u64, RepackError> {
    let mut output = HashedWriter {
        output,
        sha1,
        content,
    };
    let mut emitted = HashSet::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    for source in sources {
        let locations =
            PackLocationIter::open(&source.index_path, &source.reverse_index_path, source.size)?;
        if locations.object_count() != source.object_count {
            return Err(invalid(
                source,
                "index object count differs from pack header",
            ));
        }
        let source_objects = locations.sorted_object_ids().collect::<Vec<_>>();
        if source_objects.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid(
                source,
                "source index object IDs are not strictly ordered",
            ));
        }
        // An intact source body moves by one constant offset. Preserve its
        // headers as well as its payloads so OFS links remain valid and compact.
        let preserve_offsets = source_objects.iter().all(|oid| !emitted.contains(oid));
        let checksum = locations.pack_checksum();
        let locations = locations.collect::<Result<Vec<_>, _>>()?;
        if locations
            .first()
            .is_some_and(|entry| entry.pack_offset != 12)
        {
            return Err(invalid(source, "source index omits the first packed entry"));
        }
        let file =
            File::open(&source.path).map_err(|error| io_error("open pack union source", error))?;
        let mut input = BufReader::with_capacity(1024 * 1024, file);
        input
            .seek(SeekFrom::Start(source.size - 20))
            .map_err(|error| io_error("seek pack union checksum", error))?;
        let mut trailer = [0_u8; 20];
        input
            .read_exact(&mut trailer)
            .map_err(|error| io_error("read pack union checksum", error))?;
        if checksum.as_bytes() != trailer {
            return Err(invalid(source, "source index and pack checksums differ"));
        }
        input
            .seek(SeekFrom::Start(12))
            .map_err(|error| io_error("seek pack union body", error))?;
        for location in &locations {
            let prefix_len = location.entry_len.min(64) as usize;
            input
                .read_exact(&mut buffer[..prefix_len])
                .map_err(|error| io_error("read pack union entry header", error))?;
            let entry =
                gix_pack::data::Entry::from_bytes(&buffer[..prefix_len], location.pack_offset, 20)
                    .map_err(|error| invalid(source, format!("invalid entry header: {error}")))?;
            let header_len = entry.data_offset - location.pack_offset;
            if header_len >= location.entry_len || header_len > prefix_len as u64 {
                return Err(invalid(source, "entry header exceeds its committed range"));
            }
            // First-source precedence is safe only for independently closed packs:
            // mixing representations from mutually dependent thin packs can form
            // a delta cycle even when their original union was decodable.
            let header =
                closed_header(source, &locations, &source_objects, location, entry.header)?;
            let emit = emitted.insert(location.oid);
            let mut crc = crc32fast::Hasher::new();
            crc.update(&buffer[..prefix_len]);
            if emit {
                if preserve_offsets {
                    output
                        .write_all(&buffer[..prefix_len])
                        .map_err(|error| io_error("write intact pack union entry", error))?;
                } else {
                    header
                        .write_to(entry.decompressed_size, &mut output)
                        .and_then(|_| output.write_all(&buffer[header_len as usize..prefix_len]))
                        .map_err(|error| io_error("write pack union entry header", error))?;
                }
            }
            let mut remaining = location.entry_len - prefix_len as u64;
            while remaining > 0 {
                let length = remaining.min(buffer.len() as u64) as usize;
                input
                    .read_exact(&mut buffer[..length])
                    .map_err(|error| io_error("read pack union entry body", error))?;
                crc.update(&buffer[..length]);
                if emit {
                    output
                        .write_all(&buffer[..length])
                        .map_err(|error| io_error("write pack union entry body", error))?;
                }
                remaining -= length as u64;
            }
            if crc.finalize() != location.crc32 {
                return Err(invalid(
                    source,
                    "entry CRC does not match its committed index",
                ));
            }
        }
    }
    Ok(emitted.len() as u64)
}

fn closed_header(
    source: &RepackSource,
    locations: &[PackObjectLocation],
    source_objects: &[gix_hash::ObjectId],
    location: &PackObjectLocation,
    header: Header,
) -> Result<Header, RepackError> {
    match header {
        Header::OfsDelta { base_distance } => {
            let offset = Header::verified_base_pack_offset(location.pack_offset, base_distance)
                .ok_or_else(|| invalid(source, "OFS_DELTA base distance is invalid"))?;
            let position = locations
                .binary_search_by_key(&offset, |entry| entry.pack_offset)
                .map_err(|_| invalid(source, "OFS_DELTA base is not an indexed entry"))?;
            Ok(Header::RefDelta {
                base_id: locations[position].oid,
            })
        }
        Header::RefDelta { base_id } if source_objects.binary_search(&base_id).is_err() => {
            Err(invalid(
                source,
                "overlapping structural union requires self-contained sources",
            ))
        }
        _ => Ok(header),
    }
}

struct HashedWriter<'a, W> {
    output: &'a mut W,
    sha1: &'a mut Sha1,
    content: &'a mut blake3::Hasher,
}

impl<W: Write> Write for HashedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.output.write(bytes)?;
        self.sha1.update(&bytes[..count]);
        self.content.update(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

fn invalid(source: &RepackSource, reason: impl Into<String>) -> RepackError {
    RepackError::SourceIntegrity {
        pack_id: source.canonical_id.clone(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use crate::incoming_pack::{IncomingPack, PreparedPack, ReceiveLimits};

    use super::*;

    fn source(pack: &PreparedPack) -> RepackSource {
        RepackSource {
            canonical_id: pack.content_hash().to_hex().to_string(),
            path: pack.pack_path().to_owned(),
            index_path: pack.index_path().to_owned(),
            reverse_index_path: pack.reverse_path().to_owned(),
            size: pack.size(),
            object_count: u64::from(pack.object_count()),
            verified_identity: None,
        }
    }

    #[test]
    fn overlapping_union_checks_crc_even_for_discarded_entries() {
        let root = tempfile::tempdir().unwrap();
        let limits = ReceiveLimits {
            max_pack_bytes: 4096,
            max_objects: 4,
            max_object_bytes: 1024,
            max_inflated_bytes: 4096,
            max_delta_depth: 8,
        };
        let repeated = b"repeated blob".to_vec();
        let repeated_oid = crate::incoming_pack::object_id(gix_object::Kind::Blob, &repeated);
        let packs = [vec![repeated.clone()], vec![repeated, b"new blob".to_vec()]].map(|objects| {
            IncomingPack::from_generated_objects(
                objects
                    .into_iter()
                    .map(|bytes| (gix_object::Kind::Blob, bytes)),
                root.path(),
                limits,
                || false,
            )
            .unwrap()
            .prepare(root.path(), 4096, &AtomicBool::new(false))
            .unwrap()
            .unwrap()
        });
        let sources = packs.each_ref().map(source);
        let locations = PackLocationIter::open(
            &sources[1].index_path,
            &sources[1].reverse_index_path,
            sources[1].size,
        )
        .unwrap();
        let position = locations
            .sorted_object_ids()
            .position(|oid| oid == repeated_oid)
            .unwrap();
        let mut index = std::fs::read(&sources[1].index_path).unwrap();
        let crc_start = 8 + 256 * 4 + locations.object_count() as usize * 20;
        index[crc_start + position * 4] ^= 1;
        let checksum_start = index.len() - 20;
        let checksum = Sha1::digest(&index[..checksum_start]);
        index[checksum_start..].copy_from_slice(&checksum);
        std::fs::write(&sources[1].index_path, index).unwrap();
        let error = crate::repack::concatenate_complete_pack_inventory(&sources).unwrap_err();
        assert!(matches!(
            error,
            RepackError::SourceIntegrity { pack_id, reason }
                if pack_id == sources[1].canonical_id
                    && reason == "entry CRC does not match its committed index"
        ));
    }

    #[test]
    fn overlapping_union_requires_indexed_local_delta_bases() {
        let source = RepackSource {
            canonical_id: "unused".to_owned(),
            path: "unused.pack".into(),
            index_path: "unused.idx".into(),
            reverse_index_path: "unused.rev".into(),
            size: 100,
            object_count: 1,
            verified_identity: None,
        };
        let location = PackObjectLocation {
            oid: gix_hash::ObjectId::from([1; 20]),
            pack_offset: 20,
            entry_len: 60,
            crc32: 0,
        };
        for header in [
            Header::OfsDelta { base_distance: 0 },
            Header::OfsDelta { base_distance: 21 },
            Header::OfsDelta { base_distance: 8 },
            Header::RefDelta {
                base_id: gix_hash::ObjectId::from([2; 20]),
            },
        ] {
            assert!(
                closed_header(
                    &source,
                    std::slice::from_ref(&location),
                    &[location.oid],
                    &location,
                    header,
                )
                .is_err(),
                "unproven local base accepted: {header:?}"
            );
        }
    }
}
