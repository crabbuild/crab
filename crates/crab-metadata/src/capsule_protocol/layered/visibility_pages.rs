//! One canonical page representation for checkpoint visibility and admission.
use super::*;
use crate::pages::{Kind, PageRef, Writer};
use crate::shared_visibility as shared;

#[cfg(feature = "storage")]
mod reader;
#[cfg(feature = "storage")]
pub use reader::LayeredVisibilityReader;
#[cfg(all(test, feature = "storage"))]
mod tests;

const MAGIC: &[u8; 8] = b"CRBVPG01";
const MAX_DESCRIPTOR_BYTES: usize = 4096;
const ADMISSION_CHUNK_OBJECTS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Descriptor {
    version: u32,
    pub(super) catalog_digest: String,
    body_bytes: u64,
    body_hash: [u8; 32],
    pub(super) object_count: u32,
    member_admission: bool,
    objects: Option<PageRef>,
    refs: Option<PageRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shared: Option<shared::Descriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RefRow {
    name: String,
    objects: Option<PageRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shared: Option<shared::SetRoot>,
    recent_present: bool,
    recent: Option<PageRef>,
    history_present: bool,
    history: Option<PageRef>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionRow {
    from: [u8; 20],
    to: [u8; 20],
    objects: Option<PageRef>,
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| MetadataError::CapsuleVisibilityRecord { source })
}

fn optional_tree(
    writer: &mut Writer,
    kind: Kind,
    rows: crate::pages::Rows,
) -> Result<Option<PageRef>> {
    if rows.is_empty() {
        return Ok(None);
    }
    writer.tree(kind, rows).map(Some)
}

fn object_set(
    writer: &mut Writer,
    objects: &[[u8; 20]],
    positions: &[u32],
) -> Result<Option<PageRef>> {
    if positions.is_empty() {
        return Ok(None);
    }
    let rows = positions.iter().map(|position| {
        objects
            .get(*position as usize)
            .map(|oid| (oid.as_slice(), &[][..]))
            .ok_or_else(|| corrupt("checkpoint visibility ordinal is outside its dictionary"))
    });
    writer.tree_from_iter(Kind::CheckpointSets, rows).map(Some)
}

fn transitions(
    writer: &mut Writer,
    objects: &[[u8; 20]],
    entries: &[GitVisibilityOrdinalTransition],
) -> Result<Option<PageRef>> {
    let mut rows = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        let from = *objects
            .get(entry.from_ordinal as usize)
            .ok_or_else(|| corrupt("transition origin is absent"))?;
        let to = *objects
            .get(entry.to_ordinal as usize)
            .ok_or_else(|| corrupt("transition target is absent"))?;
        let objects = object_set(writer, objects, &entry.objects)?;
        rows.push((
            (position as u32).to_be_bytes().to_vec(),
            json(&TransitionRow { from, to, objects })?,
        ));
    }
    optional_tree(writer, Kind::CheckpointTransitions, rows)
}

pub(super) fn encode(snapshot: &LayeredVisibilitySnapshot) -> Result<Bytes> {
    snapshot.validate()?;
    let mut writer = Writer::default();
    // Preserve legacy bytes for ordinary checkpoints. Shared checkpoints bind
    // OIDs once and encode the member join as compact ordinal chunks.
    let use_shared = shared_encoding(snapshot);
    let objects = if use_shared {
        snapshot
            .member_admission
            .as_ref()
            .map(|members| {
                writer.tree_from_iter(
                    Kind::CheckpointAdmissionChunks,
                    members
                        .chunks(ADMISSION_CHUNK_OBJECTS)
                        .enumerate()
                        .map(|(index, members)| {
                            let value = members
                                .iter()
                                .flat_map(|member| {
                                    let source = member.source_index.to_be_bytes();
                                    let position = member.member_index.to_be_bytes();
                                    [source[0], source[1], position[0], position[1]]
                                })
                                .collect::<Vec<_>>();
                            Ok(((index as u32).to_be_bytes(), value))
                        }),
                )
            })
            .transpose()?
    } else if snapshot.objects.is_empty() {
        None
    } else if let Some(members) = &snapshot.member_admission {
        let dictionary = snapshot.objects.iter().zip(members).map(|(oid, member)| {
            let source = member.source_index.to_be_bytes();
            let position = member.member_index.to_be_bytes();
            Ok((oid, [source[0], source[1], position[0], position[1]]))
        });
        Some(writer.tree_from_iter(Kind::CheckpointObjects, dictionary)?)
    } else {
        let dictionary = snapshot.objects.iter().map(|oid| Ok((oid, &[][..])));
        Some(writer.tree_from_iter(Kind::CheckpointObjects, dictionary)?)
    };
    let shared_sets = if use_shared {
        Some(shared_sets(snapshot)?)
    } else {
        None
    };
    let (shared_descriptor, roots) = if let Some(sets) = &shared_sets {
        let (descriptor, roots) = shared::Prepared::new(sets.iter())?.encode(&mut writer)?;
        (Some(descriptor), roots)
    } else {
        (None, Vec::new())
    };
    let mut roots = roots.into_iter();
    let mut refs = BTreeMap::new();
    for (name, positions) in &snapshot.refs {
        let (objects, shared) = if use_shared {
            (None, roots.next().expect("one shared set per ref"))
        } else {
            (
                object_set(&mut writer, &snapshot.objects, &positions.positions())?,
                None,
            )
        };
        let recent = transitions(
            &mut writer,
            &snapshot.objects,
            snapshot.transitions.get(name).map_or(&[], Vec::as_slice),
        )?;
        let history = transitions(
            &mut writer,
            &snapshot.objects,
            snapshot
                .incremental_history
                .get(name)
                .map_or(&[], Vec::as_slice),
        )?;
        let row = RefRow {
            name: name.clone(),
            objects,
            shared,
            recent_present: snapshot.transitions.contains_key(name),
            recent,
            history_present: snapshot.incremental_history.contains_key(name),
            history,
        };
        if refs
            .insert(
                blake3::hash(name.as_bytes()).as_bytes().to_vec(),
                json(&row)?,
            )
            .is_some()
        {
            return Err(corrupt("checkpoint visibility ref hashes collide"));
        }
    }
    let refs = optional_tree(
        &mut writer,
        Kind::CheckpointRefs,
        refs.into_iter().collect(),
    )?;
    let descriptor = Descriptor {
        version: if use_shared { 2 } else { 1 },
        catalog_digest: snapshot.catalog_digest.clone(),
        body_bytes: writer.bytes.len() as u64,
        body_hash: *blake3::hash(&writer.bytes).as_bytes(),
        object_count: snapshot.objects.len() as u32,
        member_admission: snapshot.member_admission.is_some(),
        objects,
        refs,
        shared: shared_descriptor,
    };
    let header = json(&descriptor)?;
    if header.len() > MAX_DESCRIPTOR_BYTES {
        return Err(corrupt("checkpoint visibility descriptor is oversized"));
    }
    writer.bytes.extend_from_slice(&header);
    crate::pages::put_u32(&mut writer.bytes, header.len())?;
    writer.bytes.extend_from_slice(MAGIC);
    if writer.bytes.len() > MAX_LAYERED_VISIBILITY_BYTES {
        return Err(corrupt(
            "checkpoint visibility proof exceeds its byte bound",
        ));
    }
    Ok(writer.bytes.into())
}

impl Descriptor {
    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 12
            || bytes.len() > MAX_LAYERED_VISIBILITY_BYTES
            || &bytes[bytes.len() - 8..] != MAGIC
        {
            return Err(corrupt("checkpoint visibility page trailer is invalid"));
        }
        let count = u32::from_le_bytes(
            bytes[bytes.len() - 12..bytes.len() - 8]
                .try_into()
                .map_err(|_| corrupt("visibility descriptor length is truncated"))?,
        ) as usize;
        let start = bytes
            .len()
            .checked_sub(12 + count)
            .filter(|_| count <= MAX_DESCRIPTOR_BYTES)
            .ok_or_else(|| corrupt("visibility descriptor is outside its section"))?;
        let descriptor: Self = serde_json::from_slice(&bytes[start..bytes.len() - 12])
            .map_err(|source| MetadataError::CapsuleVisibilityRecord { source })?;
        descriptor.validate(bytes.len() as u64)?;
        if descriptor.body_bytes != start as u64 {
            return Err(corrupt(
                "visibility body boundary differs from its descriptor",
            ));
        }
        Ok(descriptor)
    }

    pub(super) fn validate(&self, length: u64) -> Result<()> {
        validate_content_hash(
            &self.catalog_digest,
            "checkpoint visibility catalog digest",
            "layered checkpoint",
        )?;
        if !matches!(
            (self.version, self.shared.is_some()),
            (1, false) | (2, true)
        ) || self.body_bytes > length.saturating_sub(12)
            || length > MAX_LAYERED_VISIBILITY_BYTES as u64
            || u64::from(self.object_count) > crate::git_visibility::MAX_GIT_VISIBILITY_OBJECTS
        {
            return Err(corrupt("checkpoint visibility descriptor shape is invalid"));
        }
        if let Some(shared) = &self.shared {
            shared.validate(self.body_bytes)?;
            if shared.object_count() != self.object_count as usize {
                return Err(corrupt("shared checkpoint dictionary count differs"));
            }
        }
        if self.shared.is_some() {
            let expected = if self.member_admission {
                (self.object_count as usize).div_ceil(ADMISSION_CHUNK_OBJECTS)
            } else {
                0
            };
            if self.objects.as_ref().map_or(0, |root| root.count as usize) != expected {
                return Err(corrupt("checkpoint ordinal admission coverage differs"));
            }
            validate_root(
                self.objects.as_ref(),
                Kind::CheckpointAdmissionChunks,
                self.body_bytes,
                expected as u64,
            )?;
            if let Some(root) = &self.objects
                && (root.first != 0u32.to_be_bytes()
                    || root.last != ((expected - 1) as u32).to_be_bytes())
            {
                return Err(corrupt(
                    "checkpoint admission chunk positions are incomplete",
                ));
            }
        } else {
            if self.objects.as_ref().map_or(0, |root| root.count) != self.object_count {
                return Err(corrupt("checkpoint dictionary count differs"));
            }
            validate_root(
                self.objects.as_ref(),
                Kind::CheckpointObjects,
                self.body_bytes,
                u64::from(self.object_count),
            )?;
        }
        validate_root(
            self.refs.as_ref(),
            Kind::CheckpointRefs,
            self.body_bytes,
            MAX_LAYERED_VISIBILITY_REFS as u64,
        )
    }
}

fn validate_root(root: Option<&PageRef>, kind: Kind, bytes: u64, maximum: u64) -> Result<()> {
    if let Some(root) = root {
        root.validate(kind, bytes)?;
        if u64::from(root.count) > maximum {
            return Err(corrupt("checkpoint visibility page count is oversized"));
        }
        if matches!(kind, Kind::CheckpointTransitions)
            && (root.first != 0u32.to_be_bytes() || root.last != (root.count - 1).to_be_bytes())
        {
            return Err(corrupt("checkpoint transition positions are incomplete"));
        }
    }
    Ok(())
}

impl RefRow {
    fn decode(key: &[u8], value: &[u8], descriptor: &Descriptor) -> Result<Self> {
        let row: Self = serde_json::from_slice(value)
            .map_err(|source| MetadataError::CapsuleVisibilityRecord { source })?;
        validate_visibility_ref(&row.name)?;
        if key != blake3::hash(row.name.as_bytes()).as_bytes()
            || row.name.len() > 4096
            || (!row.recent_present && row.recent.is_some())
            || (!row.history_present && row.history.is_some())
        {
            return Err(corrupt(
                "checkpoint ref row differs from its key or coverage",
            ));
        }
        if let Some(shared) = &descriptor.shared {
            if row.objects.is_some() {
                return Err(corrupt("shared checkpoint ref contains legacy objects"));
            }
            if let Some(root) = &row.shared {
                root.validate(shared, descriptor.body_bytes)?;
            }
        } else if row.shared.is_some() {
            return Err(corrupt("legacy checkpoint ref contains shared objects"));
        }
        validate_root(
            row.objects.as_ref(),
            Kind::CheckpointSets,
            descriptor.body_bytes,
            u64::from(descriptor.object_count),
        )?;
        validate_root(
            row.recent.as_ref(),
            Kind::CheckpointTransitions,
            descriptor.body_bytes,
            64,
        )?;
        validate_root(
            row.history.as_ref(),
            Kind::CheckpointTransitions,
            descriptor.body_bytes,
            MAX_LAYERED_VISIBILITY_TRANSITIONS as u64,
        )?;
        Ok(row)
    }
}

fn member(value: &[u8], admitted: bool) -> Result<Option<LayeredObjectMember>> {
    if !admitted && value.is_empty() {
        return Ok(None);
    }
    if !admitted || value.len() != 4 {
        return Err(corrupt("checkpoint member row has invalid width"));
    }
    let source_index = u16::from_be_bytes([value[0], value[1]]);
    let member_index = u16::from_be_bytes([value[2], value[3]]);
    if usize::from(source_index) >= MAX_PHYSICAL_SOURCES
        || usize::from(member_index) >= MAX_SOURCE_MEMBERS
    {
        return Err(corrupt("checkpoint member row escapes its format bounds"));
    }
    Ok(Some(LayeredObjectMember::new(source_index, member_index)))
}

pub(super) fn decode(bytes: &[u8]) -> Result<LayeredVisibilitySnapshot> {
    let descriptor = Descriptor::decode(bytes)?;
    let body = &bytes[..descriptor.body_bytes as usize];
    if blake3::hash(body).as_bytes() != &descriptor.body_hash {
        return Err(corrupt("checkpoint visibility body hash differs"));
    }
    let mut seen = BTreeMap::new();
    let mut scan = |root: Option<&PageRef>, kind| -> Result<crate::pages::Rows> {
        root.map(|root| crate::pages::scan(body, root, kind, "checkpoint visibility", &mut seen))
            .transpose()
            .map(Option::unwrap_or_default)
    };
    let mut shared_seen = BTreeMap::new();
    let mut decoded_shared = descriptor
        .shared
        .as_ref()
        .map(|shared| shared::Decoded::new(body, shared, &mut shared_seen))
        .transpose()?;
    let mut objects = Vec::with_capacity(descriptor.object_count as usize);
    let mut admission = descriptor
        .member_admission
        .then(|| Vec::with_capacity(descriptor.object_count as usize));
    if let Some(decoded) = &decoded_shared {
        objects.extend(
            decoded
                .dictionary()
                .objects()
                .iter()
                .map(|oid| crate::git_visibility::decode_oid(oid))
                .collect::<Result<Vec<_>>>()?,
        );
        if let Some(members) = admission.as_mut() {
            for (index, (key, value)) in
                scan(descriptor.objects.as_ref(), Kind::CheckpointAdmissionChunks)?
                    .into_iter()
                    .enumerate()
            {
                if key != (index as u32).to_be_bytes() {
                    return Err(corrupt("checkpoint admission chunk order differs"));
                }
                admission_chunk(&descriptor, index, &value)?;
                for row in value.as_chunks::<4>().0 {
                    members.push(member(row, true)?.expect("admitted member"));
                }
            }
        }
    } else {
        for (oid, value) in scan(descriptor.objects.as_ref(), Kind::CheckpointObjects)? {
            objects.push(
                oid.try_into()
                    .map_err(|_| corrupt("checkpoint dictionary OID width is invalid"))?,
            );
            if let Some(member) = member(&value, descriptor.member_admission)?
                && let Some(members) = admission.as_mut()
            {
                members.push(member);
            }
        }
    }
    if objects.len() != descriptor.object_count as usize {
        return Err(corrupt("checkpoint dictionary count differs"));
    }
    let ordinal = |oid: &[u8; 20]| -> Result<u32> {
        objects
            .binary_search(oid)
            .map(|index| index as u32)
            .map_err(|_| corrupt("checkpoint closure escapes its retained dictionary"))
    };
    let mut refs = BTreeMap::new();
    let mut recent = BTreeMap::new();
    let mut history = BTreeMap::new();
    let ref_rows = scan(descriptor.refs.as_ref(), Kind::CheckpointRefs)?;
    for (key, value) in ref_rows {
        let row = RefRow::decode(&key, &value, &descriptor)?;
        let mut positions = |root: Option<&PageRef>| -> Result<Vec<u32>> {
            scan(root, Kind::CheckpointSets)?
                .into_iter()
                .map(|(oid, value)| {
                    if !value.is_empty() {
                        return Err(corrupt("checkpoint closure has unexpected payload"));
                    }
                    ordinal(
                        &oid.try_into()
                            .map_err(|_| corrupt("checkpoint closure OID width is invalid"))?,
                    )
                })
                .collect()
        };
        let closure = if let (Some(decoded), Some(shared)) =
            (&mut decoded_shared, &descriptor.shared)
        {
            let set = decoded.set(body, shared, row.shared.as_ref(), &mut shared_seen)?;
            closure_from_shared(&set, &objects)?
        } else {
            GitVisibilityClosure::from_positions(positions(row.objects.as_ref())?, objects.len())?
        };
        let mut read_history =
            |root: Option<&PageRef>| -> Result<Vec<GitVisibilityOrdinalTransition>> {
                let rows = scan(root, Kind::CheckpointTransitions)?;
                rows.into_iter()
                    .enumerate()
                    .map(|(index, (key, value))| {
                        if key != (index as u32).to_be_bytes() {
                            return Err(corrupt("checkpoint transition position is invalid"));
                        }
                        let transition: TransitionRow = serde_json::from_slice(&value)
                            .map_err(|source| MetadataError::CapsuleVisibilityRecord { source })?;
                        validate_root(
                            transition.objects.as_ref(),
                            Kind::CheckpointSets,
                            descriptor.body_bytes,
                            u64::from(descriptor.object_count),
                        )?;
                        let objects = scan(transition.objects.as_ref(), Kind::CheckpointSets)?
                            .into_iter()
                            .map(|(oid, value)| {
                                if !value.is_empty() {
                                    return Err(corrupt("transition set has unexpected payload"));
                                }
                                ordinal(
                                    &oid.try_into()
                                        .map_err(|_| corrupt("transition OID width is invalid"))?,
                                )
                            })
                            .collect::<Result<_>>()?;
                        Ok(GitVisibilityOrdinalTransition {
                            from_ordinal: ordinal(&transition.from)?,
                            to_ordinal: ordinal(&transition.to)?,
                            objects,
                        })
                    })
                    .collect()
            };
        if row.recent_present {
            recent.insert(row.name.clone(), read_history(row.recent.as_ref())?);
        }
        if row.history_present {
            history.insert(row.name.clone(), read_history(row.history.as_ref())?);
        }
        if refs.insert(row.name, closure).is_some() {
            return Err(corrupt("checkpoint visibility repeats a ref"));
        }
    }
    let snapshot = LayeredVisibilitySnapshot {
        catalog_digest: descriptor.catalog_digest,
        objects,
        member_admission: admission,
        refs,
        transitions: recent,
        incremental_history: history,
    };
    snapshot.validate()?;
    if snapshot.encode()?.as_ref() != bytes {
        return Err(corrupt("checkpoint visibility pages are not canonical"));
    }
    #[cfg(feature = "storage")]
    tracing::debug!(target:"crab_metadata::index_decode", index_format="checkpoint_visibility", decoded_bytes=bytes.len() as u64);
    Ok(snapshot)
}

fn shared_encoding(snapshot: &LayeredVisibilitySnapshot) -> bool {
    snapshot.refs.values().fold(0u64, |bytes, closure| {
        bytes.saturating_add(closure.len().saturating_mul(24))
    }) > (MAX_LAYERED_VISIBILITY_BYTES / 2) as u64
}

fn shared_sets(
    snapshot: &LayeredVisibilitySnapshot,
) -> Result<Vec<crate::git_visibility::GitVisibilityObjects>> {
    use crate::git_visibility::{
        GitVisibilityDictionary, GitVisibilityObjects, VISIBILITY_CHUNK_BYTES,
    };
    let dictionary =
        GitVisibilityDictionary::new(snapshot.objects.iter().map(encode_oid).collect())?;
    let mut pool = BTreeMap::new();
    let mut sets = Vec::with_capacity(snapshot.refs.len() + 1);
    let mut retained = vec![u8::MAX; snapshot.objects.len().div_ceil(8)];
    if let Some(last) = retained.last_mut()
        && !snapshot.objects.len().is_multiple_of(8)
    {
        *last = (1 << (snapshot.objects.len() % 8)) - 1;
    }
    // This final set makes the shared dictionary cover retained, currently
    // unreachable objects. Its root is never assigned to a visible ref.
    let retained = GitVisibilityClosure::Bitmap(retained);
    for closure in snapshot.refs.values().chain(std::iter::once(&retained)) {
        if closure.len() as usize <= snapshot.objects.len().div_ceil(shared::CHUNK_BITS) {
            sets.push(GitVisibilityObjects::from_shared_ordinals(
                dictionary.clone(),
                closure.positions(),
            )?);
            continue;
        }
        let bitmap;
        let bits = match closure {
            GitVisibilityClosure::Bitmap(bits) => bits,
            GitVisibilityClosure::Sparse(positions) => {
                let mut bits = vec![0; snapshot.objects.len().div_ceil(8)];
                for position in positions {
                    bits[*position as usize / 8] |= 1 << (*position % 8);
                }
                bitmap = bits;
                &bitmap
            }
        };
        let chunks = bits
            .chunks(VISIBILITY_CHUNK_BYTES)
            .map(|bytes| {
                let mut chunk = [0; VISIBILITY_CHUNK_BYTES];
                chunk[..bytes.len()].copy_from_slice(bytes);
                pool.entry(chunk).or_insert_with(|| Arc::new(chunk)).clone()
            })
            .collect();
        sets.push(GitVisibilityObjects::from_shared_chunks(
            dictionary.clone(),
            chunks,
        )?);
    }
    Ok(sets)
}

fn closure_from_shared(
    set: &crate::git_visibility::GitVisibilityObjects,
    objects: &[[u8; 20]],
) -> Result<GitVisibilityClosure> {
    if let Some((_, chunks)) = set.shared_parts() {
        let mut bits = Vec::with_capacity(objects.len().div_ceil(8));
        bits.extend(
            chunks
                .iter()
                .flat_map(|chunk| chunk.iter().copied())
                .take(objects.len().div_ceil(8)),
        );
        let closure = GitVisibilityClosure::Bitmap(bits);
        closure.validate(objects.len())?;
        Ok(closure)
    } else {
        let positions = set
            .iter()
            .map(|oid| {
                objects
                    .binary_search(&crate::git_visibility::decode_oid(oid)?)
                    .map(|index| index as u32)
                    .map_err(|_| corrupt("shared checkpoint membership escapes its dictionary"))
            })
            .collect::<Result<_>>()?;
        GitVisibilityClosure::from_positions(positions, objects.len())
    }
}

fn admission_chunk(descriptor: &Descriptor, index: usize, value: &[u8]) -> Result<()> {
    let remaining = (descriptor.object_count as usize)
        .checked_sub(
            index
                .checked_mul(ADMISSION_CHUNK_OBJECTS)
                .ok_or_else(|| corrupt("checkpoint admission chunk index overflows"))?,
        )
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| corrupt("checkpoint admission chunk index is outside its dictionary"))?;
    if value.len() != remaining.min(ADMISSION_CHUNK_OBJECTS) * 4 {
        return Err(corrupt(
            "checkpoint admission chunk width differs from its dictionary",
        ));
    }
    Ok(())
}
