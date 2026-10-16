use std::collections::BTreeMap;
#[cfg(any(feature = "storage", test))]
use std::sync::Arc;

use super::PathStateLayerPages;
use super::{
    MAX_AUTHOR_BYTES, MAX_MESSAGE_BYTES, MAX_PATH_BYTES, PathStateLayer, Result, corruption,
};
#[cfg(any(feature = "storage", test))]
use super::{PathStateLayerRef, PathStateNode, PathStateNodeRef, PathStateRecord, corrupt_at};
#[cfg(any(feature = "storage", test))]
use crate::pages::Input;
use crate::pages::{self as paged, Kind, Writer};

pub(super) struct EncodedLayer {
    pub(super) bytes: Vec<u8>,
    pub(super) pages: PathStateLayerPages,
}

pub(super) fn encode_layer(layer: &PathStateLayer) -> Result<EncodedLayer> {
    let mut names = BTreeMap::<&[u8], u32>::new();
    for node in &layer.nodes {
        for name in node.children.keys() {
            names.entry(name.as_ref()).or_default();
        }
    }
    let mut name_rows = BTreeMap::new();
    for (index, (name, ordinal)) in names.iter_mut().enumerate() {
        validate_name(name)?;
        *ordinal = u32::try_from(index).map_err(|_| corruption("component ordinal overflows"))?;
        let mut value = ordinal.to_le_bytes().to_vec();
        value.extend_from_slice(name);
        if name_rows
            .insert(blake3::hash(name).as_bytes().to_vec(), value)
            .is_some()
        {
            return Err(corruption("component identities collide"));
        }
    }
    let mut writer = Writer::default();
    let names_root = if name_rows.is_empty() {
        None
    } else {
        Some(writer.tree(Kind::Names, name_rows.into_iter().collect())?)
    };
    let mut nodes = Vec::new();
    for (index, node) in layer.nodes.iter().enumerate() {
        let rows = node
            .children
            .iter()
            .map(|(name, child)| {
                let ordinal = names
                    .get(name.as_ref())
                    .ok_or_else(|| corruption("missing component ordinal"))?;
                let mut value = child.layer.to_le_bytes().to_vec();
                value.extend_from_slice(&child.index.to_le_bytes());
                Ok((ordinal.to_be_bytes().to_vec(), value))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut value = node.value.unwrap_or(u32::MAX).to_le_bytes().to_vec();
        value.extend_from_slice(&node.reset.unwrap_or(u32::MAX).to_le_bytes());
        if rows.is_empty() {
            value.push(0);
        } else {
            value.push(1);
            let root = writer.tree(Kind::Children, rows)?;
            paged::encode_ref(&mut value, &root);
        }
        let key = u32::try_from(index)
            .map_err(|_| corruption("node ordinal overflows"))?
            .to_be_bytes()
            .to_vec();
        nodes.push((key, value));
    }
    let nodes_root = writer.tree(Kind::Nodes, nodes)?;
    let records = layer
        .records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            if record.author.len() > MAX_AUTHOR_BYTES || record.message.len() > MAX_MESSAGE_BYTES {
                return Err(corruption("commit summary exceeds its limits"));
            }
            let mut value = record.oid.to_vec();
            value.extend_from_slice(&record.first_parent.unwrap_or(u32::MAX).to_le_bytes());
            value.extend_from_slice(&record.author_seconds.to_le_bytes());
            paged::put_u32(&mut value, record.author.len())?;
            paged::put_u32(&mut value, record.message.len())?;
            value.extend_from_slice(&record.root.layer.to_le_bytes());
            value.extend_from_slice(&record.root.index.to_le_bytes());
            value.extend_from_slice(&record.author);
            value.extend_from_slice(&record.message);
            let key = u32::try_from(index)
                .map_err(|_| corruption("record ordinal overflows"))?
                .to_be_bytes()
                .to_vec();
            Ok((key, value))
        })
        .collect::<Result<Vec<_>>>()?;
    let records_root = writer.tree(Kind::Records, records)?;
    Ok(EncodedLayer {
        bytes: writer.bytes,
        pages: PathStateLayerPages {
            records: records_root,
            nodes: nodes_root,
            names: names_root,
        },
    })
}

fn validate_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name.len() > MAX_PATH_BYTES || name.contains(&0) || name.contains(&b'/') {
        return Err(corruption("invalid path-state component"));
    }
    Ok(())
}

#[cfg(any(feature = "storage", test))]
pub(super) fn decode_layer(
    bytes: &[u8],
    reference: &PathStateLayerRef,
    path: &str,
) -> Result<PathStateLayer> {
    if bytes.len() as u64 != reference.bytes
        || blake3::hash(bytes).to_hex().as_str() != reference.hash
    {
        return corrupt_at(path, "layer content identity mismatch");
    }
    let mut names = Vec::new();
    let mut visited = BTreeMap::new();
    if let Some(root) = &reference.pages.names {
        let rows = paged::scan(bytes, root, Kind::Names, path, &mut visited)?;
        let mut ordered = BTreeMap::new();
        for (key, value) in rows {
            let (ordinal, name) = decode_name(&key, &value, path)?;
            if ordered.insert(ordinal, Arc::<[u8]>::from(name)).is_some() {
                return corrupt_at(path, "duplicate component ordinal");
            }
        }
        for (index, (ordinal, name)) in ordered.into_iter().enumerate() {
            if ordinal as usize != index
                || names
                    .last()
                    .is_some_and(|previous: &Arc<[u8]>| previous.as_ref() >= name.as_ref())
            {
                return corrupt_at(path, "component ordinals are incomplete or unordered");
            }
            names.push(name);
        }
    }
    let records = paged::scan(
        bytes,
        &reference.pages.records,
        Kind::Records,
        path,
        &mut visited,
    )?
    .into_iter()
    .map(|(_, value)| decode_record(&value, path))
    .collect::<Result<Vec<_>>>()?;
    let mut used = vec![false; names.len()];
    let mut nodes = Vec::new();
    for (_, value) in paged::scan(
        bytes,
        &reference.pages.nodes,
        Kind::Nodes,
        path,
        &mut visited,
    )? {
        let (value, reset, children) = decode_node(&value, reference.bytes, path)?;
        let mut entries = BTreeMap::new();
        if let Some(root) = children {
            for (key, value) in paged::scan(bytes, &root, Kind::Children, path, &mut visited)? {
                let ordinal = u32::from_be_bytes(
                    key.as_slice()
                        .try_into()
                        .map_err(|_| corruption("invalid component key"))?,
                ) as usize;
                let name = names
                    .get(ordinal)
                    .ok_or_else(|| corruption("component ordinal is out of bounds"))?;
                used[ordinal] = true;
                let child = decode_child(&value, path)?;
                entries.insert(Arc::clone(name), child);
            }
        }
        nodes.push(PathStateNode {
            value,
            reset,
            children: entries,
        });
    }
    if used.contains(&false) {
        return corrupt_at(path, "unused path-state component");
    }
    if records.len() != reference.commit_count as usize
        || nodes.len() != reference.node_count as usize
    {
        return corrupt_at(path, "layer count does not match its descriptor");
    }
    let mut offset = 0u64;
    for (start, (_, page)) in visited {
        if start != offset {
            return corrupt_at(path, "layer pages overlap or leave unused bytes");
        }
        offset = offset
            .checked_add(u64::from(page.bytes))
            .ok_or_else(|| corruption("layer coverage overflows"))?;
    }
    if offset != reference.bytes {
        return corrupt_at(path, "layer contains trailing unreferenced bytes");
    }
    Ok(PathStateLayer {
        base_ordinal: reference.base_ordinal,
        records,
        nodes,
    })
}

#[cfg(any(feature = "storage", test))]
pub(super) fn decode_record(bytes: &[u8], path: &str) -> Result<PathStateRecord> {
    let mut input = Input::new(bytes, path);
    let oid = input.array()?;
    let first_parent = optional(input.u32()?);
    let author_seconds = i64::from_le_bytes(input.array()?);
    let author_len = input.u32()? as usize;
    let message_len = input.u32()? as usize;
    let root = PathStateNodeRef {
        layer: input.u32()?,
        index: input.u32()?,
    };
    if author_len > MAX_AUTHOR_BYTES || message_len > MAX_MESSAGE_BYTES {
        return corrupt_at(path, "summary exceeds its byte limit");
    }
    let author = input.take(author_len)?.to_vec();
    let message = input.take(message_len)?.to_vec();
    input.finish()?;
    Ok(PathStateRecord {
        oid,
        first_parent,
        author_seconds,
        author,
        message,
        root,
    })
}

#[cfg(any(feature = "storage", test))]
pub(super) fn decode_node(
    bytes: &[u8],
    source_bytes: u64,
    path: &str,
) -> Result<(Option<u32>, Option<u32>, Option<paged::PageRef>)> {
    let mut input = Input::new(bytes, path);
    let value = optional(input.u32()?);
    let reset = optional(input.u32()?);
    let children = match input.array::<1>()?[0] {
        0 => None,
        1 => {
            let root = paged::decode_ref(&mut input, Kind::Children)?;
            root.validate(Kind::Children, source_bytes)?;
            if root.count as usize > super::MAX_CHILDREN_PER_NODE {
                return corrupt_at(path, "node has too many children");
            }
            Some(root)
        }
        _ => return corrupt_at(path, "invalid child-root tag"),
    };
    input.finish()?;
    Ok((value, reset, children))
}

#[cfg(any(feature = "storage", test))]
pub(super) fn decode_name<'a>(key: &[u8], bytes: &'a [u8], path: &str) -> Result<(u32, &'a [u8])> {
    let mut input = Input::new(bytes, path);
    let ordinal = input.u32()?;
    let name = input.take(input.remaining())?;
    validate_name(name)?;
    if blake3::hash(name).as_bytes().as_slice() != key {
        return corrupt_at(path, "component identity mismatch");
    }
    Ok((ordinal, name))
}

#[cfg(any(feature = "storage", test))]
pub(super) fn decode_child(bytes: &[u8], path: &str) -> Result<PathStateNodeRef> {
    let mut input = Input::new(bytes, path);
    let child = PathStateNodeRef {
        layer: input.u32()?,
        index: input.u32()?,
    };
    input.finish()?;
    Ok(child)
}

#[cfg(any(feature = "storage", test))]
fn optional(value: u32) -> Option<u32> {
    (value != u32::MAX).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathStateLayer {
        let name: Arc<[u8]> = Arc::from(b"repeated-component-\xff".as_slice());
        let mut nodes = vec![PathStateNode {
            value: Some(0),
            reset: None,
            children: BTreeMap::new(),
        }];
        nodes.extend((0..1024).map(|_| PathStateNode {
            value: None,
            reset: None,
            children: BTreeMap::from([(name.clone(), PathStateNodeRef { layer: 0, index: 0 })]),
        }));
        PathStateLayer {
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
        }
    }

    fn reference(encoded: &EncodedLayer, layer: &PathStateLayer) -> PathStateLayerRef {
        PathStateLayerRef {
            hash: blake3::hash(&encoded.bytes).to_hex().to_string(),
            path: "layer".to_owned(),
            base_ordinal: 0,
            commit_count: layer.records.len() as u32,
            node_count: layer.nodes.len() as u32,
            bytes: encoded.bytes.len() as u64,
            pages: encoded.pages.clone(),
        }
    }

    #[test]
    fn repeated_raw_components_remain_shared_and_encoded_once() {
        let layer = fixture();
        let encoded = encode_layer(&layer).unwrap();
        let decoded = decode_layer(&encoded.bytes, &reference(&encoded, &layer), "layer").unwrap();
        let name = layer.nodes[1].children.keys().next().unwrap();
        assert_eq!(
            (
                decoded == layer,
                encoded
                    .bytes
                    .windows(name.len())
                    .filter(|bytes| *bytes == name.as_ref())
                    .count(),
                Arc::ptr_eq(
                    decoded.nodes[1].children.keys().next().unwrap(),
                    decoded.nodes[1024].children.keys().next().unwrap()
                )
            ),
            (true, 1, true)
        );
    }

    #[test]
    fn full_validation_rejects_trailing_unreferenced_bytes() {
        let layer = fixture();
        let mut encoded = encode_layer(&layer).unwrap();
        encoded.bytes.extend_from_slice(b"unreferenced");
        let reference = reference(&encoded, &layer);
        assert!(decode_layer(&encoded.bytes, &reference, "layer").is_err());
    }

    #[test]
    fn stored_layers_reject_corruption_and_truncation() {
        let layer = fixture();
        let encoded = encode_layer(&layer).unwrap();
        let reference = reference(&encoded, &layer);
        for cut in [0, 13, encoded.bytes.len() - 1] {
            assert!(decode_layer(&encoded.bytes[..cut], &reference, "layer").is_err());
        }
        let mut damaged = encoded.bytes;
        damaged[20] ^= 1;
        assert!(decode_layer(&damaged, &reference, "layer").is_err());
    }
}
