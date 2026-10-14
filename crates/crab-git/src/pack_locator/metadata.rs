//! Authenticated pack-offset facts; Git locations remain in the parent module.

use super::{PackLocationIter, PackLocatorError, SHA1_LEN, read_u32_le, read_u64_le};
use std::path::PathBuf;

const KIND_METADATA_MAGIC: &[u8; 8] = b"CRBKIND1";
const KIND_METADATA_VERSION: u32 = 1;
const KIND_METADATA_EXTERNAL_BASE_VERSION: u32 = 2;
const KIND_METADATA_HEADER_LEN: usize = KIND_METADATA_MAGIC.len() + 4 + 8 + SHA1_LEN;
const KIND_METADATA_TRAILER_LEN: usize = 32;
const KIND_METADATA_EXTERNAL_COUNT_LEN: usize = 8;
const KIND_METADATA_EXTERNAL_ENTRY_LEN: usize = 8 + SHA1_LEN;
const KIND_METADATA_SIZE_VERSION: u32 = 3;

/// Return the exact byte size of Crab's checksummed object-kind sidecar.
#[must_use]
pub fn pack_kind_metadata_size(object_count: u64) -> Option<u64> {
    (KIND_METADATA_HEADER_LEN as u64)
        .checked_add(object_count)?
        .checked_add(KIND_METADATA_TRAILER_LEN as u64)
}

/// Return the maximum byte size of a valid object-kind sidecar.
#[must_use]
pub fn max_pack_kind_metadata_size(object_count: u64) -> Option<u64> {
    (KIND_METADATA_HEADER_LEN as u64)
        .checked_add(object_count.checked_mul(9)?)?
        .checked_add(KIND_METADATA_EXTERNAL_COUNT_LEN as u64)?
        .checked_add(object_count.checked_mul(KIND_METADATA_EXTERNAL_ENTRY_LEN as u64)?)?
        .checked_add(KIND_METADATA_TRAILER_LEN as u64)
}

/// Encode one compact, checksummed object-kind sidecar.
///
/// Entries are ordered by increasing pack offset, matching
/// [`PackLocationIter`]. The sidecar is deliberately separate from the JSON
/// pack metadata so normal fetches never download a repository-sized kind
/// table; generation owners read it only when they must rebuild locator rows.
pub fn encode_pack_kind_metadata(
    pack_checksum: gix_hash::ObjectId,
    kinds: &[gix_object::Kind],
) -> Result<Vec<u8>, PackLocatorError> {
    encode_pack_kind_metadata_v1(pack_checksum, kinds)
}

/// Encode kinds plus sparse identities for bases that live outside this pack.
pub fn encode_pack_kind_metadata_with_external_deltas(
    pack_checksum: gix_hash::ObjectId,
    entries: &[(gix_object::Kind, Option<gix_hash::ObjectId>)],
) -> Result<Vec<u8>, PackLocatorError> {
    if entries.iter().all(|(_, base)| base.is_none()) {
        return encode_pack_kind_metadata_v1(
            pack_checksum,
            &entries.iter().map(|(kind, _)| *kind).collect::<Vec<_>>(),
        );
    }
    let entries = entries
        .iter()
        .map(|(kind, base)| PackObjectMetadata {
            kind: *kind,
            logical_size: None,
            external_delta_base: *base,
        })
        .collect::<Vec<_>>();
    encode_metadata(pack_checksum, &entries, KIND_METADATA_EXTERNAL_BASE_VERSION)
}

/// Logical facts proven by the pack producer, in increasing pack-offset order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackObjectMetadata {
    pub kind: gix_object::Kind,
    /// Resolved Git object size; released kind-only sidecars leave this absent.
    pub logical_size: Option<u64>,
    pub external_delta_base: Option<gix_hash::ObjectId>,
}

/// Encode proven resolved sizes with object kinds and sparse cross-pack bases.
pub fn encode_pack_object_metadata(
    pack_checksum: gix_hash::ObjectId,
    entries: &[(gix_object::Kind, u64, Option<gix_hash::ObjectId>)],
) -> Result<Vec<u8>, PackLocatorError> {
    let entries = entries
        .iter()
        .map(|(kind, size, base)| PackObjectMetadata {
            kind: *kind,
            logical_size: Some(*size),
            external_delta_base: *base,
        })
        .collect::<Vec<_>>();
    encode_metadata(pack_checksum, &entries, KIND_METADATA_SIZE_VERSION)
}

fn encode_metadata(
    pack_checksum: gix_hash::ObjectId,
    entries: &[PackObjectMetadata],
    version: u32,
) -> Result<Vec<u8>, PackLocatorError> {
    validate_kind_metadata_checksum(pack_checksum)?;
    let overflow = || PackLocatorError::Overflow {
        path: PathBuf::from("pack kind metadata"),
    };
    let object_count = u64::try_from(entries.len()).map_err(|_| overflow())?;
    let external_count = u64::try_from(
        entries
            .iter()
            .filter(|entry| entry.external_delta_base.is_some())
            .count(),
    )
    .map_err(|_| overflow())?;
    let width = if version == KIND_METADATA_SIZE_VERSION {
        9
    } else {
        1
    };
    let capacity = (KIND_METADATA_HEADER_LEN as u64)
        .checked_add(object_count.checked_mul(width).ok_or_else(overflow)?)
        .and_then(|size| size.checked_add(KIND_METADATA_EXTERNAL_COUNT_LEN as u64))
        .and_then(|size| {
            size.checked_add(external_count.checked_mul(KIND_METADATA_EXTERNAL_ENTRY_LEN as u64)?)
        })
        .and_then(|size| size.checked_add(KIND_METADATA_TRAILER_LEN as u64))
        .ok_or_else(overflow)?;
    let mut bytes = Vec::with_capacity(usize::try_from(capacity).map_err(|_| overflow())?);
    encode_kind_metadata_header(&mut bytes, version, object_count, pack_checksum);
    bytes.extend(entries.iter().map(|entry| kind_code(entry.kind)));
    if version == KIND_METADATA_SIZE_VERSION {
        for entry in entries {
            let size = entry
                .logical_size
                .ok_or_else(|| PackLocatorError::InvalidKindMetadata {
                    reason: "resolved object size is absent".to_owned(),
                })?;
            bytes.extend_from_slice(&size.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&external_count.to_le_bytes());
    for (index, entry) in entries.iter().enumerate() {
        let Some(base) = entry.external_delta_base else {
            continue;
        };
        validate_kind_metadata_checksum(base)?;
        let index = u64::try_from(index).map_err(|_| overflow())?;
        bytes.extend_from_slice(&index.to_le_bytes());
        bytes.extend_from_slice(base.as_bytes());
    }
    append_kind_metadata_digest(&mut bytes);
    Ok(bytes)
}

fn encode_pack_kind_metadata_v1(
    pack_checksum: gix_hash::ObjectId,
    kinds: &[gix_object::Kind],
) -> Result<Vec<u8>, PackLocatorError> {
    validate_kind_metadata_checksum(pack_checksum)?;
    let object_count = u64::try_from(kinds.len()).map_err(|_| PackLocatorError::Overflow {
        path: PathBuf::from("pack kind metadata"),
    })?;
    let capacity = usize::try_from(pack_kind_metadata_size(object_count).ok_or_else(|| {
        PackLocatorError::Overflow {
            path: PathBuf::from("pack kind metadata"),
        }
    })?)
    .map_err(|_| PackLocatorError::Overflow {
        path: PathBuf::from("pack kind metadata"),
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    encode_kind_metadata_header(
        &mut bytes,
        KIND_METADATA_VERSION,
        object_count,
        pack_checksum,
    );
    for kind in kinds {
        bytes.push(kind_code(*kind));
    }
    append_kind_metadata_digest(&mut bytes);
    Ok(bytes)
}

fn validate_kind_metadata_checksum(
    pack_checksum: gix_hash::ObjectId,
) -> Result<(), PackLocatorError> {
    if pack_checksum.as_bytes().len() != SHA1_LEN {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: "kind metadata supports only SHA-1 pack checksums".to_owned(),
        });
    }
    Ok(())
}

fn encode_kind_metadata_header(
    bytes: &mut Vec<u8>,
    version: u32,
    object_count: u64,
    pack_checksum: gix_hash::ObjectId,
) {
    bytes.extend_from_slice(KIND_METADATA_MAGIC);
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes.extend_from_slice(&object_count.to_le_bytes());
    bytes.extend_from_slice(pack_checksum.as_bytes());
}

fn append_kind_metadata_digest(bytes: &mut Vec<u8>) {
    let digest = blake3::hash(bytes);
    bytes.extend_from_slice(digest.as_bytes());
}

/// Decode and bind one kind sidecar to its verified pack locations.
///
/// The returned pairs retain pack-offset order. The sidecar checksum, object
/// count, and Git pack checksum must all agree with the supplied index pair.
pub fn decode_pack_kind_metadata(
    bytes: &[u8],
    locations: PackLocationIter,
) -> Result<Vec<(gix_hash::ObjectId, gix_object::Kind)>, PackLocatorError> {
    decode_pack_kind_metadata_iter(bytes, locations)?.collect()
}

/// Decode a kind sidecar into a bounded-memory pack-offset iterator.
pub fn decode_pack_kind_metadata_iter(
    bytes: &[u8],
    locations: PackLocationIter,
) -> Result<PackKindMetadataIter, PackLocatorError> {
    let pack_checksum = locations.pack_checksum();
    let object_count = locations.object_count();
    let kinds = decode_pack_kind_metadata_payload(bytes, pack_checksum, object_count)?
        .into_iter()
        .map(|entry| entry.kind)
        .collect::<Vec<_>>();
    Ok(PackKindMetadataIter {
        locations,
        kinds: kinds.into_iter(),
    })
}

/// Decode object kinds and cross-pack base identities in pack-offset order.
pub fn decode_pack_kind_metadata_with_external_deltas(
    bytes: &[u8],
    locations: PackLocationIter,
) -> Result<
    Vec<(
        gix_hash::ObjectId,
        gix_object::Kind,
        Option<gix_hash::ObjectId>,
    )>,
    PackLocatorError,
> {
    Ok(decode_pack_object_metadata(bytes, locations)?
        .into_iter()
        .map(|(oid, entry)| (oid, entry.kind, entry.external_delta_base))
        .collect())
}

/// Decode authenticated kind, resolved size and external base facts against locations.
pub fn decode_pack_object_metadata(
    bytes: &[u8],
    locations: PackLocationIter,
) -> Result<Vec<(gix_hash::ObjectId, PackObjectMetadata)>, PackLocatorError> {
    let metadata = decode_pack_kind_metadata_payload(
        bytes,
        locations.pack_checksum(),
        locations.object_count(),
    )?;
    locations
        .zip(metadata)
        .map(|(location, entry)| {
            let location = location?;
            if entry.external_delta_base == Some(location.oid) {
                return Err(PackLocatorError::InvalidKindMetadata {
                    reason: format!(
                        "object {} uses itself as an external delta base",
                        location.oid
                    ),
                });
            }
            Ok((location.oid, entry))
        })
        .collect()
}

/// Decode sidecar records in increasing pack-offset order.
pub fn decode_pack_kind_metadata_records(
    bytes: &[u8],
    pack_checksum: gix_hash::ObjectId,
    object_count: u64,
) -> Result<Vec<(gix_object::Kind, Option<gix_hash::ObjectId>)>, PackLocatorError> {
    Ok(
        decode_pack_kind_metadata_payload(bytes, pack_checksum, object_count)?
            .into_iter()
            .map(|entry| (entry.kind, entry.external_delta_base))
            .collect(),
    )
}

/// Decode authenticated fact records in increasing pack-offset order.
pub fn decode_pack_object_metadata_records(
    bytes: &[u8],
    pack_checksum: gix_hash::ObjectId,
    object_count: u64,
) -> Result<Vec<PackObjectMetadata>, PackLocatorError> {
    decode_pack_kind_metadata_payload(bytes, pack_checksum, object_count)
}

/// Validate one kind sidecar against a pack checksum and object count.
pub fn validate_pack_kind_metadata(
    bytes: &[u8],
    pack_checksum: gix_hash::ObjectId,
    object_count: u64,
) -> Result<(), PackLocatorError> {
    decode_pack_kind_metadata_payload(bytes, pack_checksum, object_count).map(|_| ())
}

fn decode_pack_kind_metadata_payload(
    bytes: &[u8],
    pack_checksum: gix_hash::ObjectId,
    object_count: u64,
) -> Result<Vec<PackObjectMetadata>, PackLocatorError> {
    if bytes.len() < KIND_METADATA_HEADER_LEN + KIND_METADATA_TRAILER_LEN {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: "sidecar is truncated".to_owned(),
        });
    }
    if bytes.get(..KIND_METADATA_MAGIC.len()) != Some(KIND_METADATA_MAGIC) {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: "missing signature".to_owned(),
        });
    }
    let version_start = KIND_METADATA_MAGIC.len();
    let version =
        read_u32_le(bytes, version_start).ok_or_else(|| PackLocatorError::InvalidKindMetadata {
            reason: "truncated version".to_owned(),
        })?;
    if ![
        KIND_METADATA_VERSION,
        KIND_METADATA_EXTERNAL_BASE_VERSION,
        KIND_METADATA_SIZE_VERSION,
    ]
    .contains(&version)
    {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: format!("unsupported version {version}"),
        });
    }
    let count_start = version_start + 4;
    let encoded_object_count =
        read_u64_le(bytes, count_start).ok_or_else(|| PackLocatorError::InvalidKindMetadata {
            reason: "truncated object count".to_owned(),
        })?;
    if encoded_object_count != object_count {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: format!(
                "object count {} does not match expected {object_count}",
                encoded_object_count
            ),
        });
    }
    let object_count =
        usize::try_from(object_count).map_err(|_| PackLocatorError::InvalidKindMetadata {
            reason: "object count does not fit in usize".to_owned(),
        })?;
    let checksum_start = count_start + 8;
    let checksum_end = checksum_start + SHA1_LEN;
    if bytes.get(checksum_start..checksum_end) != Some(pack_checksum.as_bytes()) {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: "pack checksum does not match expected checksum".to_owned(),
        });
    }
    let digest_start = bytes.len() - KIND_METADATA_TRAILER_LEN;
    if blake3::hash(&bytes[..digest_start]).as_bytes() != &bytes[digest_start..] {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: "sidecar checksum mismatch".to_owned(),
        });
    }
    let kind_start = checksum_end;
    let kind_end = kind_start.checked_add(object_count).ok_or_else(|| {
        PackLocatorError::InvalidKindMetadata {
            reason: "kind payload length overflowed".to_owned(),
        }
    })?;
    let kind_bytes =
        bytes
            .get(kind_start..kind_end)
            .ok_or_else(|| PackLocatorError::InvalidKindMetadata {
                reason: "kind payload is truncated".to_owned(),
            })?;
    let sizes_end = if version == KIND_METADATA_SIZE_VERSION {
        object_count
            .checked_mul(8)
            .and_then(|size| kind_end.checked_add(size))
    } else {
        Some(kind_end)
    }
    .filter(|end| *end <= digest_start)
    .ok_or_else(|| PackLocatorError::InvalidKindMetadata {
        reason: "size payload is truncated or overflowing".to_owned(),
    })?;
    let mut metadata = kind_bytes
        .iter()
        .map(|code| {
            decode_kind_code(*code).map(|kind| PackObjectMetadata {
                kind,
                logical_size: None,
                external_delta_base: None,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if version == KIND_METADATA_VERSION {
        if kind_end != digest_start {
            return Err(PackLocatorError::InvalidKindMetadata {
                reason: "version 1 sidecar has trailing payload".to_owned(),
            });
        }
        return Ok(metadata);
    }

    if version == KIND_METADATA_SIZE_VERSION {
        for (entry, size) in metadata
            .iter_mut()
            .zip(bytes[kind_end..sizes_end].as_chunks::<8>().0)
        {
            entry.logical_size = Some(u64::from_le_bytes(*size));
        }
    }
    let external_count = read_u64_le(&bytes[..digest_start], sizes_end).ok_or_else(|| {
        PackLocatorError::InvalidKindMetadata {
            reason: "external delta count is truncated".to_owned(),
        }
    })?;
    let external_count =
        usize::try_from(external_count).map_err(|_| PackLocatorError::InvalidKindMetadata {
            reason: "external delta count does not fit in memory".to_owned(),
        })?;
    if external_count > object_count {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: "external delta count exceeds object count".to_owned(),
        });
    }
    let records_start = sizes_end + KIND_METADATA_EXTERNAL_COUNT_LEN;
    let records_len = external_count
        .checked_mul(KIND_METADATA_EXTERNAL_ENTRY_LEN)
        .ok_or_else(|| PackLocatorError::InvalidKindMetadata {
            reason: "external delta payload length overflowed".to_owned(),
        })?;
    if records_start.checked_add(records_len) != Some(digest_start) {
        return Err(PackLocatorError::InvalidKindMetadata {
            reason: "external delta payload length does not match its count".to_owned(),
        });
    }
    let mut previous_index = None;
    for record in bytes[records_start..digest_start]
        .as_chunks::<KIND_METADATA_EXTERNAL_ENTRY_LEN>()
        .0
    {
        let index =
            read_u64_le(record, 0).ok_or_else(|| PackLocatorError::InvalidKindMetadata {
                reason: "external delta object index is truncated".to_owned(),
            })?;
        let index = usize::try_from(index).map_err(|_| PackLocatorError::InvalidKindMetadata {
            reason: "external delta object index does not fit in memory".to_owned(),
        })?;
        if version == KIND_METADATA_SIZE_VERSION
            && previous_index.is_some_and(|previous| index <= previous)
        {
            return Err(PackLocatorError::InvalidKindMetadata {
                reason: "external delta ordinals are not increasing".to_owned(),
            });
        }
        previous_index = Some(index);
        let entry =
            metadata
                .get_mut(index)
                .ok_or_else(|| PackLocatorError::InvalidKindMetadata {
                    reason: "external delta object index exceeds object count".to_owned(),
                })?;
        if entry.external_delta_base.is_some() {
            return Err(PackLocatorError::InvalidKindMetadata {
                reason: "external delta object index is duplicated".to_owned(),
            });
        }
        let base = <[u8; SHA1_LEN]>::try_from(&record[8..]).map_err(|_| {
            PackLocatorError::InvalidKindMetadata {
                reason: "external delta base identity is truncated".to_owned(),
            }
        })?;
        entry.external_delta_base = Some(gix_hash::ObjectId::from(base));
    }
    Ok(metadata)
}

/// Iterator over object IDs and kinds in increasing pack-offset order.
pub struct PackKindMetadataIter {
    locations: PackLocationIter,
    kinds: std::vec::IntoIter<gix_object::Kind>,
}

impl Iterator for PackKindMetadataIter {
    type Item = Result<(gix_hash::ObjectId, gix_object::Kind), PackLocatorError>;

    fn next(&mut self) -> Option<Self::Item> {
        let kind = self.kinds.next()?;
        let location = self.locations.next()?;
        Some(location.map(|location| (location.oid, kind)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.kinds.size_hint()
    }
}

impl ExactSizeIterator for PackKindMetadataIter {}

fn kind_code(kind: gix_object::Kind) -> u8 {
    match kind {
        gix_object::Kind::Commit => 1,
        gix_object::Kind::Tree => 2,
        gix_object::Kind::Blob => 3,
        gix_object::Kind::Tag => 4,
    }
}

fn decode_kind_code(code: u8) -> Result<gix_object::Kind, PackLocatorError> {
    match code {
        1 => Ok(gix_object::Kind::Commit),
        2 => Ok(gix_object::Kind::Tree),
        3 => Ok(gix_object::Kind::Blob),
        4 => Ok(gix_object::Kind::Tag),
        other => Err(PackLocatorError::InvalidKindMetadata {
            reason: format!("unsupported object kind code {other}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_facts_round_trip_and_released_records_remain_absent() {
        let checksum = gix_hash::ObjectId::from([1; 20]);
        let base = gix_hash::ObjectId::from([2; 20]);
        let entries = [
            (gix_object::Kind::Blob, 0, None),
            (gix_object::Kind::Tree, u64::MAX, Some(base)),
        ];
        let bytes = encode_pack_object_metadata(checksum, &entries).unwrap();
        let decoded = decode_pack_kind_metadata_payload(&bytes, checksum, 2).unwrap();
        assert_eq!(
            decoded
                .iter()
                .map(|entry| (entry.kind, entry.logical_size, entry.external_delta_base))
                .collect::<Vec<_>>(),
            entries
                .iter()
                .map(|(kind, size, base)| (*kind, Some(*size), *base))
                .collect::<Vec<_>>()
        );
        assert!(bytes.len() as u64 <= max_pack_kind_metadata_size(2).unwrap());
        for bytes in [
            encode_pack_kind_metadata(checksum, &[gix_object::Kind::Blob; 2]).unwrap(),
            encode_pack_kind_metadata_with_external_deltas(
                checksum,
                &entries
                    .iter()
                    .map(|(kind, _, base)| (*kind, *base))
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        ] {
            assert!(
                decode_pack_kind_metadata_payload(&bytes, checksum, 2)
                    .unwrap()
                    .iter()
                    .all(|entry| entry.logical_size.is_none())
            );
        }
    }

    #[test]
    fn resolved_sidecar_rejects_corruption_and_unbounded_counts() {
        let checksum = gix_hash::ObjectId::from([1; 20]);
        let base = gix_hash::ObjectId::from([2; 20]);
        let good =
            encode_pack_object_metadata(checksum, &[(gix_object::Kind::Blob, 4096, Some(base)); 3])
                .unwrap();
        let sizes = KIND_METADATA_HEADER_LEN + 3;
        let external_count = sizes + 24;
        let records = external_count + 8;
        for fault in [
            "tampered_size",
            "truncated_sizes",
            "huge_count",
            "excess_external_count",
            "bad_ordinal",
            "duplicate",
            "unordered",
            "trailing",
        ] {
            let mut bytes = good[..good.len() - KIND_METADATA_TRAILER_LEN].to_vec();
            let expected_count = if fault == "huge_count" { u64::MAX } else { 3 };
            match fault {
                "tampered_size" => bytes[sizes] ^= 1,
                "truncated_sizes" => bytes.truncate(sizes + 7),
                "huge_count" => bytes[12..20].copy_from_slice(&u64::MAX.to_le_bytes()),
                "excess_external_count" => {
                    bytes[external_count..external_count + 8].copy_from_slice(&4_u64.to_le_bytes())
                }
                "bad_ordinal" => bytes[records..records + 8].copy_from_slice(&3_u64.to_le_bytes()),
                "duplicate" => {
                    bytes[records + 28..records + 36].copy_from_slice(&0_u64.to_le_bytes())
                }
                "unordered" => bytes[records..records + 8].copy_from_slice(&2_u64.to_le_bytes()),
                "trailing" => bytes.push(0),
                _ => unreachable!(),
            }
            if fault == "tampered_size" {
                bytes.extend_from_slice(&good[good.len() - KIND_METADATA_TRAILER_LEN..]);
            } else {
                append_kind_metadata_digest(&mut bytes);
            }
            assert!(
                decode_pack_kind_metadata_payload(&bytes, checksum, expected_count).is_err(),
                "{fault}"
            );
        }
        assert!(
            decode_pack_kind_metadata_payload(&good, gix_hash::ObjectId::from([9; 20]), 3).is_err()
        );
    }
}
