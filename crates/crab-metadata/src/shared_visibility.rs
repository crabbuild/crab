//! Shared, exact object memberships for capsule deltas and checkpoint closures.
//! Ordinals never authorize objects without the ref's authenticated set root.
use crate::error::{MetadataError, Result};
use crate::git_visibility::{
    GitVisibilityDictionary, GitVisibilityObjects, MAX_GIT_VISIBILITY_INDEX_BYTES,
    MAX_GIT_VISIBILITY_OBJECTS, VISIBILITY_CHUNK_BYTES, decode_oid, encode_oid,
};
use crate::pages::{Kind, PageRef, Writer};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

pub(crate) const CHUNK_BITS: usize = VISIBILITY_CHUNK_BYTES * 8;
pub(crate) const TABLE_SLOTS: usize = 256;
#[cfg(feature = "storage")]
mod reader;
#[cfg(feature = "storage")]
pub(crate) use reader::Reader;

type Chunk = Arc<[u8; VISIBILITY_CHUNK_BYTES]>;
type Seen = BTreeMap<u64, (Kind, PageRef)>;
fn corrupt(reason: &str) -> MetadataError {
    crate::validation::corrupt_object("shared visibility", reason)
}
fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| MetadataError::CapsuleVisibilityRecord { source })
}
pub(crate) fn ordinal(bytes: &[u8]) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .try_into()
            .map_err(|_| corrupt("invalid visibility ordinal"))?,
    ))
}
fn optional_tree(
    writer: &mut Writer,
    kind: Kind,
    rows: crate::pages::Rows,
) -> Result<Option<PageRef>> {
    if rows.is_empty() {
        Ok(None)
    } else {
        writer.tree(kind, rows).map(Some)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    pub(crate) dictionary: Option<PageRef>,
    pub(crate) ordinals: Option<PageRef>,
    pub(crate) chunks: Option<PageRef>,
}
impl Descriptor {
    pub(crate) fn object_count(&self) -> usize {
        self.ordinals.as_ref().map_or(0, |root| root.count as usize)
    }
    pub(crate) fn chunk_count(&self) -> usize {
        self.chunks.as_ref().map_or(0, |root| root.count as usize)
    }
    pub(crate) fn slots(&self) -> usize {
        self.object_count().div_ceil(CHUNK_BITS)
    }
    pub(crate) fn binding(&self) -> Result<[u8; 32]> {
        let mut identity = self.clone();
        for root in [
            &mut identity.dictionary,
            &mut identity.ordinals,
            &mut identity.chunks,
        ]
        .into_iter()
        .flatten()
        {
            root.offset = 0;
        }
        let mut hash = blake3::Hasher::new();
        hash.update(b"crab shared visibility roots v1\0");
        hash.update(&json(&identity)?);
        Ok(*hash.finalize().as_bytes())
    }
    pub(crate) fn validate(&self, body_bytes: u64) -> Result<()> {
        if self.object_count() as u64 > MAX_GIT_VISIBILITY_OBJECTS
            || self
                .dictionary
                .as_ref()
                .map_or(0, |root| root.count as usize)
                != self.object_count()
            || self.dictionary.is_some() != self.ordinals.is_some()
            || self.chunk_count() as u64
                > MAX_GIT_VISIBILITY_INDEX_BYTES / VISIBILITY_CHUNK_BYTES as u64
            || (self.object_count() == 0 && self.chunks.is_some())
        {
            return Err(corrupt("shared visibility dictionary shape is invalid"));
        }
        if let Some(root) = &self.dictionary {
            root.validate(Kind::VisibilityDictionary, body_bytes)?;
        }
        for (root, kind) in [
            (&self.ordinals, Kind::VisibilityOrdinals),
            (&self.chunks, Kind::VisibilityChunkPool),
        ] {
            if let Some(root) = root {
                validate_positional(root, kind, body_bytes, root.count as usize)?;
            }
        }
        Ok(())
    }
}
fn validate_positional(root: &PageRef, kind: Kind, body_bytes: u64, count: usize) -> Result<()> {
    root.validate(kind, body_bytes)?;
    if count == 0
        || root.count as usize != count
        || ordinal(&root.first)? != 0
        || ordinal(&root.last)? as usize != count - 1
    {
        return Err(corrupt("shared visibility positional coverage is invalid"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Encoding {
    Sparse,
    Bitmap,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetRoot {
    pub(crate) encoding: Encoding,
    pub(crate) count: u32,
    pub(crate) root: PageRef,
}
impl SetRoot {
    pub(crate) fn validate(&self, descriptor: &Descriptor, body_bytes: u64) -> Result<()> {
        if self.count == 0 || self.count as usize > descriptor.object_count() {
            return Err(corrupt("shared visibility membership count is invalid"));
        }
        match self.encoding {
            Encoding::Sparse => {
                self.root.validate(Kind::VisibilitySparseSet, body_bytes)?;
                if self.root.count != self.count
                    || ordinal(&self.root.last)? as usize >= descriptor.object_count()
                    || self.count as usize > descriptor.slots()
                {
                    return Err(corrupt("shared sparse visibility shape is invalid"));
                }
            }
            Encoding::Bitmap => {
                validate_positional(
                    &self.root,
                    Kind::VisibilitySetChunks,
                    body_bytes,
                    descriptor.slots().div_ceil(TABLE_SLOTS),
                )?;
                if descriptor.chunks.is_none() || self.count as usize <= descriptor.slots() {
                    return Err(corrupt("shared bitmap visibility shape is invalid"));
                }
            }
        }
        Ok(())
    }
    #[cfg(feature = "storage")]
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.encoding == other.encoding
            && self.count == other.count
            && self.root.hash == other.root.hash
            && self.root.bytes == other.root.bytes
            && self.root.count == other.root.count
            && self.root.height == other.root.height
            && self.root.first == other.root.first
            && self.root.last == other.root.last
    }
}
#[derive(Clone)]
enum Normalized {
    Empty,
    Sparse(Arc<[u32]>),
    Bitmap { chunks: Arc<[Chunk]>, count: u32 },
}
pub(crate) struct Prepared {
    dictionary: Arc<GitVisibilityDictionary>,
    sets: Vec<Normalized>,
    pool: BTreeMap<[u8; VISIBILITY_CHUNK_BYTES], u32>,
}
fn identity(set: &GitVisibilityObjects) -> Option<(usize, usize, usize)> {
    set.sharing_identity()
}
impl Prepared {
    pub(crate) fn new<'a>(
        sets: impl IntoIterator<Item = &'a GitVisibilityObjects>,
    ) -> Result<Self> {
        let sets = sets.into_iter().collect::<Vec<_>>();
        let common = sets.iter().find_map(|set| set.shared_dictionary().cloned());
        let mut compatible = true;
        let dictionary = if let Some(common) = common {
            let mut union = vec![0u8; common.objects().len().div_ceil(8)];
            let mut visited = BTreeSet::new();
            let mut checked = BTreeSet::new();
            for set in &sets {
                if identity(set).is_some_and(|key| !visited.insert(key)) {
                    continue;
                }
                if let Some((dictionary, chunks)) = set.shared_parts() {
                    let key = Arc::as_ptr(dictionary) as usize;
                    if checked.insert(key)
                        && !Arc::ptr_eq(dictionary, &common)
                        && dictionary.objects() != common.objects()
                    {
                        compatible = false;
                        break;
                    }
                    for (target, source) in union
                        .iter_mut()
                        .zip(chunks.iter().flat_map(|chunk| chunk.iter()))
                    {
                        *target |= source;
                    }
                } else {
                    for oid in set.iter() {
                        match common.objects().binary_search(oid) {
                            Ok(index) => union[index / 8] |= 1 << (index % 8),
                            Err(_) => {
                                compatible = false;
                                break;
                            }
                        }
                    }
                }
            }
            if compatible
                && union
                    .iter()
                    .map(|byte| byte.count_ones() as usize)
                    .sum::<usize>()
                    == common.objects().len()
            {
                common
            } else {
                Self::union_dictionary(&sets)?
            }
        } else {
            Self::union_dictionary(&sets)?
        };
        let mut pool = BTreeMap::new();
        let mut normalized = Vec::with_capacity(sets.len());
        let mut cache: HashMap<(usize, usize, usize), Normalized> = HashMap::new();
        for set in sets {
            let key = identity(set);
            if let Some(existing) = key.and_then(|key| cache.get(&key)) {
                normalized.push(existing.clone());
                continue;
            }
            let prepared = if set.is_empty() {
                Normalized::Empty
            } else if set.len() <= dictionary.objects().len().div_ceil(CHUNK_BITS) {
                let positions = set
                    .iter()
                    .map(|oid| {
                        dictionary
                            .objects()
                            .binary_search(oid)
                            .map(|index| index as u32)
                            .map_err(|_| {
                                corrupt("shared visibility object is absent from dictionary")
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                Normalized::Sparse(positions.into())
            } else {
                let chunks = match set.shared_parts() {
                    Some((old, chunks))
                        if Arc::ptr_eq(old, &dictionary)
                            || old.objects() == dictionary.objects() =>
                    {
                        chunks.to_vec()
                    }
                    _ => {
                        let mut chunks = vec![
                            [0; VISIBILITY_CHUNK_BYTES];
                            dictionary.objects().len().div_ceil(CHUNK_BITS)
                        ];
                        for oid in set.iter() {
                            let index = dictionary
                                .objects()
                                .binary_search(oid)
                                .map_err(|_| corrupt("visibility object is absent"))?;
                            chunks[index / CHUNK_BITS][index % CHUNK_BITS / 8] |= 1 << (index % 8);
                        }
                        chunks.into_iter().map(Arc::new).collect()
                    }
                };
                for chunk in &chunks {
                    pool.entry(**chunk).or_insert(0);
                }
                Normalized::Bitmap {
                    chunks: chunks.into(),
                    count: set.len() as u32,
                }
            };
            if let Some(key) = key {
                cache.insert(key, prepared.clone());
            }
            normalized.push(prepared);
        }
        for (index, ordinal) in pool.values_mut().enumerate() {
            *ordinal = index as u32;
        }
        Ok(Self {
            dictionary,
            sets: normalized,
            pool,
        })
    }
    fn union_dictionary(sets: &[&GitVisibilityObjects]) -> Result<Arc<GitVisibilityDictionary>> {
        let mut objects = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for set in sets {
            if identity(set).is_some_and(|key| !visited.insert(key)) {
                continue;
            }
            for oid in set.iter() {
                objects.insert(oid.clone());
                if objects.len() as u64 > MAX_GIT_VISIBILITY_OBJECTS {
                    return Err(corrupt("shared visibility dictionary exceeds object bound"));
                }
            }
        }
        GitVisibilityDictionary::new(objects.into_iter().collect())
    }
    pub(crate) fn encode(&self, writer: &mut Writer) -> Result<(Descriptor, Vec<Option<SetRoot>>)> {
        let dictionary = optional_tree(
            writer,
            Kind::VisibilityDictionary,
            self.dictionary
                .objects()
                .iter()
                .enumerate()
                .map(|(index, oid)| {
                    Ok((
                        decode_oid(oid)?.to_vec(),
                        (index as u32).to_be_bytes().to_vec(),
                    ))
                })
                .collect::<Result<_>>()?,
        )?;
        let ordinals = optional_tree(
            writer,
            Kind::VisibilityOrdinals,
            self.dictionary
                .objects()
                .iter()
                .enumerate()
                .map(|(index, oid)| {
                    Ok((
                        (index as u32).to_be_bytes().to_vec(),
                        decode_oid(oid)?.to_vec(),
                    ))
                })
                .collect::<Result<_>>()?,
        )?;
        let chunks = optional_tree(
            writer,
            Kind::VisibilityChunkPool,
            self.pool
                .iter()
                .map(|(chunk, index)| (index.to_be_bytes().to_vec(), chunk.to_vec()))
                .collect(),
        )?;
        let descriptor = Descriptor {
            dictionary,
            ordinals,
            chunks,
        };
        let mut roots = Vec::with_capacity(self.sets.len());
        let mut cached: HashMap<(u8, usize), SetRoot> = HashMap::new();
        for set in &self.sets {
            let (key, encoding, count, rows) = match set {
                Normalized::Empty => {
                    roots.push(None);
                    continue;
                }
                Normalized::Sparse(positions) => {
                    let key = (0u8, positions.as_ptr() as usize);
                    if let Some(root) = cached.get(&key) {
                        roots.push(Some(root.clone()));
                        continue;
                    }
                    (
                        key,
                        Encoding::Sparse,
                        positions.len() as u32,
                        positions
                            .iter()
                            .map(|index| (index.to_be_bytes().to_vec(), Vec::new()))
                            .collect(),
                    )
                }
                Normalized::Bitmap { chunks, count } => {
                    let key = (1u8, chunks.as_ptr() as usize);
                    if let Some(root) = cached.get(&key) {
                        roots.push(Some(root.clone()));
                        continue;
                    }
                    let rows = chunks
                        .chunks(TABLE_SLOTS)
                        .enumerate()
                        .map(|(index, chunks)| {
                            (
                                (index as u32).to_be_bytes().to_vec(),
                                chunks
                                    .iter()
                                    .flat_map(|chunk| self.pool[chunk.as_ref()].to_be_bytes())
                                    .collect(),
                            )
                        })
                        .collect();
                    (key, Encoding::Bitmap, *count, rows)
                }
            };
            let root = SetRoot {
                encoding,
                count,
                root: writer.tree(
                    match encoding {
                        Encoding::Sparse => Kind::VisibilitySparseSet,
                        Encoding::Bitmap => Kind::VisibilitySetChunks,
                    },
                    rows,
                )?,
            };
            cached.insert(key, root.clone());
            roots.push(Some(root));
            if writer.bytes.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
                return Err(corrupt("shared visibility exceeds byte bound"));
            }
        }
        Ok((descriptor, roots))
    }
}

pub(crate) struct Decoded {
    dictionary: Arc<GitVisibilityDictionary>,
    pool: Vec<Chunk>,
    sets: BTreeMap<u64, (SetRoot, GitVisibilityObjects)>,
    empty: GitVisibilityObjects,
}
impl Decoded {
    pub(crate) fn new(body: &[u8], descriptor: &Descriptor, seen: &mut Seen) -> Result<Self> {
        descriptor.validate(body.len() as u64)?;
        let reverse = scan(
            body,
            descriptor.ordinals.as_ref(),
            Kind::VisibilityOrdinals,
            seen,
        )?;
        let mut objects = Vec::with_capacity(reverse.len());
        for (index, (key, value)) in reverse.into_iter().enumerate() {
            if ordinal(&key)? as usize != index {
                return Err(corrupt("reverse visibility dictionary has wrong ordinal"));
            }
            objects.push(encode_oid(
                &value
                    .try_into()
                    .map_err(|_| corrupt("reverse dictionary OID length is invalid"))?,
            ));
        }
        let dictionary = GitVisibilityDictionary::new(objects)?;
        let forward = scan(
            body,
            descriptor.dictionary.as_ref(),
            Kind::VisibilityDictionary,
            seen,
        )?;
        for (index, (key, value)) in forward.into_iter().enumerate() {
            if ordinal(&value)? as usize != index
                || dictionary.objects().get(index).is_none_or(|oid| {
                    decode_oid(oid).ok().as_ref().map(<[u8; 20]>::as_slice) != Some(key.as_slice())
                })
            {
                return Err(corrupt("visibility dictionary directions disagree"));
            }
        }
        let chunks = scan(
            body,
            descriptor.chunks.as_ref(),
            Kind::VisibilityChunkPool,
            seen,
        )?;
        let mut pool = Vec::with_capacity(chunks.len());
        let mut previous = None;
        for (index, (key, value)) in chunks.into_iter().enumerate() {
            let chunk: [u8; VISIBILITY_CHUNK_BYTES] = value
                .try_into()
                .map_err(|_| corrupt("visibility chunk length is invalid"))?;
            if ordinal(&key)? as usize != index
                || previous.as_ref().is_some_and(|prior| prior >= &chunk)
            {
                return Err(corrupt("visibility chunk pool is not canonical"));
            }
            previous = Some(chunk);
            pool.push(Arc::new(chunk));
        }
        let empty = GitVisibilityObjects::from_shared_ordinals(dictionary.clone(), Vec::new())?;
        Ok(Self {
            dictionary,
            pool,
            sets: BTreeMap::new(),
            empty,
        })
    }
    pub(crate) fn set(
        &mut self,
        body: &[u8],
        descriptor: &Descriptor,
        root: Option<&SetRoot>,
        seen: &mut Seen,
    ) -> Result<GitVisibilityObjects> {
        let Some(root) = root else {
            return Ok(self.empty.clone());
        };
        root.validate(descriptor, body.len() as u64)?;
        if let Some((prior, objects)) = self.sets.get(&root.root.offset) {
            if prior != root {
                return Err(corrupt("shared membership page has conflicting roots"));
            }
            return Ok(objects.clone());
        }
        let set = match root.encoding {
            Encoding::Sparse => {
                let rows = scan(body, Some(&root.root), Kind::VisibilitySparseSet, seen)?;
                let positions = rows
                    .into_iter()
                    .map(|(key, value)| {
                        if !value.is_empty() {
                            return Err(corrupt("sparse visibility row has payload"));
                        }
                        ordinal(&key)
                    })
                    .collect::<Result<Vec<_>>>()?;
                GitVisibilityObjects::from_shared_ordinals(self.dictionary.clone(), positions)?
            }
            Encoding::Bitmap => {
                let zero = Arc::new([0u8; VISIBILITY_CHUNK_BYTES]);
                let mut chunks = vec![zero; descriptor.slots()];
                let rows = scan(body, Some(&root.root), Kind::VisibilitySetChunks, seen)?;
                for (key, value) in rows {
                    let block = ordinal(&key)? as usize;
                    let ids = table_ids(descriptor, block, &value)?;
                    for (position, index) in ids.into_iter().enumerate() {
                        chunks[block * TABLE_SLOTS + position] = self.pool[index as usize].clone();
                    }
                }
                GitVisibilityObjects::from_shared_chunks(self.dictionary.clone(), chunks)?
            }
        };
        if set.len() != root.count as usize {
            return Err(corrupt(
                "shared visibility membership count differs from its bits",
            ));
        }
        self.sets
            .insert(root.root.offset, (root.clone(), set.clone()));
        Ok(set)
    }
}
fn scan(
    body: &[u8],
    root: Option<&PageRef>,
    kind: Kind,
    seen: &mut Seen,
) -> Result<crate::pages::Rows> {
    root.map(|root| crate::pages::scan(body, root, kind, "shared visibility", seen))
        .transpose()
        .map(Option::unwrap_or_default)
}
pub(crate) fn table_ids(descriptor: &Descriptor, block: usize, value: &[u8]) -> Result<Vec<u32>> {
    let start = block
        .checked_mul(TABLE_SLOTS)
        .filter(|start| *start < descriptor.slots())
        .ok_or_else(|| corrupt("visibility chunk block is out of range"))?;
    let count = TABLE_SLOTS.min(descriptor.slots() - start);
    if value.len() != count * 4 {
        return Err(corrupt("visibility chunk table has wrong coverage"));
    }
    value
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| {
            let index = ordinal(bytes)?;
            if index as usize >= descriptor.chunk_count() {
                Err(corrupt("visibility chunk pool reference is out of range"))
            } else {
                Ok(index)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn objects(
        dictionary: Arc<GitVisibilityDictionary>,
        positions: &[usize],
    ) -> GitVisibilityObjects {
        let mut chunks =
            vec![[0; VISIBILITY_CHUNK_BYTES]; dictionary.objects().len().div_ceil(CHUNK_BITS)];
        for &position in positions {
            chunks[position / CHUNK_BITS][position % CHUNK_BITS / 8] |= 1 << (position % 8);
        }
        GitVisibilityObjects::from_shared_chunks(
            dictionary,
            chunks.into_iter().map(Arc::new).collect(),
        )
        .unwrap()
    }
    #[test]
    fn exact_shared_sets_round_trip_without_losing_sparse_empty_or_dense_memberships() {
        let dictionary =
            GitVisibilityDictionary::new((0..2051).map(|index| format!("{index:040x}")).collect())
                .unwrap();
        let dense = objects(
            dictionary.clone(),
            &(0..2051).filter(|index| index % 2 == 0).collect::<Vec<_>>(),
        );
        let sparse = objects(dictionary, &[1, 1023]);
        let empty = GitVisibilityObjects::default();
        let inputs = [dense.clone(), sparse.clone(), empty];
        let prepared = Prepared::new(inputs.iter()).unwrap();
        let mut writer = Writer::default();
        let (descriptor, roots) = prepared.encode(&mut writer).unwrap();
        let mut seen = BTreeMap::new();
        let mut decoded = Decoded::new(&writer.bytes, &descriptor, &mut seen).unwrap();
        for (input, root) in inputs.iter().zip(&roots) {
            assert_eq!(
                input,
                &decoded
                    .set(&writer.bytes, &descriptor, root.as_ref(), &mut seen)
                    .unwrap()
            );
        }
        assert_eq!(roots[0].as_ref().unwrap().encoding, Encoding::Bitmap);
        assert_eq!(roots[1].as_ref().unwrap().encoding, Encoding::Sparse);
        assert!(roots[2].is_none());
    }
    #[test]
    fn different_dictionaries_and_unused_dictionary_members_are_normalized() {
        let a =
            GitVisibilityDictionary::new((0..10).map(|index| format!("{index:040x}")).collect())
                .unwrap();
        let b =
            GitVisibilityDictionary::new((8..20).map(|index| format!("{index:040x}")).collect())
                .unwrap();
        let inputs = [objects(a, &[1, 8]), objects(b, &[0, 3])];
        let prepared = Prepared::new(inputs.iter()).unwrap();
        assert_eq!(prepared.dictionary.objects().len(), 3);
        let mut writer = Writer::default();
        let (descriptor, roots) = prepared.encode(&mut writer).unwrap();
        let mut seen = BTreeMap::new();
        let mut decoded = Decoded::new(&writer.bytes, &descriptor, &mut seen).unwrap();
        for (input, root) in inputs.iter().zip(&roots) {
            assert_eq!(
                input,
                &decoded
                    .set(&writer.bytes, &descriptor, root.as_ref(), &mut seen)
                    .unwrap()
            );
        }
    }
}
