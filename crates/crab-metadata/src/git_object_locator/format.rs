use crab_xet::hash::MerkleHash;

use super::{
    GitLocatorCoverage, GitObjectCatalogIdentity, GitObjectKind, GitObjectLocation,
    GitObjectMetadata, GitObjectOrdinal, GitPackLocatorRecord,
};

pub(crate) const METADATA_KEY: [u8; 1] = [0x00];
pub(crate) const OBJECT_FAMILY: u8 = 0x01;
pub(crate) const PACK_FAMILY: u8 = 0x02;
pub(crate) const ORDINAL_FAMILY: u8 = 0x03;
pub(crate) const ORDINAL_METADATA_FAMILY: u8 = 0x04;
// Derived reverse membership rows let the writer sweep stale packs without
// scanning the canonical OID catalog; readers never use this family.
pub(crate) const PACK_OBJECT_FAMILY: u8 = 0x05;
pub(crate) const FORMAT_FINGERPRINT: [u8; 8] = *b"CRABGCAT";
pub(crate) const OBJECT_KEY_LEN: usize = 21;
pub(crate) const OBJECT_VALUE_LEN: usize = 64;
pub(crate) const PACK_KEY_LEN: usize = 9;
pub(crate) const PACK_VALUE_LEN: usize = 88;
pub(crate) const ORDINAL_KEY_LEN: usize = 5;
pub(crate) const ORDINAL_METADATA_VALUE_LEN: usize = 32;
pub(crate) const METADATA_VALUE_LEN: usize = 101;
pub(crate) const PACK_OBJECT_KEY_LEN: usize = 29;
pub(crate) const PACK_OBJECT_PREFIX_LEN: usize = 9;
pub(crate) const PACK_OBJECT_VALUE_LEN: usize = 4;
pub(crate) const PACK_OBJECT_INDEX_MARKER_KEY: [u8; 2] = [PACK_OBJECT_FAMILY, 0xff];
pub(crate) const PACK_OBJECT_INDEX_REBUILDING_VALUE: [u8; 1] = [0];
pub(crate) const PACK_OBJECT_INDEX_MARKER_VALUE: [u8; 1] = [1];

const PACK_HEADER_LEN: u64 = 12;
const PACK_TRAILER_LEN: u64 = 20;
const MIN_PACK_SIZE: u64 = PACK_HEADER_LEN + PACK_TRAILER_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoredObjectLocation {
    pub(crate) ordinal: GitObjectOrdinal,
    pub(crate) pack_slot: u64,
    pub(crate) pack_offset: u64,
    pub(crate) entry_len: u64,
    pub(crate) crc32: u32,
    pub(crate) metadata: GitObjectMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocatorMetadata {
    pub(crate) next_pack_slot: u64,
    pub(crate) next_object_ordinal: u64,
    pub(crate) identity: Option<GitObjectCatalogIdentity>,
}

impl LocatorMetadata {
    pub(crate) const fn empty() -> Self {
        Self {
            next_pack_slot: 1,
            next_object_ordinal: 0,
            identity: None,
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

pub(crate) fn ordinal_key(ordinal: GitObjectOrdinal) -> [u8; ORDINAL_KEY_LEN] {
    let mut key = [0; ORDINAL_KEY_LEN];
    key[0] = ORDINAL_FAMILY;
    key[1..].copy_from_slice(&ordinal.to_be_bytes());
    key
}

pub(crate) fn decode_ordinal_key(bytes: &[u8]) -> Option<GitObjectOrdinal> {
    if bytes.len() != ORDINAL_KEY_LEN || bytes[0] != ORDINAL_FAMILY {
        return None;
    }
    Some(u32::from_be_bytes(array(bytes, 1)?))
}

pub(crate) fn ordinal_metadata_key(ordinal: GitObjectOrdinal) -> [u8; ORDINAL_KEY_LEN] {
    let mut key = [0; ORDINAL_KEY_LEN];
    key[0] = ORDINAL_METADATA_FAMILY;
    key[1..].copy_from_slice(&ordinal.to_be_bytes());
    key
}

pub(crate) fn decode_ordinal_metadata_key(bytes: &[u8]) -> Option<GitObjectOrdinal> {
    if bytes.len() != ORDINAL_KEY_LEN || bytes[0] != ORDINAL_METADATA_FAMILY {
        return None;
    }
    Some(u32::from_be_bytes(array(bytes, 1)?))
}

pub(crate) fn decode_pack_key(bytes: &[u8]) -> Option<u64> {
    if bytes.len() != PACK_KEY_LEN || bytes[0] != PACK_FAMILY {
        return None;
    }
    let pack_slot = u64::from_be_bytes(array(bytes, 1)?);
    (pack_slot != 0).then_some(pack_slot)
}

pub(crate) fn pack_object_prefix(pack_slot: u64) -> Option<[u8; PACK_OBJECT_PREFIX_LEN]> {
    if pack_slot == 0 {
        return None;
    }
    let mut key = [0; PACK_OBJECT_PREFIX_LEN];
    key[0] = PACK_OBJECT_FAMILY;
    key[1..].copy_from_slice(&pack_slot.to_be_bytes());
    Some(key)
}

pub(crate) fn pack_object_key(pack_slot: u64, oid: &[u8; 20]) -> Option<[u8; PACK_OBJECT_KEY_LEN]> {
    let prefix = pack_object_prefix(pack_slot)?;
    let mut key = [0; PACK_OBJECT_KEY_LEN];
    key[..PACK_OBJECT_PREFIX_LEN].copy_from_slice(&prefix);
    key[PACK_OBJECT_PREFIX_LEN..].copy_from_slice(oid);
    Some(key)
}

pub(crate) fn decode_pack_object_key(bytes: &[u8]) -> Option<(u64, [u8; 20])> {
    if bytes.len() != PACK_OBJECT_KEY_LEN || bytes[0] != PACK_OBJECT_FAMILY {
        return None;
    }
    let pack_slot = u64::from_be_bytes(array(bytes, 1)?);
    if pack_slot == 0 {
        return None;
    }
    Some((pack_slot, array(bytes, PACK_OBJECT_PREFIX_LEN)?))
}

pub(crate) fn encode_pack_object_ordinal(ordinal: GitObjectOrdinal) -> [u8; PACK_OBJECT_VALUE_LEN] {
    ordinal.to_be_bytes()
}

pub(crate) fn decode_pack_object_ordinal(bytes: &[u8]) -> Option<GitObjectOrdinal> {
    if bytes.len() != PACK_OBJECT_VALUE_LEN {
        return None;
    }
    Some(u32::from_be_bytes(array(bytes, 0)?))
}

pub(crate) fn encode_object_location(location: StoredObjectLocation) -> [u8; OBJECT_VALUE_LEN] {
    let mut bytes = [0; OBJECT_VALUE_LEN];
    bytes[..4].copy_from_slice(&location.ordinal.to_be_bytes());
    bytes[4..12].copy_from_slice(&location.pack_slot.to_be_bytes());
    bytes[12..20].copy_from_slice(&location.pack_offset.to_be_bytes());
    bytes[20..28].copy_from_slice(&location.entry_len.to_be_bytes());
    bytes[28..32].copy_from_slice(&location.crc32.to_be_bytes());
    bytes[32..].copy_from_slice(&encode_object_metadata(location.metadata));
    bytes
}

pub(crate) fn encode_object_metadata(
    metadata: GitObjectMetadata,
) -> [u8; ORDINAL_METADATA_VALUE_LEN] {
    let mut bytes = [0; ORDINAL_METADATA_VALUE_LEN];
    let mut flags = 0_u8;
    if metadata.kind.is_some() {
        flags |= 1;
    }
    if metadata.logical_size.is_some() {
        flags |= 1 << 1;
    }
    if metadata.delta_base_oid.is_some() {
        flags |= 1 << 2;
    }
    bytes[0] = flags;
    bytes[1] = match metadata.kind {
        None => 0,
        Some(GitObjectKind::Commit) => 1,
        Some(GitObjectKind::Tree) => 2,
        Some(GitObjectKind::Blob) => 3,
        Some(GitObjectKind::Tag) => 4,
    };
    bytes[2..10].copy_from_slice(&metadata.logical_size.unwrap_or_default().to_be_bytes());
    if let Some(base) = metadata.delta_base_oid {
        bytes[10..30].copy_from_slice(&base);
    }
    bytes
}

pub(crate) fn decode_object_metadata(bytes: &[u8]) -> Option<GitObjectMetadata> {
    if bytes.len() != ORDINAL_METADATA_VALUE_LEN {
        return None;
    }
    let flags = bytes[0];
    if flags & !0b111 != 0 || bytes[30..].iter().any(|byte| *byte != 0) {
        return None;
    }
    let kind = match (flags & 1 != 0, bytes[1]) {
        (false, 0) => None,
        (true, 1) => Some(GitObjectKind::Commit),
        (true, 2) => Some(GitObjectKind::Tree),
        (true, 3) => Some(GitObjectKind::Blob),
        (true, 4) => Some(GitObjectKind::Tag),
        _ => return None,
    };
    let logical_size = if flags & (1 << 1) != 0 {
        Some(u64::from_be_bytes(array(bytes, 2)?))
    } else if bytes[2..10].iter().all(|byte| *byte == 0) {
        None
    } else {
        return None;
    };
    let delta_base_oid = if flags & (1 << 2) != 0 {
        Some(array(bytes, 10)?)
    } else if bytes[10..30].iter().all(|byte| *byte == 0) {
        None
    } else {
        return None;
    };
    Some(GitObjectMetadata {
        kind,
        logical_size,
        delta_base_oid,
    })
}

pub(crate) fn decode_object_location(bytes: &[u8]) -> Option<StoredObjectLocation> {
    if bytes.len() != OBJECT_VALUE_LEN {
        return None;
    }
    let metadata = decode_object_metadata(&bytes[32..])?;
    let location = StoredObjectLocation {
        ordinal: u32::from_be_bytes(array(bytes, 0)?),
        pack_slot: u64::from_be_bytes(array(bytes, 4)?),
        pack_offset: u64::from_be_bytes(array(bytes, 12)?),
        entry_len: u64::from_be_bytes(array(bytes, 20)?),
        crc32: u32::from_be_bytes(array(bytes, 28)?),
        metadata,
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
    bytes[16..24].copy_from_slice(&metadata.next_object_ordinal.to_be_bytes());
    if let Some(identity) = metadata.identity {
        bytes[24] = 1;
        bytes[25..33].copy_from_slice(&identity.generation.to_be_bytes());
        bytes[33..65].copy_from_slice(&<[u8; 32]>::from(identity.pack_index_hash));
        bytes[65..69].copy_from_slice(
            &GitObjectOrdinal::try_from(identity.object_count)
                .unwrap_or(GitObjectOrdinal::MAX)
                .to_be_bytes(),
        );
        bytes[69..].copy_from_slice(&<[u8; 32]>::from(identity.catalog_digest));
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
    let next_object_ordinal = u64::from_be_bytes(array(bytes, 16)?);
    if next_object_ordinal > u64::from(GitObjectOrdinal::MAX) {
        return None;
    }
    let identity = match bytes[24] {
        0 if bytes[25..].iter().all(|byte| *byte == 0) => None,
        1 => {
            let generation = u64::from_be_bytes(array(bytes, 25)?);
            let object_count = u64::from(u32::from_be_bytes(array(bytes, 65)?));
            if generation == 0 || object_count > next_object_ordinal {
                return None;
            }
            Some(GitObjectCatalogIdentity {
                generation,
                pack_index_hash: MerkleHash::from(array(bytes, 33)?),
                object_count,
                catalog_digest: MerkleHash::from(array(bytes, 69)?),
            })
        }
        _ => return None,
    };
    Some(LocatorMetadata {
        next_pack_slot,
        next_object_ordinal,
        identity,
    })
}

pub(crate) fn coverage(metadata: LocatorMetadata) -> Option<GitLocatorCoverage> {
    metadata.identity.map(|identity| GitLocatorCoverage {
        generation: identity.generation,
        pack_index_hash: identity.pack_index_hash,
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
    fn object_row_binds_one_dense_ordinal_to_one_exact_pack_range() {
        let oid = [0x11; 20];
        let location = StoredObjectLocation {
            ordinal: 0x0102_0304,
            pack_slot: 0x0102_0304_0506_0708,
            pack_offset: 0x1112_1314_1516_1718,
            entry_len: 0x2122_2324_2526_2728,
            crc32: 0x3132_3334,
            metadata: GitObjectMetadata {
                kind: Some(GitObjectKind::Blob),
                logical_size: Some(0x4142_4344_4546_4748),
                delta_base_oid: Some([0x51; 20]),
            },
        };

        let key = object_key(&oid);
        let value = encode_object_location(location);

        assert_eq!(key.len(), 21);
        assert_eq!(key[0], OBJECT_FAMILY);
        assert_eq!(&key[1..], &oid);
        assert_eq!(value.len(), 64);
        assert_eq!(&value[..4], &location.ordinal.to_be_bytes());
        assert_eq!(&value[4..12], &location.pack_slot.to_be_bytes());
        assert_eq!(&value[12..20], &location.pack_offset.to_be_bytes());
        assert_eq!(&value[20..28], &location.entry_len.to_be_bytes());
        assert_eq!(&value[28..32], &location.crc32.to_be_bytes());
        assert_eq!(value[32], 0b111);
        assert_eq!(value[33], 3);
        assert_eq!(
            &value[34..42],
            &location
                .metadata
                .logical_size
                .expect("logical size")
                .to_be_bytes()
        );
        assert_eq!(&value[42..62], &[0x51; 20]);
        assert_eq!(&value[62..], &[0, 0]);
        assert_eq!(decode_object_key(&key), Some(oid));
        assert_eq!(decode_object_location(&value), Some(location));
        assert_eq!(
            decode_ordinal_key(&ordinal_key(location.ordinal)),
            Some(location.ordinal)
        );
    }

    #[test]
    fn ordinal_metadata_row_round_trips_without_reinterpreting_ordinal_rows() {
        let metadata = GitObjectMetadata {
            kind: Some(GitObjectKind::Tag),
            logical_size: Some(0x0102_0304_0506_0708),
            delta_base_oid: Some([0x51; 20]),
        };
        let key = ordinal_metadata_key(0x0102_0304);
        let value = encode_object_metadata(metadata);

        assert_eq!(key[0], ORDINAL_METADATA_FAMILY);
        assert_eq!(&key[1..], &0x0102_0304_u32.to_be_bytes());
        assert_eq!(value.len(), ORDINAL_METADATA_VALUE_LEN);
        assert_eq!(value[0], 0b111);
        assert_eq!(value[1], 4);
        assert_eq!(&value[2..10], &0x0102_0304_0506_0708_u64.to_be_bytes());
        assert_eq!(&value[10..30], &[0x51; 20]);
        assert_eq!(decode_ordinal_metadata_key(&key), Some(0x0102_0304));
        assert_eq!(decode_object_metadata(&value), Some(metadata));
        assert_eq!(
            decode_object_metadata(&[0; ORDINAL_METADATA_VALUE_LEN]),
            Some(Default::default())
        );
        assert_eq!(ordinal_key(0x0102_0304)[0], ORDINAL_FAMILY);
    }

    #[test]
    fn pack_membership_key_binds_one_oid_to_one_pack_slot_and_ordinal() {
        let oid = [0x61; 20];
        let pack_slot = 0x0102_0304_0506_0708;
        let ordinal = 0x0102_0304;
        let key = pack_object_key(pack_slot, &oid).expect("valid pack slot");

        assert_eq!(key.len(), PACK_OBJECT_KEY_LEN);
        assert_eq!(key[0], PACK_OBJECT_FAMILY);
        assert_eq!(decode_pack_object_key(&key), Some((pack_slot, oid)));
        assert_eq!(
            decode_pack_object_ordinal(&encode_pack_object_ordinal(ordinal)),
            Some(ordinal)
        );
        assert_eq!(pack_object_key(0, &oid), None);
        assert_eq!(decode_pack_object_key(&PACK_OBJECT_INDEX_MARKER_KEY), None);
        assert_eq!(PACK_OBJECT_INDEX_REBUILDING_VALUE, [0]);
        assert_eq!(PACK_OBJECT_INDEX_MARKER_VALUE, [1]);
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
        bytes[24] = 2;
        assert_eq!(decode_metadata(&bytes), None);
    }

    #[test]
    fn metadata_requires_zero_absent_coverage_and_nonzero_present_generation() {
        let mut absent = encode_metadata(LocatorMetadata::empty());
        absent[69] = 1;
        assert_eq!(decode_metadata(&absent), None);

        let present = LocatorMetadata {
            next_pack_slot: 9,
            next_object_ordinal: 4,
            identity: Some(GitObjectCatalogIdentity {
                generation: 7,
                pack_index_hash: hash(3),
                object_count: 4,
                catalog_digest: hash(5),
            }),
        };
        assert_eq!(decode_metadata(&encode_metadata(present)), Some(present));

        let mut zero_generation = encode_metadata(present);
        zero_generation[25..33].fill(0);
        assert_eq!(decode_metadata(&zero_generation), None);

        let mut mismatched_count = encode_metadata(present);
        mismatched_count[65..69].copy_from_slice(&5_u32.to_be_bytes());
        assert_eq!(decode_metadata(&mismatched_count), None);
    }

    #[test]
    fn compact_decoders_reject_wrong_lengths_zero_slots_and_invalid_ranges() {
        assert_eq!(decode_object_key(&[OBJECT_FAMILY; 20]), None);
        assert_eq!(decode_pack_key(&[PACK_FAMILY; 8]), None);
        assert_eq!(pack_key(0), None);

        let mut object = encode_object_location(StoredObjectLocation {
            ordinal: 0,
            pack_slot: 1,
            pack_offset: 12,
            entry_len: 20,
            crc32: 4,
            metadata: GitObjectMetadata::default(),
        });
        object[4..12].fill(0);
        assert_eq!(decode_object_location(&object), None);
        object[4..12].copy_from_slice(&1_u64.to_be_bytes());
        object[20..28].fill(0);
        assert_eq!(decode_object_location(&object), None);
        object[12..20].copy_from_slice(&u64::MAX.to_be_bytes());
        object[20..28].copy_from_slice(&1_u64.to_be_bytes());
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
