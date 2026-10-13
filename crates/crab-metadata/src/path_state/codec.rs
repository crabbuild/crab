use std::collections::BTreeMap;
#[cfg(any(feature = "storage", test))]
use std::sync::Arc;

#[cfg(any(feature = "storage", test))]
use super::{
    LAYER_HEADER_BYTES, MAX_AUTHOR_BYTES, MAX_CHILDREN_PER_NODE, MAX_MESSAGE_BYTES, MAX_PATH_BYTES,
    PathStateLayerRef, PathStateNode, PathStateNodeRef, PathStateRecord, corrupt_at, corruption,
};
use super::{LAYER_MAGIC, LAYER_VERSION, PathStateLayer, Result};
#[cfg(any(feature = "storage", test))]
use crate::error::MetadataError;

pub(super) fn encode_layer(layer: &PathStateLayer) -> Result<Vec<u8>> {
    let mut names = BTreeMap::<&[u8], u32>::new();
    for node in &layer.nodes {
        for name in node.children.keys() {
            names.entry(name.as_ref()).or_default();
        }
    }
    let name_count = u32::try_from(names.len())
        .map_err(|_| super::corruption("path-state component count overflows"))?;
    for (ordinal, value) in names.values_mut().enumerate() {
        *value = u32::try_from(ordinal)
            .map_err(|_| super::corruption("path-state component ordinal overflows"))?;
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(LAYER_MAGIC);
    bytes.extend_from_slice(&LAYER_VERSION.to_le_bytes());
    bytes.extend_from_slice(&layer.base_ordinal.to_le_bytes());
    bytes.extend_from_slice(&(layer.records.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(layer.nodes.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&name_count.to_le_bytes());
    for name in names.keys() {
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(name);
    }
    for record in &layer.records {
        bytes.extend_from_slice(&record.oid);
        bytes.extend_from_slice(&record.first_parent.unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(&record.author_seconds.to_le_bytes());
        bytes.extend_from_slice(&(record.author.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(record.message.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&record.root.layer.to_le_bytes());
        bytes.extend_from_slice(&record.root.index.to_le_bytes());
        bytes.extend_from_slice(&record.author);
        bytes.extend_from_slice(&record.message);
    }
    for node in &layer.nodes {
        bytes.extend_from_slice(&node.value.unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(&(node.children.len() as u32).to_le_bytes());
        for (name, child) in &node.children {
            let ordinal = names
                .get(name.as_ref())
                .ok_or_else(|| super::corruption("path-state component is missing"))?;
            bytes.extend_from_slice(&ordinal.to_le_bytes());
            bytes.extend_from_slice(&child.layer.to_le_bytes());
            bytes.extend_from_slice(&child.index.to_le_bytes());
        }
    }
    Ok(bytes)
}

// Only storage loads encoded layers; payload builders keep their validated index.
// Unit tests still exercise the decoder without enabling storage dependencies.
#[cfg(any(feature = "storage", test))]
pub(super) fn decode_layer(
    bytes: &[u8],
    reference: &PathStateLayerRef,
    path: &str,
) -> Result<PathStateLayer> {
    const RECORD_FIXED_BYTES: usize = 48;
    const NODE_FIXED_BYTES: usize = 8;
    const CHILD_FIXED_BYTES: usize = 12;

    if bytes.len() < LAYER_HEADER_BYTES || &bytes[..8] != LAYER_MAGIC {
        return corrupt_at(path, "invalid path-state layer header");
    }
    let mut cursor = 8;
    if read_u32(bytes, &mut cursor, path)? != LAYER_VERSION {
        return corrupt_at(path, "unsupported path-state layer version");
    }
    let base_ordinal = read_u32(bytes, &mut cursor, path)?;
    let record_count = read_u32(bytes, &mut cursor, path)? as usize;
    let node_count = read_u32(bytes, &mut cursor, path)? as usize;
    if base_ordinal != reference.base_ordinal
        || record_count != reference.commit_count as usize
        || node_count != reference.node_count as usize
    {
        return corrupt_at(
            path,
            "path-state layer header does not match its descriptor",
        );
    }
    let name_count = read_u32(bytes, &mut cursor, path)? as usize;
    if name_count > bytes.len().saturating_sub(cursor) / 5 {
        return corrupt_at(path, "path-state component count exceeds the layer bytes");
    }
    let mut names = Vec::<Arc<[u8]>>::new();
    names
        .try_reserve_exact(name_count)
        .map_err(|_| corruption("path-state component allocation exceeds capacity"))?;
    for _ in 0..name_count {
        let length = read_u32(bytes, &mut cursor, path)? as usize;
        if length == 0 || length > MAX_PATH_BYTES {
            return corrupt_at(path, "path-state component exceeds its limits");
        }
        let name = read_vec(bytes, &mut cursor, length, path)?;
        if name.contains(&0)
            || name.contains(&b'/')
            || names
                .last()
                .is_some_and(|previous| previous.as_ref() >= name.as_slice())
        {
            return corrupt_at(path, "path-state component table is not canonical");
        }
        names.push(Arc::from(name));
    }
    let mut used_names = vec![false; name_count];
    if record_count > bytes.len().saturating_sub(cursor) / RECORD_FIXED_BYTES {
        return corrupt_at(path, "path-state record count exceeds the layer bytes");
    }
    let mut records = Vec::new();
    records
        .try_reserve_exact(record_count)
        .map_err(|_| corruption("path-state record allocation exceeds capacity"))?;
    for _ in 0..record_count {
        if bytes.len().saturating_sub(cursor) < RECORD_FIXED_BYTES {
            return corrupt_at(path, "truncated path-state record");
        }
        let oid = read_array(bytes, &mut cursor, path)?;
        let first_parent = match read_u32(bytes, &mut cursor, path)? {
            u32::MAX => None,
            parent => Some(parent),
        };
        let author_seconds = read_i64(bytes, &mut cursor, path)?;
        let author_len = read_u32(bytes, &mut cursor, path)? as usize;
        let message_len = read_u32(bytes, &mut cursor, path)? as usize;
        let root = PathStateNodeRef {
            layer: read_u32(bytes, &mut cursor, path)?,
            index: read_u32(bytes, &mut cursor, path)?,
        };
        if author_len > MAX_AUTHOR_BYTES || message_len > MAX_MESSAGE_BYTES {
            return corrupt_at(path, "path-state record exceeds its limits");
        }
        records.push(PathStateRecord {
            oid,
            first_parent,
            author: read_vec(bytes, &mut cursor, author_len, path)?,
            author_seconds,
            message: read_vec(bytes, &mut cursor, message_len, path)?,
            root,
        });
    }
    if node_count > bytes.len().saturating_sub(cursor) / NODE_FIXED_BYTES {
        return corrupt_at(path, "path-state node count exceeds the layer bytes");
    }
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(node_count)
        .map_err(|_| corruption("path-state node allocation exceeds capacity"))?;
    for _ in 0..node_count {
        if bytes.len().saturating_sub(cursor) < NODE_FIXED_BYTES {
            return corrupt_at(path, "truncated path-state node");
        }
        let value = match read_u32(bytes, &mut cursor, path)? {
            u32::MAX => None,
            value => Some(value),
        };
        let child_count = read_u32(bytes, &mut cursor, path)? as usize;
        if child_count > MAX_CHILDREN_PER_NODE
            || child_count > bytes.len().saturating_sub(cursor) / CHILD_FIXED_BYTES
        {
            return corrupt_at(path, "path-state node has too many children");
        }
        let mut children = BTreeMap::new();
        let mut previous_name = None;
        for _ in 0..child_count {
            if bytes.len().saturating_sub(cursor) < CHILD_FIXED_BYTES {
                return corrupt_at(path, "truncated path-state child");
            }
            let name_ordinal = read_u32(bytes, &mut cursor, path)? as usize;
            if previous_name.is_some_and(|previous| previous >= name_ordinal) {
                return corrupt_at(path, "path-state child names are not in canonical order");
            }
            let name = names
                .get(name_ordinal)
                .ok_or_else(|| corruption("path-state child component is out of bounds"))?;
            let used = used_names
                .get_mut(name_ordinal)
                .ok_or_else(|| corruption("path-state child component is out of bounds"))?;
            *used = true;
            previous_name = Some(name_ordinal);
            let child = PathStateNodeRef {
                layer: read_u32(bytes, &mut cursor, path)?,
                index: read_u32(bytes, &mut cursor, path)?,
            };
            children.insert(Arc::clone(name), child);
        }
        nodes.push(PathStateNode { value, children });
    }
    if used_names.contains(&false) {
        return corrupt_at(path, "path-state component table has an unused name");
    }
    if cursor != bytes.len() {
        return corrupt_at(path, "path-state layer has trailing bytes");
    }
    tracing::debug!(target: "crab_metadata::index_decode", index_format = "path_state",
        decoded_bytes = bytes.len() as u64, "index block decoded");
    Ok(PathStateLayer {
        base_ordinal,
        records,
        nodes,
    })
}

#[cfg(any(feature = "storage", test))]
fn read_vec(bytes: &[u8], cursor: &mut usize, length: usize, path: &str) -> Result<Vec<u8>> {
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| corruption("path-state offset overflows"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "truncated path-state bytes".to_owned(),
        })?;
    *cursor = end;
    Ok(value.to_vec())
}

#[cfg(any(feature = "storage", test))]
fn read_u32(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(bytes, cursor, path)?))
}

#[cfg(any(feature = "storage", test))]
fn read_i64(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<i64> {
    Ok(i64::from_le_bytes(read_array(bytes, cursor, path)?))
}

#[cfg(any(feature = "storage", test))]
fn read_array<const N: usize>(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<[u8; N]> {
    let end = cursor
        .checked_add(N)
        .ok_or_else(|| corruption("path-state offset overflows"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| MetadataError::CorruptObject {
            path: path.to_owned(),
            reason: "truncated path-state bytes".to_owned(),
        })?;
    *cursor = end;
    value.try_into().map_err(|_| MetadataError::CorruptObject {
        path: path.to_owned(),
        reason: "invalid path-state scalar".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_raw_components_are_encoded_once_per_layer() {
        let name: Arc<[u8]> = Arc::from(b"repeated-path-component-\xff".as_slice());
        let mut nodes = vec![PathStateNode {
            value: Some(0),
            children: BTreeMap::new(),
        }];
        nodes.extend((0..1024).map(|_| PathStateNode {
            value: None,
            children: BTreeMap::from([(name.clone(), PathStateNodeRef { layer: 0, index: 0 })]),
        }));
        let layer = PathStateLayer {
            base_ordinal: 0,
            records: vec![PathStateRecord {
                oid: [1; 20],
                first_parent: None,
                author_seconds: 1,
                author: Vec::new(),
                message: Vec::new(),
                root: PathStateNodeRef {
                    layer: 0,
                    index: 1024,
                },
            }],
            nodes,
        };
        let encoded = encode_layer(&layer).unwrap();
        let reference = PathStateLayerRef {
            hash: blake3::hash(&encoded).to_hex().to_string(),
            path: "layer".to_owned(),
            base_ordinal: 0,
            commit_count: 1,
            node_count: layer.nodes.len() as u32,
            bytes: encoded.len() as u64,
        };
        let decoded = decode_layer(&encoded, &reference, "layer").unwrap();
        let shared = Arc::ptr_eq(
            decoded.nodes[1].children.keys().next().unwrap(),
            decoded.nodes[1024].children.keys().next().unwrap(),
        );
        assert_eq!(
            (
                decoded == layer,
                encoded
                    .windows(name.len())
                    .filter(|window| *window == name.as_ref())
                    .count(),
                shared
            ),
            (true, 1, true)
        );
    }

    #[test]
    fn component_tables_reject_invalid_names_ordinals_and_truncated_layers() {
        let leaf = PathStateNode {
            value: Some(0),
            children: BTreeMap::new(),
        };
        let layer = PathStateLayer {
            base_ordinal: 0,
            records: vec![PathStateRecord {
                oid: [1; 20],
                first_parent: None,
                author_seconds: 1,
                author: Vec::new(),
                message: Vec::new(),
                root: PathStateNodeRef { layer: 0, index: 2 },
            }],
            nodes: vec![
                leaf.clone(),
                leaf,
                PathStateNode {
                    value: None,
                    children: BTreeMap::from([
                        (
                            Arc::from(b"a".as_slice()),
                            PathStateNodeRef { layer: 0, index: 0 },
                        ),
                        (
                            Arc::from(b"b".as_slice()),
                            PathStateNodeRef { layer: 0, index: 1 },
                        ),
                    ]),
                },
            ],
        };
        let encoded = encode_layer(&layer).unwrap();
        let reference = PathStateLayerRef {
            hash: blake3::hash(&encoded).to_hex().to_string(),
            path: "layer".to_owned(),
            base_ordinal: 0,
            commit_count: 1,
            node_count: 3,
            bytes: encoded.len() as u64,
        };
        let edges = LAYER_HEADER_BYTES + 2 * 5 + 48 + 3 * 8;
        for (name, offset, replacement) in [
            ("excessive table count", 24, u32::MAX.to_le_bytes().to_vec()),
            (
                "empty component",
                LAYER_HEADER_BYTES,
                0_u32.to_le_bytes().to_vec(),
            ),
            ("nul component", LAYER_HEADER_BYTES + 4, vec![0]),
            ("slash component", LAYER_HEADER_BYTES + 4, vec![b'/']),
            ("duplicate table name", LAYER_HEADER_BYTES + 9, vec![b'a']),
            (
                "invalid child ordinal",
                edges,
                u32::MAX.to_le_bytes().to_vec(),
            ),
            (
                "duplicate child ordinal",
                edges + 12,
                0_u32.to_le_bytes().to_vec(),
            ),
        ] {
            let mut invalid = encoded.clone();
            invalid[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert!(
                decode_layer(&invalid, &reference, "layer").is_err(),
                "{name}"
            );
        }
        for length in 0..encoded.len() {
            assert!(
                decode_layer(&encoded[..length], &reference, "layer").is_err(),
                "truncated at {length}"
            );
        }
        let mut unused = encoded.clone();
        unused[edges - 4..edges].copy_from_slice(&1_u32.to_le_bytes());
        unused.truncate(unused.len() - 12);
        assert!(decode_layer(&unused, &reference, "layer").is_err());
    }
}
