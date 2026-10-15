//! Run-wide catalog keys with exact logical-capsule occurrence membership.
use super::*;
use crate::pages::{Input, Kind, PageRef, Rows, Writer};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

const MAGIC: &[u8; 8] = b"CRBRCG01";
const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024;
const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
// Values contain fixed-width file/placement rows or one bounded PageRef row.
const MAX_VALUE_BYTES: usize = 2048;
const KINDS: [Kind; 4] = [
    Kind::RunCatalogFiles,
    Kind::RunCatalogShards,
    Kind::RunCatalogXorbs,
    Kind::RunCatalogChunks,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CatalogPages {
    version: u32,
    capsule_bodies: Vec<Option<u64>>,
    body_bytes: u64,
    body_hash: [u8; 32],
    roots: [Option<PageRef>; 4],
}

struct Variant {
    identity: [u8; 32],
    source: u16,
    members: Vec<u16>,
    value: Vec<u8>,
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| MetadataError::PointerCatalogRecord { source })
}

impl CatalogPages {
    pub(super) fn build(capsules: &[Capsule]) -> Result<Option<(Bytes, Self)>> {
        let mut directories = KINDS.map(|_| BTreeMap::<_, Variant>::new());
        let mut capsule_bodies = Vec::with_capacity(capsules.len());
        let mut admitted = 0_u64;
        for (position, capsule) in capsules.iter().enumerate() {
            let position = u16::try_from(position)
                .map_err(|_| corrupt("catalog capsule position overflows"))?;
            let Some(bytes) = section_bytes_for_kind(capsule, CapsuleSectionKind::CatalogDelta)?
            else {
                capsule_bodies.push(None);
                continue;
            };
            let (body_bytes, rows) = super::super::pointer::paged::index_rows(&bytes)?;
            capsule_bodies.push(Some(body_bytes));
            for (directory, rows) in directories.iter_mut().zip(rows) {
                for row in rows {
                    if row.value.len() > MAX_VALUE_BYTES {
                        return Err(corrupt("run catalog value exceeds its bound"));
                    }
                    let key = (row.key, row.identity);
                    match directory.entry(key) {
                        std::collections::btree_map::Entry::Vacant(entry) => {
                            admitted = admitted.saturating_add(row.value.len() as u64 + 128);
                            entry.insert(Variant {
                                identity: row.identity,
                                source: position,
                                members: vec![position],
                                value: row.value,
                            });
                        }
                        std::collections::btree_map::Entry::Occupied(mut entry) => {
                            admitted = admitted.saturating_add(2);
                            entry.get_mut().members.push(position);
                        }
                    }
                    if admitted > MAX_BYTES {
                        return Err(corrupt("run catalog exceeds its byte bound"));
                    }
                }
            }
        }
        if directories.iter().all(BTreeMap::is_empty) {
            return Ok(None);
        }
        let mut writer = Writer::default();
        let mut roots = std::array::from_fn(|_| None);
        for (position, directory) in directories.into_iter().enumerate() {
            let mut rows: Rows = Vec::new();
            let mut key = None;
            let mut variants = Vec::new();
            for ((next, _), variant) in directory {
                if key.as_ref().is_some_and(|key| key != &next) {
                    if let Some(key) = key.take() {
                        rows.push((key, encode_variants(&variants)?));
                    }
                    variants.clear();
                }
                key = Some(next);
                variants.push(variant);
            }
            if let Some(key) = key {
                rows.push((key, encode_variants(&variants)?));
            }
            if !rows.is_empty() {
                roots[position] = Some(writer.tree(KINDS[position], rows)?);
            }
            if writer.bytes.len() as u64 > MAX_BYTES {
                return Err(corrupt("run catalog exceeds its byte bound"));
            }
        }
        let descriptor = Self {
            version: 1,
            capsule_bodies,
            body_bytes: writer.bytes.len() as u64,
            body_hash: *blake3::hash(&writer.bytes).as_bytes(),
            roots,
        };
        let header = json(&descriptor)?;
        if header.len() > MAX_DESCRIPTOR_BYTES {
            return Err(corrupt("run catalog descriptor exceeds its byte bound"));
        }
        writer.bytes.extend_from_slice(&header);
        crate::pages::put_u32(&mut writer.bytes, header.len())?;
        writer.bytes.extend_from_slice(MAGIC);
        descriptor.validate_region(writer.bytes.len() as u64, capsules.len())?;
        Ok(Some((writer.bytes.into(), descriptor)))
    }

    pub(super) fn validate_presence(
        pages: Option<&Self>,
        capsules: &[RunCapsuleLocation],
    ) -> Result<()> {
        let empty = PointerCatalog::new().encode_delta()?;
        let empty_hash = blake3::hash(&empty).to_hex().to_string();
        if pages.is_some_and(|pages| pages.capsule_bodies.len() != capsules.len()) {
            return Err(corrupt(
                "run catalog source count differs from its capsules",
            ));
        }
        for (position, capsule) in capsules.iter().enumerate() {
            let body = pages.and_then(|pages| pages.capsule_bodies[position]);
            match (&capsule.catalog, body) {
                (None, None) => {}
                (Some(range), None)
                    if pages.is_none()
                        && range.hash == empty_hash
                        && range.length == empty.len() as u64 => {}
                (Some(range), Some(body))
                    if body
                        .checked_add(12)
                        .is_some_and(|length| length < range.length)
                        && (body != 0
                            || (range.hash == empty_hash
                                && range.length == empty.len() as u64)) => {}
                _ => {
                    return Err(corrupt(
                        "run catalog sources differ from their capsule sections",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_region(&self, length: u64, capsule_count: usize) -> Result<()> {
        if self.version != 1
            || capsule_count == 0
            || capsule_count > MAX_CAPSULES_PER_RUN
            || self.capsule_bodies.len() != capsule_count
            || self.body_bytes == 0
            || self.roots.iter().all(Option::is_none)
            || json(self)?.len() > MAX_DESCRIPTOR_BYTES
            || self
                .body_bytes
                .checked_add(json(self)?.len() as u64)
                .and_then(|n| n.checked_add(12))
                != Some(length)
            || length > MAX_BYTES
            || self
                .capsule_bodies
                .iter()
                .flatten()
                .any(|bytes| *bytes > MAX_BYTES)
        {
            return Err(corrupt("run catalog descriptor has an invalid shape"));
        }
        for (root, kind) in self.roots.iter().zip(KINDS) {
            if let Some(root) = root {
                root.validate(kind, self.body_bytes)?;
            }
        }
        Ok(())
    }

    pub(super) fn verify(&self, bytes: &[u8], capsules: &[Capsule]) -> Result<()> {
        self.validate_region(bytes.len() as u64, capsules.len())?;
        let header = json(self)?;
        let start = self.body_bytes as usize;
        if bytes.get(start..start + header.len()) != Some(header.as_slice())
            || bytes.get(bytes.len() - 8..) != Some(MAGIC.as_slice())
            || u32::from_le_bytes(
                bytes[bytes.len() - 12..bytes.len() - 8]
                    .try_into()
                    .map_err(|_| corrupt("run catalog trailer is invalid"))?,
            ) as usize
                != header.len()
            || blake3::hash(&bytes[..start]).as_bytes() != &self.body_hash
        {
            return Err(corrupt("run catalog body differs from its descriptor"));
        }
        let mut visited = BTreeMap::new();
        for (root, kind) in self.roots.iter().zip(KINDS) {
            if let Some(root) = root {
                for (_, value) in crate::pages::scan(
                    &bytes[..start],
                    root,
                    kind,
                    "run catalog pages",
                    &mut visited,
                )? {
                    decode_variants(&value, capsules.len())?;
                }
            }
        }
        // Footer roots cannot invent occurrence membership. Full verification
        // ties every variant and physical reference back to the nested capsules.
        let expected = Self::build(capsules)?
            .ok_or_else(|| corrupt("run catalog has no nested catalog contribution"))?;
        if expected.0.as_ref() != bytes || &expected.1 != self {
            return Err(corrupt("run catalog does not match its nested capsules"));
        }
        Ok(())
    }
}

fn encode_variants(variants: &[Variant]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    put_u16(&mut bytes, variants.len())?;
    for variant in variants {
        bytes.extend_from_slice(&variant.identity);
        bytes.extend_from_slice(&variant.source.to_le_bytes());
        put_u16(&mut bytes, variant.members.len())?;
        for member in &variant.members {
            bytes.extend_from_slice(&member.to_le_bytes());
        }
        put_u16(&mut bytes, variant.value.len())?;
        bytes.extend_from_slice(&variant.value);
    }
    Ok(bytes)
}

fn decode_variants(bytes: &[u8], capsule_count: usize) -> Result<Vec<Variant>> {
    let mut input = Input::new(bytes, "run catalog variants");
    let count = u16::from_le_bytes(input.array()?) as usize;
    if count == 0 || count > capsule_count {
        return Err(corrupt("run catalog variant count is invalid"));
    }
    let mut output: Vec<Variant> = Vec::with_capacity(count);
    let mut seen = BTreeSet::new();
    for _ in 0..count {
        let identity = input.array()?;
        if output
            .last()
            .is_some_and(|previous| previous.identity >= identity)
        {
            return Err(corrupt("run catalog variants are not sorted and unique"));
        }
        let source = u16::from_le_bytes(input.array()?);
        let count = u16::from_le_bytes(input.array()?) as usize;
        if count == 0 || count > capsule_count {
            return Err(corrupt("run catalog occurrence count is invalid"));
        }
        let mut members = Vec::with_capacity(count);
        for _ in 0..count {
            let member = u16::from_le_bytes(input.array()?);
            if member as usize >= capsule_count
                || members.last().is_some_and(|previous| *previous >= member)
                || !seen.insert(member)
            {
                return Err(corrupt("run catalog occurrences are invalid or overlap"));
            }
            members.push(member);
        }
        if members.first() != Some(&source) {
            return Err(corrupt("run catalog source is not its first occurrence"));
        }
        let length = u16::from_le_bytes(input.array()?) as usize;
        if length == 0 || length > MAX_VALUE_BYTES {
            return Err(corrupt("run catalog value has an invalid length"));
        }
        output.push(Variant {
            identity,
            source,
            members,
            value: input.take(length)?.to_vec(),
        });
    }
    input.finish()?;
    Ok(output)
}

fn put_u16(bytes: &mut Vec<u8>, value: usize) -> Result<()> {
    bytes.extend_from_slice(
        &u16::try_from(value)
            .map_err(|_| corrupt("run catalog scalar overflows"))?
            .to_le_bytes(),
    );
    Ok(())
}
