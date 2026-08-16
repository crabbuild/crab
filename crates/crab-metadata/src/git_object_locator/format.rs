use crab_xet::hash::MerkleHash;

use super::{GitLocatorCoverage, GitObjectLocation, GitPackLocatorRecord};

pub(crate) const METADATA_KEY: [u8; 1] = [0x00];
pub(crate) const OBJECT_FAMILY: u8 = 0x01;
pub(crate) const PACK_FAMILY: u8 = 0x02;
pub(crate) const FORMAT_FINGERPRINT: [u8; 8] = *b"CRABGLOC";
pub(crate) const OBJECT_KEY_LEN: usize = 21;
pub(crate) const OBJECT_VALUE_LEN: usize = 28;
pub(crate) const PACK_KEY_LEN: usize = 9;
pub(crate) const PACK_VALUE_LEN: usize = 88;
pub(crate) const METADATA_VALUE_LEN: usize = 57;

const PACK_HEADER_LEN: u64 = 12;
const PACK_TRAILER_LEN: u64 = 20;
const MIN_PACK_SIZE: u64 = PACK_HEADER_LEN + PACK_TRAILER_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredObjectLocation {
    pub(crate) pack_slot: u64,
    pub(crate) pack_offset: u64,
    pub(crate) entry_len: u64,
    pub(crate) crc32: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocatorMetadata {
    pub(crate) next_pack_slot: u64,
    pub(crate) coverage: Option<GitLocatorCoverage>,
}

impl LocatorMetadata {
    pub(crate) const fn empty() -> Self {
        Self {
            next_pack_slot: 1,
            coverage: None,
        }
    }
}

pub(crate) fn object_key(oid: &[u8; 20]) -> [u8; OBJECT_KEY_LEN] {
    let mut key = [0; OBJECT_KEY_LEN];
    key[0] = OBJECT_FAMILY;
    key[1..].copy_from_slice(oid);
    key
}

pub(crate) fn decode_object_key(bytes: &[u8]) -> Option<[u8; 20]> {
    if bytes.len() != OBJECT_KEY_LEN || bytes[0] != OBJECT_FAMILY {
        return None;
    }
    array(bytes, 1)
}

pub(crate) fn pack_key(pack_slot: u64) -> Option<[u8; PACK_KEY_LEN]> {
    if pack_slot == 0 {
        return None;
    }
    let mut key = [0; PACK_KEY_LEN];
    key[0] = PACK_FAMILY;
    key[1..].copy_from_slice(&pack_slot.to_be_bytes());
    Some(key)
}

pub(crate) fn decode_pack_key(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != PACK_KEY_LEN || bytes[0] != PACK_FAMILY {
        return None;
    }
    let pack_slot = u64::from_be_bytes(array(bytes, 1)?);
    (pack_slot != 0).then_some(pack_slot)
}

pub(crate) fn encode_object_location(location: StoredObjectLocation) -> [u8; OBJECT_VALUE_LEN] {
    let mut bytes = [0; OBJECT_VALUE_LEN];
    bytes[..8].copy_from_slice(&location.pack_slot.to_be_bytes());
    bytes[8..16].copy_from_slice(&location.pack_offset.to_be_bytes());
    bytes[16..24].copy_from_slice(&location.entry_len.to_be_bytes());
    bytes[24..].copy_from_slice(&location.crc32.to_be_bytes());
    bytes
}

pub(crate) fn decode_object_location(bytes: &[u8]) -> Option<StoredObjectLocation> {
    if bytes.len() != OBJECT_VALUE_LEN {
        return None;
    }
    let location = StoredObjectLocation {
        pack_slot: u64::from_be_bytes(array(bytes, 0)?),
        pack_offset: u64::from_be_bytes(array(bytes, 8)?),
        entry_len: u64::from_be_bytes(array(bytes, 16)?),
        crc32: u32::from_be_bytes(array(bytes, 24)?),
    };
    if location.pack_slot == 0
        || location.entry_len == 0
        || location
            .pack_offset
            .checked_add(location.entry_len)
            .is_none()
    {
        return None;
    }
    Some(location)
}

pub(crate) fn encode_pack_record(record: GitPackLocatorRecord) -> [u8; PACK_VALUE_LEN] {
    let mut bytes = [0; PACK_VALUE_LEN];
    bytes[..32].copy_from_slice(&<[u8; 32]>::from(record.pack_id));
    bytes[32..40].copy_from_slice(&record.committed_generation.to_be_bytes());
    bytes[40..72].copy_from_slice(&<[u8; 32]>::from(record.pack_index_hash));
    bytes[72..80].copy_from_slice(&record.object_count.to_be_bytes());
    bytes[80..].copy_from_slice(&record.pack_size.to_be_bytes());
    bytes
}

pub(crate) fn decode_pack_record(bytes: &[u8]) -> Option<GitPackLocatorRecord> {
    if bytes.len() != PACK_VALUE_LEN {
        return None;
    }
    let record = GitPackLocatorRecord {
        pack_id: MerkleHash::from(array(bytes, 0)?),
        committed_generation: u64::from_be_bytes(array(bytes, 32)?),
        pack_index_hash: MerkleHash::from(array(bytes, 40)?),
        object_count: u64::from_be_bytes(array(bytes, 72)?),
        pack_size: u64::from_be_bytes(array(bytes, 80)?),
    };
    if record.committed_generation == 0 || record.pack_size < MIN_PACK_SIZE {
        return None;
    }
    Some(record)
}

pub(crate) fn encode_metadata(metadata: LocatorMetadata) -> [u8; METADATA_VALUE_LEN] {
    let mut bytes = [0; METADATA_VALUE_LEN];
    bytes[..8].copy_from_slice(&FORMAT_FINGERPRINT);
    bytes[8..16].copy_from_slice(&metadata.next_pack_slot.to_be_bytes());
    if let Some(coverage) = metadata.coverage {
        bytes[16] = 1;
        bytes[17..25].copy_from_slice(&coverage.generation.to_be_bytes());
        bytes[25..].copy_from_slice(&<[u8; 32]>::from(coverage.pack_index_hash));
    }
    bytes
}

pub(crate) fn decode_metadata(bytes: &[u8]) -> Option<LocatorMetadata> {
    if bytes.len() != METADATA_VALUE_LEN || bytes[..8] != FORMAT_FINGERPRINT {
        return None;
    }
    let next_pack_slot = u64::from_be_bytes(array(bytes, 8)?);
    if next_pack_slot == 0 {
        return None;
    }
    let coverage = match bytes[16] {
        0 if bytes[17..].iter().all(|byte| *byte == 0) => None,
        1 => {
            let generation = u64::from_be_bytes(array(bytes, 17)?);
            if generation == 0 {
                return None;
            }
            Some(GitLocatorCoverage {
                generation,
                pack_index_hash: MerkleHash::from(array(bytes, 25)?),
            })
        }
        _ => return None,
    };
    Some(LocatorMetadata {
        next_pack_slot,
        coverage,
    })
}

pub(crate) fn validate_location_for_pack(location: GitObjectLocation, pack_size: u64) -> bool {
    if pack_size < MIN_PACK_SIZE
        || location.pack_offset < PACK_HEADER_LEN
        || location.entry_len == 0
    {
        return false;
    }
    location
        .pack_offset
        .checked_add(location.entry_len)
        .is_some_and(|end| end <= pack_size - PACK_TRAILER_LEN)
}

fn array<const N: usize>(bytes: &[u8], start: usize) -> Option<[u8; N]> {
    bytes.get(start..start.checked_add(N)?)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: u64) -> MerkleHash {
        MerkleHash::from([seed, seed + 1, seed + 2, seed + 3])
    }

    #[test]
    fn object_row_is_exactly_one_family_byte_plus_sha1_and_28_value_bytes() {
        let oid = [0x11; 20];
        let location = StoredObjectLocation {
            pack_slot: 0x0102_0304_0506_0708,
            pack_offset: 0x1112_1314_1516_1718,
            entry_len: 0x2122_2324_2526_2728,
            crc32: 0x3132_3334,
        };

        let key = object_key(&oid);
        let value = encode_object_location(location);

        assert_eq!(key.len(), 21);
        assert_eq!(key[0], OBJECT_FAMILY);
        assert_eq!(&key[1..], &oid);
        assert_eq!(value.len(), 28);
        assert_eq!(&value[..8], &location.pack_slot.to_be_bytes());
        assert_eq!(&value[8..16], &location.pack_offset.to_be_bytes());
        assert_eq!(&value[16..24], &location.entry_len.to_be_bytes());
        assert_eq!(&value[24..], &location.crc32.to_be_bytes());
        assert_eq!(decode_object_key(&key), Some(oid));
        assert_eq!(decode_object_location(&value), Some(location));
    }

    #[test]
    fn metadata_rejects_unknown_fingerprint_slot_zero_and_invalid_coverage_flag() {
        let valid = LocatorMetadata::empty();
        let mut bytes = encode_metadata(valid);
        bytes[0] = b'X';
        assert_eq!(decode_metadata(&bytes), None);
        let mut bytes = encode_metadata(valid);
        bytes[8..16].fill(0);
        assert_eq!(decode_metadata(&bytes), None);
        let mut bytes = encode_metadata(valid);
        bytes[16] = 2;
        assert_eq!(decode_metadata(&bytes), None);
    }

    #[test]
    fn metadata_requires_zero_absent_coverage_and_nonzero_present_generation() {
        let mut absent = encode_metadata(LocatorMetadata::empty());
        absent[25] = 1;
        assert_eq!(decode_metadata(&absent), None);

        let present = LocatorMetadata {
            next_pack_slot: 9,
            coverage: Some(GitLocatorCoverage {
                generation: 7,
                pack_index_hash: hash(3),
            }),
        };
        assert_eq!(decode_metadata(&encode_metadata(present)), Some(present));

        let mut zero_generation = encode_metadata(present);
        zero_generation[17..25].fill(0);
        assert_eq!(decode_metadata(&zero_generation), None);
    }

    #[test]
    fn compact_decoders_reject_wrong_lengths_zero_slots_and_invalid_ranges() {
        assert_eq!(decode_object_key(&[OBJECT_FAMILY; 20]), None);
        assert_eq!(decode_pack_key(&[PACK_FAMILY; 8]), None);
        assert_eq!(pack_key(0), None);

        let mut object = encode_object_location(StoredObjectLocation {
            pack_slot: 1,
            pack_offset: 12,
            entry_len: 20,
            crc32: 4,
        });
        object[..8].fill(0);
        assert_eq!(decode_object_location(&object), None);
        object[..8].copy_from_slice(&1_u64.to_be_bytes());
        object[16..24].fill(0);
        assert_eq!(decode_object_location(&object), None);
        object[8..16].copy_from_slice(&u64::MAX.to_be_bytes());
        object[16..24].copy_from_slice(&1_u64.to_be_bytes());
        assert_eq!(decode_object_location(&object), None);
    }

    #[test]
    fn pack_record_is_exact_and_rejects_invalid_generation_or_size() {
        let record = GitPackLocatorRecord {
            pack_id: hash(1),
            committed_generation: 0x0102_0304_0506_0708,
            pack_index_hash: hash(2),
            object_count: 0x1112_1314_1516_1718,
            pack_size: 0x2122_2324_2526_2728,
        };
        let value = encode_pack_record(record);
        assert_eq!(value.len(), PACK_VALUE_LEN);
        assert_eq!(&value[32..40], &record.committed_generation.to_be_bytes());
        assert_eq!(&value[72..80], &record.object_count.to_be_bytes());
        assert_eq!(&value[80..], &record.pack_size.to_be_bytes());
        assert_eq!(decode_pack_record(&value), Some(record));

        let mut invalid = value;
        invalid[32..40].fill(0);
        assert_eq!(decode_pack_record(&invalid), None);
        invalid[32..40].copy_from_slice(&1_u64.to_be_bytes());
        invalid[80..].copy_from_slice(&(MIN_PACK_SIZE - 1).to_be_bytes());
        assert_eq!(decode_pack_record(&invalid), None);
    }

    #[test]
    fn location_validation_excludes_pack_header_trailer_and_overflow() {
        assert!(validate_location_for_pack(
            GitObjectLocation {
                pack_offset: 12,
                entry_len: 96,
                crc32: 1,
            },
            128,
        ));
        assert!(!validate_location_for_pack(
            GitObjectLocation {
                pack_offset: 11,
                entry_len: 1,
                crc32: 1,
            },
            128,
        ));
        assert!(!validate_location_for_pack(
            GitObjectLocation {
                pack_offset: u64::MAX,
                entry_len: 1,
                crc32: 1,
            },
            128,
        ));
    }
}
