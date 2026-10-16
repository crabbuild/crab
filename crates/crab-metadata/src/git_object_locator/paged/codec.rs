use super::*;
use crate::git_object_locator::GitObjectMetadata;

pub(super) struct Placement {
    pub(super) slot: u32,
    pub(super) offset: u64,
    pub(super) logical: GitObjectMetadata,
}

pub(super) fn encode_pointer(slot: u32, locator: GitObjectLocator) -> Vec<u8> {
    let mut value = offset_key(slot, locator.location.pack_offset);
    let mut logical = locator.metadata;
    logical.delta_base_oid = None;
    value.extend_from_slice(&encode_metadata(logical));
    value
}

pub(super) fn decode_pointer(bytes: &[u8]) -> Result<Placement> {
    let logical = decode_metadata(
        bytes
            .get(12..)
            .ok_or_else(|| corruption("truncated object pointer"))?,
    )?;
    if logical.delta_base_oid.is_some() {
        return bad("object pointer contains encoding-specific base facts");
    }
    Ok(Placement {
        slot: u32::from_be_bytes(array(bytes, 0)?),
        offset: u64::from_be_bytes(array(bytes, 4)?),
        logical,
    })
}

pub(super) fn offset_key(slot: u32, offset: u64) -> Vec<u8> {
    let mut key = slot.to_be_bytes().to_vec();
    key.extend_from_slice(&offset.to_be_bytes());
    key
}

pub(super) fn encode_location(location: GitObjectLocation, metadata: GitObjectMetadata) -> Vec<u8> {
    let mut value = location.entry_len.to_be_bytes().to_vec();
    value.extend_from_slice(&location.crc32.to_be_bytes());
    value.extend_from_slice(&encode_metadata(metadata));
    value
}

pub(super) fn decode_location(
    offset: u64,
    bytes: &[u8],
) -> Result<(GitObjectLocation, GitObjectMetadata)> {
    let metadata = decode_metadata(
        bytes
            .get(12..)
            .ok_or_else(|| corruption("truncated physical location"))?,
    )?;
    Ok((
        GitObjectLocation {
            pack_offset: offset,
            entry_len: u64::from_be_bytes(array(bytes, 0)?),
            crc32: u32::from_be_bytes(array(bytes, 8)?),
        },
        metadata,
    ))
}

fn encode_metadata(metadata: GitObjectMetadata) -> Vec<u8> {
    let full = format::encode_object_metadata(metadata);
    let mut value = vec![full[0]];
    for (flag, range) in [(1, 1..2), (2, 2..10), (4, 10..30)] {
        if full[0] & flag != 0 {
            value.extend_from_slice(&full[range]);
        }
    }
    value
}

fn decode_metadata(bytes: &[u8]) -> Result<GitObjectMetadata> {
    let flags = *bytes
        .first()
        .ok_or_else(|| corruption("missing metadata flags"))?;
    let mut full = [0; 32];
    full[0] = flags;
    let mut position = 1;
    for (flag, range) in [(1, 1..2), (2, 2..10), (4, 10..30)] {
        if flags & flag == 0 {
            continue;
        }
        let end = position + range.len();
        full[range].copy_from_slice(
            bytes
                .get(position..end)
                .ok_or_else(|| corruption("truncated metadata fields"))?,
        );
        position = end;
    }
    if position != bytes.len() {
        return bad("metadata has trailing bytes");
    }
    format::decode_object_metadata(&full).ok_or_else(|| corruption("invalid object metadata"))
}

pub(super) fn encode_pack(pack: GitPackInventoryEntry) -> Vec<u8> {
    let mut value = <[u8; 32]>::from(pack.pack_id).to_vec();
    value.extend_from_slice(&pack.object_count.to_be_bytes());
    value.extend_from_slice(&pack.pack_size.to_be_bytes());
    value
}

pub(super) fn decode_pack(bytes: &[u8]) -> Result<GitPackInventoryEntry> {
    if bytes.len() != 48 {
        return bad("invalid pack-slot row length");
    }
    Ok(GitPackInventoryEntry {
        pack_id: MerkleHash::from(array(bytes, 0)?),
        object_count: u64::from_be_bytes(array(bytes, 32)?),
        pack_size: u64::from_be_bytes(array(bytes, 40)?),
    })
}

pub(super) fn encode_binding(slot: u32, pack: GitPackInventoryEntry) -> Vec<u8> {
    let mut value = slot.to_be_bytes().to_vec();
    value.extend_from_slice(&pack.object_count.to_be_bytes());
    value.extend_from_slice(&pack.pack_size.to_be_bytes());
    value
}

pub(super) fn decode_binding(
    pack_id: MerkleHash,
    bytes: &[u8],
) -> Result<(u32, GitPackInventoryEntry)> {
    if bytes.len() != 20 {
        return bad("invalid pack binding row length");
    }
    Ok((
        u32::from_be_bytes(array(bytes, 0)?),
        GitPackInventoryEntry {
            pack_id,
            object_count: u64::from_be_bytes(array(bytes, 4)?),
            pack_size: u64::from_be_bytes(array(bytes, 12)?),
        },
    ))
}

pub(super) fn array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N]> {
    offset
        .checked_add(N)
        .and_then(|end| bytes.get(offset..end))
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| corruption("truncated object index value"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_object_locator::GitObjectKind;

    proptest::proptest! {
        #[test]
        fn compact_metadata_preserves_presence_and_extreme_values(
            kind in 0usize..5,
            size in proptest::option::of(proptest::num::u64::ANY),
            base in proptest::option::of(proptest::array::uniform20(proptest::num::u8::ANY)),
        ) {
            let kinds = [None, Some(GitObjectKind::Commit), Some(GitObjectKind::Tree), Some(GitObjectKind::Blob), Some(GitObjectKind::Tag)];
            let expected = GitObjectMetadata { kind: kinds[kind], logical_size: size, delta_base_oid: base };
            proptest::prop_assert_eq!(decode_metadata(&encode_metadata(expected)).unwrap(), expected);
        }
    }

    #[test]
    fn compact_metadata_rejects_truncation_reserved_flags_and_trailing_data() {
        let metadata = GitObjectMetadata {
            kind: Some(GitObjectKind::Blob),
            logical_size: Some(0),
            delta_base_oid: Some([0; 20]),
        };
        let bytes = encode_metadata(metadata);
        for end in 0..bytes.len() {
            assert!(decode_metadata(&bytes[..end]).is_err());
        }
        for invalid in [vec![8], vec![1, 0], vec![0, 0]] {
            assert!(decode_metadata(&invalid).is_err());
        }
        assert_eq!(decode_metadata(&bytes).unwrap(), metadata);
    }
}
