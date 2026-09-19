use std::collections::BTreeMap;

use super::{
    CHILD_FIXED_BYTES, LAYER_HEADER_BYTES, LAYER_MAGIC, LAYER_VERSION, MAX_AUTHOR_BYTES,
    MAX_CHILDREN_PER_NODE, MAX_MESSAGE_BYTES, MAX_PATH_BYTES, NODE_FIXED_BYTES, PathStateLayer,
    PathStateLayerRef, PathStateNode, PathStateNodeRef, PathStateRecord, RECORD_FIXED_BYTES,
    Result, corrupt_at, corruption,
};
use crate::error::MetadataError;

pub(super) fn encode_layer(layer: &PathStateLayer) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(LAYER_MAGIC);
    bytes.extend_from_slice(&LAYER_VERSION.to_le_bytes());
    bytes.extend_from_slice(&layer.base_ordinal.to_le_bytes());
    bytes.extend_from_slice(&(layer.records.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(layer.nodes.len() as u32).to_le_bytes());
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
            bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&child.layer.to_le_bytes());
            bytes.extend_from_slice(&child.index.to_le_bytes());
            bytes.extend_from_slice(name);
        }
    }
    Ok(bytes)
}

pub(super) fn decode_layer(
    bytes: &[u8],
    reference: &PathStateLayerRef,
    path: &str,
) -> Result<PathStateLayer> {
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
        if child_count > MAX_CHILDREN_PER_NODE {
            return corrupt_at(path, "path-state node has too many children");
        }
        let mut children = BTreeMap::new();
        for _ in 0..child_count {
            if bytes.len().saturating_sub(cursor) < CHILD_FIXED_BYTES {
                return corrupt_at(path, "truncated path-state child");
            }
            let name_len = read_u32(bytes, &mut cursor, path)? as usize;
            if name_len == 0 || name_len > MAX_PATH_BYTES {
                return corrupt_at(path, "path-state child name exceeds its limits");
            }
            let child = PathStateNodeRef {
                layer: read_u32(bytes, &mut cursor, path)?,
                index: read_u32(bytes, &mut cursor, path)?,
            };
            let name = read_vec(bytes, &mut cursor, name_len, path)?;
            if children.insert(name, child).is_some() {
                return corrupt_at(path, "path-state node has a duplicate child");
            }
        }
        nodes.push(PathStateNode { value, children });
    }
    if cursor != bytes.len() {
        return corrupt_at(path, "path-state layer has trailing bytes");
    }
    Ok(PathStateLayer {
        base_ordinal,
        records,
        nodes,
    })
}

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

fn read_u32(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<u32> {
    Ok(u32::from_le_bytes(read_array(bytes, cursor, path)?))
}

fn read_i64(bytes: &[u8], cursor: &mut usize, path: &str) -> Result<i64> {
    Ok(i64::from_le_bytes(read_array(bytes, cursor, path)?))
}

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
