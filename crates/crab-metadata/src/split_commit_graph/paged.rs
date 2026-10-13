use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::*;
use crate::PageRef;
#[cfg(any(feature = "storage", test))]
use crate::pages::Input;
use crate::pages::{self, Kind, Writer};

/// Authenticated positional records and OID directory for one graph layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitGraphLayerPages {
    pub records: PageRef,
    pub oids: PageRef,
}

pub(super) fn encode_layer(layer: &CommitGraphLayer) -> Result<(Vec<u8>, CommitGraphLayerPages)> {
    let mut writer = Writer::default();
    let mut records = Vec::with_capacity(layer.records.len());
    let mut oids = BTreeMap::new();
    for (index, record) in layer.records.iter().enumerate() {
        let ordinal = layer
            .base_ordinal
            .checked_add(u32::try_from(index).map_err(|_| {
                crate::validation::corrupt_object("commit graph", "ordinal exceeds u32")
            })?)
            .ok_or_else(|| crate::validation::corrupt_object("commit graph", "ordinal overflow"))?;
        if record.parents.len() > MAX_PARENTS
            || record.parents.iter().any(|parent| *parent >= ordinal)
        {
            return corrupt("invalid paged graph parents");
        }
        let mut value = Vec::new();
        value.extend_from_slice(&record.oid);
        value.extend_from_slice(&record.tree_oid);
        value.extend_from_slice(&record.commit_time.to_le_bytes());
        value.extend_from_slice(&record.corrected_generation.to_le_bytes());
        pages::put_u32(&mut value, record.parents.len())?;
        for parent in &record.parents {
            value.extend_from_slice(&parent.to_le_bytes());
        }
        records.push((ordinal.to_be_bytes().to_vec(), value));
        if oids
            .insert(record.oid.to_vec(), ordinal.to_le_bytes().to_vec())
            .is_some()
        {
            return corrupt("duplicate paged graph OID");
        }
    }
    let records = writer.tree(Kind::CommitRecords, records)?;
    let oids = writer.tree(Kind::CommitOids, oids.into_iter().collect())?;
    Ok((writer.bytes, CommitGraphLayerPages { records, oids }))
}

pub(super) fn validate_roots(
    reference: &CommitGraphLayerRef,
    roots: &CommitGraphLayerPages,
    path: &str,
) -> Result<()> {
    roots
        .records
        .validate(Kind::CommitRecords, reference.bytes)?;
    roots.oids.validate(Kind::CommitOids, reference.bytes)?;
    let end = reference
        .base_ordinal
        .checked_add(reference.commit_count)
        .and_then(|end| end.checked_sub(1));
    if roots.records.count != reference.commit_count
        || roots.oids.count != reference.commit_count
        || roots.records.first != reference.base_ordinal.to_be_bytes()
        || end.is_none_or(|end| roots.records.last != end.to_be_bytes())
    {
        return corrupt_at(path, "paged graph root coverage mismatch");
    }
    Ok(())
}

#[cfg(any(feature = "storage", test))]
pub(super) fn record(bytes: &[u8], ordinal: u32, path: &str) -> Result<CommitGraphRecord> {
    let mut input = Input::new(bytes, path);
    let oid = input.array()?;
    let tree_oid = input.array()?;
    let commit_time = i64::from_le_bytes(input.array()?);
    let corrected_generation = u64::from_le_bytes(input.array()?);
    let count = input.u32()? as usize;
    if count > MAX_PARENTS || count > input.remaining() / 4 {
        return corrupt_at(path, "paged graph parent count exceeds its bounds");
    }
    let mut parents = Vec::with_capacity(count);
    for _ in 0..count {
        let parent = input.u32()?;
        if parent >= ordinal {
            return corrupt_at(path, "paged graph parent is not earlier than child");
        }
        parents.push(parent);
    }
    input.finish()?;
    Ok(CommitGraphRecord {
        oid,
        tree_oid,
        commit_time,
        corrected_generation,
        parents,
    })
}

#[cfg(any(feature = "storage", test))]
pub(super) fn decode_layer(
    bytes: &[u8],
    reference: &CommitGraphLayerRef,
    roots: &CommitGraphLayerPages,
    path: &str,
) -> Result<CommitGraphLayer> {
    validate_roots(reference, roots, path)?;
    let mut visited = BTreeMap::new();
    let rows = pages::scan(
        bytes,
        &roots.records,
        Kind::CommitRecords,
        path,
        &mut visited,
    )?;
    let mut expected_oids = BTreeMap::new();
    let mut records = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let ordinal =
            u32::from_be_bytes(key.as_slice().try_into().map_err(|_| {
                crate::validation::corrupt_object(path, "invalid graph ordinal key")
            })?);
        let record = record(&value, ordinal, path)?;
        if expected_oids
            .insert(record.oid.to_vec(), ordinal.to_le_bytes().to_vec())
            .is_some()
        {
            return corrupt_at(path, "duplicate graph OID");
        }
        records.push(record);
    }
    let actual_oids = pages::scan(bytes, &roots.oids, Kind::CommitOids, path, &mut visited)?;
    if actual_oids != expected_oids.into_iter().collect::<Vec<_>>() {
        return corrupt_at(
            path,
            "graph OID directory disagrees with positional records",
        );
    }
    let mut offset = 0u64;
    for (start, (_, page)) in visited {
        if start != offset {
            return corrupt_at(path, "graph pages overlap or leave unreferenced bytes");
        }
        offset = offset.checked_add(u64::from(page.bytes)).ok_or_else(|| {
            crate::validation::corrupt_object(path, "graph page coverage overflow")
        })?;
    }
    if offset != bytes.len() as u64 {
        return corrupt_at(path, "graph has trailing unreferenced bytes");
    }
    Ok(CommitGraphLayer {
        base_ordinal: reference.base_ordinal,
        records,
    })
}
