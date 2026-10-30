//! Canonical authenticated pages for one capsule visibility delta.
use super::*;
use crate::git_visibility::{
    GIT_VISIBILITY_EDIT_VERSION, MAX_GIT_VISIBILITY_INDEX_BYTES, decode_oid, encode_oid,
};
use crate::pages::{Kind, PageRef, Writer};

#[cfg(feature = "storage")]
mod reader;
#[cfg(feature = "storage")]
pub use reader::{CapsuleVisibilityReader, VisibilityChange};

const MAGIC: &[u8; 8] = b"CRBVSD01";
const TRAILER_BYTES: usize = 12;
const MAX_HEADER_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Descriptor {
    version: u32,
    body_bytes: u64,
    body_hash: [u8; 32],
    refs: PageRef,
}

/// Authenticated scalar edit metadata and opaque added/removed set roots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisibilityEditHeader {
    name: String,
    old_oid: Option<String>,
    new_oid: String,
    replaces: bool,
    added: Option<PageRef>,
    removed: Option<PageRef>,
}

pub(super) fn encode(delta: &CapsuleVisibilityDelta) -> Result<Bytes> {
    // Admit encoded OID work before allocating row buffers. Ref headers and page
    // directories are checked against the same proof-byte ceiling after encoding.
    let minimum = delta.edits.iter().try_fold(0u64, |total, (_, edit)| {
        let rows = (edit.added.len() as u64).checked_add(edit.removed.len() as u64)?;
        total.checked_add(rows.checked_mul(24)?)
    });
    if minimum.is_none_or(|bytes| bytes > MAX_GIT_VISIBILITY_INDEX_BYTES) {
        return Err(contract_error("visibility delta exceeds its byte bound"));
    }
    let mut writer = Writer::default();
    let mut refs = BTreeMap::new();
    for (name, edit) in &delta.edits {
        let added = objects(&mut writer, &edit.added)?;
        let removed = objects(&mut writer, &edit.removed)?;
        let row = VisibilityEditHeader {
            name: name.clone(),
            old_oid: edit.old_oid.clone(),
            new_oid: edit.new_oid.clone(),
            replaces: edit.replaces,
            added,
            removed,
        };
        let key = blake3::hash(name.as_bytes()).as_bytes().to_vec();
        let value = serde_json::to_vec(&row)
            .map_err(|error| MetadataError::CapsuleVisibilityRecord { source: error })?;
        if refs.insert(key, value).is_some() {
            return Err(contract_error("visibility ref-name hash collision"));
        }
    }
    let refs = writer.tree(Kind::VisibilityRefs, refs.into_iter().collect())?;
    let descriptor = Descriptor {
        version: VISIBILITY_DELTA_VERSION,
        body_bytes: writer.bytes.len() as u64,
        body_hash: *blake3::hash(&writer.bytes).as_bytes(),
        refs,
    };
    let header = serde_json::to_vec(&descriptor)
        .map_err(|error| MetadataError::CapsuleVisibilityRecord { source: error })?;
    if header.len() > MAX_HEADER_BYTES {
        return Err(contract_error(
            "visibility descriptor exceeds its byte bound",
        ));
    }
    writer.bytes.extend_from_slice(&header);
    crate::pages::put_u32(&mut writer.bytes, header.len())?;
    writer.bytes.extend_from_slice(MAGIC);
    if writer.bytes.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES {
        return Err(contract_error("visibility delta exceeds its byte bound"));
    }
    Ok(writer.bytes.into())
}

pub(super) fn decode(bytes: &[u8]) -> Result<CapsuleVisibilityDelta> {
    let descriptor = Descriptor::decode(bytes)?;
    let body = &bytes[..descriptor.body_bytes as usize];
    if blake3::hash(body).as_bytes() != &descriptor.body_hash {
        return Err(corrupt("visibility page body hash differs"));
    }
    let mut visited = BTreeMap::new();
    let refs = crate::pages::scan(
        body,
        &descriptor.refs,
        Kind::VisibilityRefs,
        "capsule visibility refs",
        &mut visited,
    )?;
    let mut edits = BTreeMap::new();
    let mut change_bytes = 0u64;
    for (key, value) in refs {
        let row = VisibilityEditHeader::decode(&key, &value, descriptor.body_bytes)?;
        let count = row.added.as_ref().map_or(0, |root| u64::from(root.count))
            + row.removed.as_ref().map_or(0, |root| u64::from(root.count));
        change_bytes = change_bytes
            .checked_add(count * 24)
            .filter(|bytes| *bytes <= MAX_GIT_VISIBILITY_INDEX_BYTES)
            .ok_or_else(|| corrupt("visibility decoded changes exceed their byte bound"))?;
        let mut read_objects = |root: Option<&PageRef>| -> Result<Vec<String>> {
            root.map(|root| {
                crate::pages::scan(
                    body,
                    root,
                    Kind::VisibilityChanges,
                    "capsule visibility objects",
                    &mut visited,
                )
            })
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(|(oid, value)| {
                if !value.is_empty() {
                    return Err(corrupt("visibility object row has unexpected payload"));
                }
                Ok(encode_oid(&oid.try_into().map_err(|_| {
                    corrupt("visibility OID has invalid length")
                })?))
            })
            .collect()
        };
        let added = read_objects(row.added.as_ref())?;
        let removed = read_objects(row.removed.as_ref())?;
        let edit = GitVisibilityEdit {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid: row.old_oid,
            new_oid: row.new_oid,
            replaces: row.replaces,
            added: added.into(),
            removed: removed.into(),
        };
        if edits.insert(row.name, edit).is_some() {
            return Err(corrupt("visibility ref repeats"));
        }
    }
    let delta = CapsuleVisibilityDelta::new(edits).map_err(as_corruption)?;
    // Rebuilding verifies canonical page placement and rejects unclaimed bytes,
    // aliases with conflicting counts, and semantically re-signed malformed data.
    if delta.encode()?.as_ref() != bytes {
        return Err(corrupt("visibility pages are not canonical"));
    }
    Ok(delta)
}

fn objects(
    writer: &mut Writer,
    oids: &crate::git_visibility::GitVisibilityObjects,
) -> Result<Option<PageRef>> {
    if oids.is_empty() {
        return Ok(None);
    }
    let rows = oids
        .iter()
        .map(|oid| Ok((decode_oid(oid)?.to_vec(), Vec::new())))
        .collect::<Result<Vec<_>>>()?;
    writer.tree(Kind::VisibilityChanges, rows).map(Some)
}

impl Descriptor {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < TRAILER_BYTES
            || bytes.len() as u64 > MAX_GIT_VISIBILITY_INDEX_BYTES
            || &bytes[bytes.len() - MAGIC.len()..] != MAGIC
        {
            return Err(corrupt("visibility page trailer or size is invalid"));
        }
        let end = bytes.len() - TRAILER_BYTES;
        let length = u32::from_le_bytes(
            bytes[end..end + 4]
                .try_into()
                .map_err(|_| corrupt("visibility descriptor length is truncated"))?,
        ) as usize;
        if length == 0 || length > MAX_HEADER_BYTES {
            return Err(corrupt("visibility descriptor length exceeds its bound"));
        }
        let start = end
            .checked_sub(length)
            .ok_or_else(|| corrupt("visibility descriptor range is invalid"))?;
        let descriptor: Self = serde_json::from_slice(&bytes[start..end])
            .map_err(|error| MetadataError::CapsuleVisibilityRecord { source: error })?;
        if descriptor.version != VISIBILITY_DELTA_VERSION || descriptor.body_bytes != start as u64 {
            return Err(corrupt(
                "visibility descriptor version or body span is invalid",
            ));
        }
        descriptor.validate_region(bytes.len() as u64)?;
        Ok(descriptor)
    }

    pub(crate) fn validate_region(&self, length: u64) -> Result<()> {
        let header = serde_json::to_vec(self)
            .map_err(|source| MetadataError::CapsuleVisibilityRecord { source })?;
        if self.version != VISIBILITY_DELTA_VERSION
            || header.len() > MAX_HEADER_BYTES
            || self
                .body_bytes
                .checked_add(header.len() as u64)
                .and_then(|n| n.checked_add(TRAILER_BYTES as u64))
                != Some(length)
            || length > MAX_GIT_VISIBILITY_INDEX_BYTES
        {
            return Err(corrupt(
                "visibility descriptor differs from its section span",
            ));
        }
        self.refs.validate(Kind::VisibilityRefs, self.body_bytes)?;
        if self.refs.count as usize > crate::git_visibility::MAX_GIT_VISIBILITY_REFS {
            return Err(corrupt("visibility delta contains too many refs"));
        }
        Ok(())
    }
}

#[cfg(feature = "storage")]
impl VisibilityEditHeader {
    /// Return the prior tip used by the edit.
    pub fn old_oid(&self) -> Option<&str> {
        self.old_oid.as_deref()
    }
    /// Return the edit's new tip.
    pub fn new_oid(&self) -> &str {
        &self.new_oid
    }
    /// Whether the added set replaces the prior closure.
    pub fn replaces(&self) -> bool {
        self.replaces
    }
    /// Return the exact authenticated added-set count.
    pub fn added_count(&self) -> u32 {
        self.added.as_ref().map_or(0, |root| root.count)
    }
    /// Return the exact authenticated removed-set count.
    pub fn removed_count(&self) -> u32 {
        self.removed.as_ref().map_or(0, |root| root.count)
    }
    /// Compare rooted addition identities without treating locations as identities.
    pub fn same_additions(&self, other: &Self) -> bool {
        match (&self.added, &other.added) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.hash == b.hash
                    && a.bytes == b.bytes
                    && a.height == b.height
                    && a.count == b.count
                    && a.first == b.first
                    && a.last == b.last
            }
            _ => false,
        }
    }
}

impl VisibilityEditHeader {
    fn decode(key: &[u8], bytes: &[u8], body_bytes: u64) -> Result<Self> {
        let row: Self = serde_json::from_slice(bytes)
            .map_err(|error| MetadataError::CapsuleVisibilityRecord { source: error })?;
        if !row.name.starts_with("refs/")
            || !valid_ref_name(&row.name)
            || blake3::hash(row.name.as_bytes()).as_bytes() != key
        {
            return Err(corrupt("visibility ref key differs from its name"));
        }
        if let Some(old) = &row.old_oid {
            decode_oid(old)?;
        }
        decode_oid(&row.new_oid)?;
        let count = row.added.as_ref().map_or(0, |root| u64::from(root.count))
            + row.removed.as_ref().map_or(0, |root| u64::from(root.count));
        if count > crate::git_visibility::MAX_GIT_VISIBILITY_OBJECTS
            || (row.replaces && (row.removed.is_some() || row.added.is_none()))
        {
            return Err(corrupt("visibility edit counts are invalid"));
        }
        for root in [&row.added, &row.removed].into_iter().flatten() {
            root.validate(Kind::VisibilityChanges, body_bytes)?;
        }
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta() -> CapsuleVisibilityDelta {
        CapsuleVisibilityDelta::new(BTreeMap::from([
            (
                "refs/heads/main".into(),
                GitVisibilityEdit::from_delta_objects(
                    Some("1".repeat(40)),
                    "2".repeat(40),
                    vec!["2".repeat(40)],
                    vec!["3".repeat(40)],
                ),
            ),
            (
                "refs/heads/alias".into(),
                GitVisibilityEdit::from_delta_objects(
                    Some("2".repeat(40)),
                    "2".repeat(40),
                    vec![],
                    vec![],
                ),
            ),
        ]))
        .unwrap()
    }

    fn wire(body: Vec<u8>, mut descriptor: Descriptor) -> Vec<u8> {
        descriptor.body_bytes = body.len() as u64;
        descriptor.body_hash = *blake3::hash(&body).as_bytes();
        let header = serde_json::to_vec(&descriptor).unwrap();
        let mut bytes = body;
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&(header.len() as u32).to_le_bytes());
        bytes.extend_from_slice(MAGIC);
        bytes
    }

    #[test]
    fn paged_visibility_preserves_added_removed_and_empty_edits() {
        let expected = delta();
        assert_eq!(decode(&encode(&expected).unwrap()).unwrap(), expected);
    }

    #[test]
    fn visibility_rejects_authenticated_unclaimed_bytes() {
        let bytes = encode(&delta()).unwrap();
        let descriptor = Descriptor::decode(&bytes).unwrap();
        let mut body = bytes[..descriptor.body_bytes as usize].to_vec();
        body.push(0);
        assert!(decode(&wire(body, descriptor)).is_err());
    }

    #[test]
    fn visibility_pages_reject_corruption_and_retired_encodings() {
        let bytes = encode(&delta()).unwrap();
        let mut damaged = bytes.to_vec();
        damaged[0] ^= 1;
        let old =
            serde_json::to_vec(&serde_json::json!({"version":2,"edits":delta().edits()})).unwrap();
        assert_eq!(
            (
                decode(&damaged).is_err(),
                decode(&old).is_err(),
                decode(&bytes[..bytes.len() - 1]).is_err()
            ),
            (true, true, true)
        );
    }

    #[test]
    fn re_signed_visibility_cannot_add_and_remove_the_same_oid() {
        let mut writer = Writer::default();
        let changes = writer
            .tree(Kind::VisibilityChanges, vec![(vec![0x22; 20], vec![])])
            .unwrap();
        let row = VisibilityEditHeader {
            name: "refs/heads/main".into(),
            old_oid: Some("1".repeat(40)),
            new_oid: "2".repeat(40),
            replaces: false,
            added: Some(changes.clone()),
            removed: Some(changes),
        };
        let refs = writer
            .tree(
                Kind::VisibilityRefs,
                vec![(
                    blake3::hash(row.name.as_bytes()).as_bytes().to_vec(),
                    serde_json::to_vec(&row).unwrap(),
                )],
            )
            .unwrap();
        let descriptor = Descriptor {
            version: VISIBILITY_DELTA_VERSION,
            body_bytes: 0,
            body_hash: [0; 32],
            refs,
        };
        assert!(decode(&wire(writer.bytes, descriptor)).is_err());
    }

    #[test]
    fn wide_ref_directory_pages_internal_routing_and_shared_changes() {
        let edit =
            GitVisibilityEdit::from_replacement_objects(None, "2".repeat(40), vec!["2".repeat(40)]);
        let expected = CapsuleVisibilityDelta::new(
            (0..2000)
                .map(|i| (format!("refs/heads/branch-{i:04}"), edit.clone()))
                .collect(),
        )
        .unwrap();
        let bytes = encode(&expected).unwrap();
        let descriptor = Descriptor::decode(&bytes).unwrap();
        assert_eq!(
            (descriptor.refs.height > 0, decode(&bytes).unwrap()),
            (true, expected)
        );
    }

    proptest::proptest! {
        #[test]
        fn visibility_change_pages_match_normalized_edits(
            added in proptest::collection::vec(2u8..64,0..100),
            removed in proptest::collection::vec(64u8..128,0..100),
        ) {
            let edit = GitVisibilityEdit::from_delta_objects(Some("1".repeat(40)), "2".repeat(40),
                added.into_iter().map(|n|format!("{n:040x}")).collect(),
                removed.into_iter().map(|n|format!("{n:040x}")).collect());
            let expected = CapsuleVisibilityDelta::new(BTreeMap::from([("refs/heads/main".into(),edit)])).unwrap();
            proptest::prop_assert_eq!(decode(&encode(&expected).unwrap()).unwrap(),expected);
        }
    }
}
