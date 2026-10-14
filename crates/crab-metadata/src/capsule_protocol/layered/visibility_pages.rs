//! Authenticated visibility dictionaries and compressed closure pages.
use super::*;
use crate::pages::{self, Kind, PageRef, Rows, Writer};

const MAGIC: &[u8; 8] = b"CRBVPG03";
const HEADER_BYTES: usize = 12;
const MAX_DIRECTORY_BYTES: usize = 16 * 1024;
const BLOCK_BITS: u32 = 4096;
const BLOCK_BYTES: usize = BLOCK_BITS as usize / 8;
const PATH: &str = "layered visibility pages";

#[cfg(feature = "storage")]
mod reader;
#[cfg(feature = "storage")]
pub use reader::LayeredVisibilityReader;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Directory {
    catalog_digest: String,
    body_bytes: u64,
    member_admission: bool,
    objects: Option<PageRef>,
    refs: Option<PageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Closure {
    count: u32,
    root: Option<PageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefRecord {
    name: String,
    closure: Closure,
    transitions: Option<PageRef>,
    history: Option<PageRef>,
    has_transitions: bool,
    has_history: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transition {
    from: u32,
    to: u32,
    objects: Closure,
}

type Audit = BTreeMap<u64, (Kind, PageRef)>;

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| MetadataError::VisibilityPageRecord { source })
}

fn record<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|source| MetadataError::VisibilityPageRecord { source })
}

fn tree(writer: &mut Writer, kind: Kind, mut rows: Rows) -> Result<Option<PageRef>> {
    if rows.is_empty() {
        return Ok(None);
    }
    rows.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    writer.tree(kind, rows).map(Some)
}

fn closure(writer: &mut Writer, positions: &[u32]) -> Result<Closure> {
    let mut blocks = BTreeMap::<u32, Vec<u16>>::new();
    for position in positions {
        blocks
            .entry(position / BLOCK_BITS)
            .or_default()
            .push((position % BLOCK_BITS) as u16);
    }
    let rows = blocks
        .into_iter()
        .map(|(block, positions)| {
            let mut value = Vec::new();
            if positions.len() * 2 <= BLOCK_BYTES {
                value.push(0);
                for position in positions {
                    value.extend_from_slice(&position.to_be_bytes());
                }
            } else {
                value.resize(BLOCK_BYTES + 1, 0);
                value[0] = 1;
                for position in positions {
                    let position = usize::from(position);
                    value[1 + position / 8] |= 1 << (position % 8);
                }
            }
            (block.to_be_bytes().to_vec(), value)
        })
        .collect();
    Ok(Closure {
        count: u32::try_from(positions.len())
            .map_err(|_| corrupt("visibility closure count overflows"))?,
        root: tree(writer, Kind::VisibilityClosures, rows)?,
    })
}

fn transitions(
    writer: &mut Writer,
    entries: &[GitVisibilityOrdinalTransition],
) -> Result<Option<PageRef>> {
    let mut rows = Vec::with_capacity(entries.len());
    for (ordinal, entry) in entries.iter().enumerate() {
        let value = Transition {
            from: entry.from_ordinal,
            to: entry.to_ordinal,
            objects: closure(writer, &entry.objects)?,
        };
        let ordinal =
            u32::try_from(ordinal).map_err(|_| corrupt("visibility transition count overflows"))?;
        rows.push((ordinal.to_be_bytes().to_vec(), json(&value)?));
    }
    tree(writer, Kind::VisibilityTransitions, rows)
}

pub(super) fn encode(snapshot: &LayeredVisibilitySnapshot) -> Result<Bytes> {
    snapshot.validate()?;
    let mut writer = Writer::default();
    let objects = snapshot
        .objects
        .iter()
        .enumerate()
        .map(|(ordinal, oid)| {
            let mut value = Vec::new();
            if let Some(admission) = &snapshot.member_admission {
                let member = admission[ordinal];
                value.extend_from_slice(&member.source_index.to_be_bytes());
                value.extend_from_slice(&member.member_index.to_be_bytes());
            }
            (oid.to_vec(), value)
        })
        .collect();
    let objects = tree(&mut writer, Kind::VisibilityOids, objects)?;
    let mut refs = Vec::with_capacity(snapshot.refs.len());
    for (name, positions) in &snapshot.refs {
        let value = RefRecord {
            name: name.clone(),
            closure: closure(&mut writer, positions)?,
            transitions: transitions(
                &mut writer,
                snapshot.transitions.get(name).map_or(&[], Vec::as_slice),
            )?,
            history: transitions(
                &mut writer,
                snapshot
                    .incremental_history
                    .get(name)
                    .map_or(&[], Vec::as_slice),
            )?,
            has_transitions: snapshot.transitions.contains_key(name),
            has_history: snapshot.incremental_history.contains_key(name),
        };
        refs.push((
            blake3::hash(name.as_bytes()).as_bytes().to_vec(),
            json(&value)?,
        ));
    }
    let refs = tree(&mut writer, Kind::VisibilityRefs, refs)?;
    let directory = Directory {
        catalog_digest: snapshot.catalog_digest.clone(),
        body_bytes: writer.bytes.len() as u64,
        member_admission: snapshot.member_admission.is_some(),
        objects,
        refs,
    };
    directory.validate()?;
    let header = json(&directory)?;
    if header.len() > MAX_DIRECTORY_BYTES {
        return Err(corrupt("visibility directory exceeds its byte bound"));
    }
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
    bytes.extend(header);
    bytes.extend(writer.bytes);
    if bytes.len() > MAX_LAYERED_VISIBILITY_BYTES {
        return Err(corrupt(
            "layered visibility snapshot exceeds its size bound",
        ));
    }
    Ok(bytes.into())
}

impl Directory {
    pub(super) fn validate(&self) -> Result<()> {
        validate_content_hash(&self.catalog_digest, "visibility source catalog", PATH)?;
        for (root, kind, maximum) in [
            (
                &self.objects,
                Kind::VisibilityOids,
                crate::git_visibility::MAX_GIT_VISIBILITY_OBJECTS,
            ),
            (
                &self.refs,
                Kind::VisibilityRefs,
                MAX_LAYERED_VISIBILITY_REFS as u64,
            ),
        ] {
            if let Some(root) = root {
                root.validate(kind, self.body_bytes)?;
                if u64::from(root.count) > maximum {
                    return Err(corrupt("visibility directory exceeds its count bound"));
                }
            }
        }
        if self.body_bytes > MAX_LAYERED_VISIBILITY_BYTES as u64 {
            return Err(corrupt("visibility page body exceeds its byte bound"));
        }
        Ok(())
    }
}

pub(super) fn directory(bytes: &[u8]) -> Result<Directory> {
    if bytes.len() > MAX_LAYERED_VISIBILITY_BYTES || bytes.get(..8) != Some(MAGIC) {
        return Err(corrupt("visibility page magic or byte bound is invalid"));
    }
    let header_bytes = scalar(
        bytes
            .get(8..12)
            .ok_or_else(|| corrupt("visibility directory is truncated"))?,
    )? as usize;
    if header_bytes > MAX_DIRECTORY_BYTES {
        return Err(corrupt("visibility directory exceeds its byte bound"));
    }
    let end = HEADER_BYTES
        .checked_add(header_bytes)
        .ok_or_else(|| corrupt("visibility directory range overflows"))?;
    let directory: Directory = record(
        bytes
            .get(HEADER_BYTES..end)
            .ok_or_else(|| corrupt("visibility directory is truncated"))?,
    )?;
    directory.validate()?;
    if bytes.len().checked_sub(end).map(|size| size as u64) != Some(directory.body_bytes) {
        return Err(corrupt("visibility body length differs from its directory"));
    }
    Ok(directory)
}

impl Directory {
    pub(super) fn validate_checkpoint(
        &self,
        sources: &[PackSourceDescriptor],
        section_bytes: u64,
        object_count: Option<u64>,
    ) -> Result<u64> {
        self.validate()?;
        let header_bytes = section_bytes
            .checked_sub(self.body_bytes)
            .ok_or_else(|| corrupt("visibility section does not cover its pages"))?;
        if header_bytes < HEADER_BYTES as u64
            || header_bytes > (HEADER_BYTES + MAX_DIRECTORY_BYTES) as u64
            || section_bytes > MAX_LAYERED_VISIBILITY_BYTES as u64
            || self.catalog_digest != source_catalog_digest(sources)?
            || object_count
                != Some(
                    self.objects
                        .as_ref()
                        .map_or(0, |root| u64::from(root.count)),
                )
        {
            return Err(corrupt(
                "visibility directory does not match its checkpoint",
            ));
        }
        Ok(header_bytes)
    }
}

fn rows(body: &[u8], root: Option<&PageRef>, kind: Kind, audit: &mut Audit) -> Result<Rows> {
    match root {
        Some(root) => pages::scan(body, root, kind, PATH, audit),
        None => Ok(Vec::new()),
    }
}

fn scalar(bytes: &[u8]) -> Result<u32> {
    bytes
        .try_into()
        .map(u32::from_be_bytes)
        .map_err(|_| corrupt("visibility ordinal is invalid"))
}

fn positions(
    body: &[u8],
    closure: &Closure,
    object_count: usize,
    audit: &mut Audit,
) -> Result<Vec<u32>> {
    if closure.count as usize > object_count || (closure.count == 0) != closure.root.is_none() {
        return Err(corrupt("visibility closure count is invalid"));
    }
    let mut output = Vec::with_capacity(closure.count as usize);
    for (key, value) in rows(body, closure.root.as_ref(), Kind::VisibilityClosures, audit)? {
        let start = scalar(&key)?
            .checked_mul(BLOCK_BITS)
            .ok_or_else(|| corrupt("visibility block overflows"))?;
        let positions: Vec<u16> = match value.first() {
            Some(0)
                if value.len() > 1 && value.len() <= BLOCK_BYTES + 1 && value.len() % 2 == 1 =>
            {
                value[1..]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|bytes| u16::from_be_bytes(*bytes))
                    .collect()
            }
            Some(1) if value.len() == BLOCK_BYTES + 1 => value[1..]
                .iter()
                .enumerate()
                .flat_map(|(index, byte)| {
                    (0..8).filter_map(move |bit| {
                        (byte & (1 << bit) != 0).then_some((index * 8 + bit) as u16)
                    })
                })
                .collect(),
            _ => return Err(corrupt("visibility block encoding is invalid")),
        };
        if positions.is_empty() || positions.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(corrupt("visibility block positions are empty or unordered"));
        }
        for position in positions {
            if u32::from(position) >= BLOCK_BITS {
                return Err(corrupt("visibility block position is out of bounds"));
            }
            let position = start
                .checked_add(u32::from(position))
                .ok_or_else(|| corrupt("visibility position overflows"))?;
            if position as usize >= object_count || output.len() >= closure.count as usize {
                return Err(corrupt("visibility closure exceeds its declared coverage"));
            }
            output.push(position);
        }
    }
    if output.len() != closure.count as usize {
        return Err(corrupt("visibility closure has incomplete coverage"));
    }
    Ok(output)
}

fn restore_transitions(
    body: &[u8],
    root: Option<&PageRef>,
    object_count: usize,
    maximum: usize,
    audit: &mut Audit,
) -> Result<Vec<GitVisibilityOrdinalTransition>> {
    if root.is_some_and(|root| root.count as usize > maximum) {
        return Err(corrupt("visibility transition count exceeds its bound"));
    }
    let mut output = Vec::new();
    for (key, value) in rows(body, root, Kind::VisibilityTransitions, audit)? {
        if scalar(&key)? as usize != output.len() {
            return Err(corrupt(
                "visibility transitions have incomplete positional coverage",
            ));
        }
        let entry: Transition = record(&value)?;
        output.push(GitVisibilityOrdinalTransition {
            from_ordinal: entry.from,
            to_ordinal: entry.to,
            objects: positions(body, &entry.objects, object_count, audit)?,
        });
    }
    Ok(output)
}

pub(super) fn decode(bytes: &[u8]) -> Result<LayeredVisibilitySnapshot> {
    let directory = directory(bytes)?;
    let body = &bytes[bytes.len() - directory.body_bytes as usize..];
    let mut audit = BTreeMap::new();
    let mut objects = Vec::new();
    let mut admission = directory.member_admission.then(Vec::new);
    for (oid, value) in rows(
        body,
        directory.objects.as_ref(),
        Kind::VisibilityOids,
        &mut audit,
    )? {
        objects.push(
            oid.try_into()
                .map_err(|_| corrupt("visibility OID width is invalid"))?,
        );
        if let Some(admission) = &mut admission {
            if value.len() != 4 {
                return Err(corrupt("visibility member admission width is invalid"));
            }
            admission.push(LayeredObjectMember::new(
                u16::from_be_bytes([value[0], value[1]]),
                u16::from_be_bytes([value[2], value[3]]),
            ));
        } else if !value.is_empty() {
            return Err(corrupt("unexpected visibility member admission"));
        }
    }
    let mut refs = BTreeMap::new();
    let mut transitions = BTreeMap::new();
    let mut history = BTreeMap::new();
    for (key, value) in rows(
        body,
        directory.refs.as_ref(),
        Kind::VisibilityRefs,
        &mut audit,
    )? {
        let entry: RefRecord = record(&value)?;
        if key.as_slice() != blake3::hash(entry.name.as_bytes()).as_bytes() {
            return Err(corrupt("visibility ref key does not match its name"));
        }
        validate_visibility_ref(&entry.name)?;
        let closure = positions(body, &entry.closure, objects.len(), &mut audit)?;
        let recent = restore_transitions(
            body,
            entry.transitions.as_ref(),
            objects.len(),
            64,
            &mut audit,
        )?;
        let complete = restore_transitions(
            body,
            entry.history.as_ref(),
            objects.len(),
            MAX_LAYERED_VISIBILITY_TRANSITIONS,
            &mut audit,
        )?;
        if refs.insert(entry.name.clone(), closure).is_some() {
            return Err(corrupt("visibility refs repeat a name"));
        }
        if !entry.has_transitions && entry.transitions.is_some()
            || !entry.has_history && entry.history.is_some()
        {
            return Err(corrupt(
                "visibility history presence differs from its roots",
            ));
        }
        if entry.has_transitions {
            transitions.insert(entry.name.clone(), recent);
        }
        if entry.has_history {
            history.insert(entry.name, complete);
        }
    }
    let snapshot = LayeredVisibilitySnapshot {
        catalog_digest: directory.catalog_digest,
        objects,
        member_admission: admission,
        refs,
        transitions,
        incremental_history: history,
    };
    snapshot.validate()?;
    if encode(&snapshot)?.as_ref() != bytes {
        return Err(corrupt("visibility page encoding is not canonical"));
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn fixture(count: u32) -> LayeredVisibilitySnapshot {
        let objects = (0..count)
            .map(|ordinal| {
                let mut oid = [0; 20];
                oid[16..].copy_from_slice(&ordinal.to_be_bytes());
                oid
            })
            .collect::<Vec<_>>();
        LayeredVisibilitySnapshot {
            catalog_digest: "a".repeat(64),
            objects,
            member_admission: Some(vec![LayeredObjectMember::new(0, 0); count as usize]),
            refs: BTreeMap::from([
                ("refs/heads/main".into(), (0..count).collect()),
                (
                    "refs/heads/sparse".into(),
                    (0..count).step_by(4097).collect(),
                ),
                ("refs/heads/empty".into(), vec![]),
            ]),
            transitions: BTreeMap::from([("refs/heads/empty".into(), vec![])]),
            incremental_history: BTreeMap::new(),
        }
    }

    #[test]
    fn paged_visibility_preserves_dense_sparse_empty_and_member_presence() {
        for count in [0, 1, 4095, 4096, 4097, 100_000] {
            for admission in [true, false] {
                let mut snapshot = fixture(count);
                if !admission {
                    snapshot.member_admission = None;
                }
                let bytes = encode(&snapshot).unwrap();
                assert_eq!(decode(&bytes).unwrap(), snapshot);
            }
        }
    }

    #[test]
    fn paged_visibility_preserves_recent_and_incremental_history() {
        let mut snapshot = fixture(10_000);
        let entries = vec![GitVisibilityOrdinalTransition {
            from_ordinal: 3,
            to_ordinal: 9999,
            objects: vec![3, 9999],
        }];
        snapshot
            .transitions
            .insert("refs/heads/main".into(), entries.clone());
        snapshot
            .incremental_history
            .insert("refs/heads/main".into(), entries);
        assert_eq!(decode(&encode(&snapshot).unwrap()).unwrap(), snapshot);
    }

    #[test]
    fn paged_visibility_rejects_corruption_truncation_and_retired_streams() {
        let bytes = encode(&fixture(200)).unwrap();
        for end in [0, 7, 11, 12, bytes.len() - 1] {
            assert!(decode(&bytes[..end]).is_err());
        }
        let header = scalar(&bytes[8..12]).unwrap() as usize;
        let mut corrupt_body = bytes.to_vec();
        corrupt_body[HEADER_BYTES + header + 14] ^= 1;
        assert!(decode(&corrupt_body).is_err());
        assert!(decode(b"CRBVORD1\0\0\0\x02").is_err());
    }

    #[test]
    fn paged_visibility_rejects_unclaimed_body_and_excessive_directory_counts() {
        let mut bytes = encode(&fixture(200)).unwrap().to_vec();
        let size = scalar(&bytes[8..12]).unwrap() as usize;
        let mut directory: Directory = record(&bytes[HEADER_BYTES..HEADER_BYTES + size]).unwrap();
        directory.objects.as_mut().unwrap().count = u32::MAX;
        assert!(directory.validate().is_err());
        bytes.push(0);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn closure_blocks_reject_authenticated_invalid_coverage() {
        let mut bitmap = vec![0; BLOCK_BYTES + 1];
        bitmap[0] = 1;
        bitmap[1] = 2;
        for (value, count, object_count) in [
            (vec![0, 0, 0], 2, 2),
            (vec![0, 0, 0, 0, 0], 2, 2),
            (vec![0, 16, 0], 1, 5000),
            (vec![0, 0, 1], 1, 1),
            (vec![0], 1, 2),
            (bitmap, 1, 1),
        ] {
            let mut writer = Writer::default();
            let root = writer
                .tree(
                    Kind::VisibilityClosures,
                    vec![(0u32.to_be_bytes().to_vec(), value)],
                )
                .unwrap();
            let closure = Closure {
                count,
                root: Some(root),
            };
            assert!(
                super::positions(&writer.bytes, &closure, object_count, &mut BTreeMap::new())
                    .is_err()
            );
        }
    }

    #[test]
    fn closure_blocks_keep_dense_coverage_compact() {
        let mut writer = Writer::default();
        let positions = (0..1_000_000).collect::<Vec<_>>();
        let encoded = closure(&mut writer, &positions).unwrap();
        assert!(writer.bytes.len() < 160 * 1024);
        assert_eq!(
            super::positions(
                &writer.bytes,
                &encoded,
                positions.len(),
                &mut BTreeMap::new()
            )
            .unwrap(),
            positions
        );
    }

    proptest::proptest! {
        #[test]
        fn closure_blocks_preserve_arbitrary_sparse_sets(values in proptest::collection::btree_set(0u32..50_000, 0..1000)) {
            let positions = values.into_iter().collect::<Vec<_>>();
            let mut writer = Writer::default();
            let encoded = closure(&mut writer, &positions).unwrap();
            proptest::prop_assert_eq!(super::positions(&writer.bytes, &encoded, 50_000, &mut BTreeMap::new()).unwrap(), positions);
        }
    }
}
