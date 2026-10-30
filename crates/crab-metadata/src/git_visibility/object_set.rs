//! Exact immutable memberships sharing a validated object dictionary and bitmap chunks.
use super::{MAX_GIT_VISIBILITY_OBJECTS, Result, corrupt, validate_sorted_oids};
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};

/// Fixed bitmap chunk width; each byte maps eight consecutive dictionary ordinals.
pub const VISIBILITY_CHUNK_BYTES: usize = 128;
const CHUNK_BITS: usize = VISIBILITY_CHUNK_BYTES * 8;

/// A sorted, validated dictionary that can be shared across one visibility transaction.
#[derive(Debug, PartialEq, Eq)]
pub struct GitVisibilityDictionary {
    objects: Vec<String>,
}

impl GitVisibilityDictionary {
    /// Validate canonical OIDs once before any memberships can borrow this dictionary.
    pub fn new(objects: Vec<String>) -> Result<Arc<Self>> {
        if objects.len() as u64 > MAX_GIT_VISIBILITY_OBJECTS {
            return Err(corrupt("visibility dictionary contains too many objects"));
        }
        validate_sorted_oids(objects.iter(), "dictionary")?;
        Ok(Arc::new(Self { objects }))
    }
}

#[derive(Debug, Clone)]
struct Shared {
    dictionary: Arc<GitVisibilityDictionary>,
    chunks: Vec<Arc<[u8; VISIBILITY_CHUNK_BYTES]>>,
    count: usize,
}

/// Exact sorted OIDs, stored as a list or immutable memberships in a shared dictionary.
///
/// Sharing never grants visibility: every set retains its own complete bitmap.
/// Serializing this value preserves the existing JSON array contract. Paged codecs
/// must explicitly encode its shared representation to avoid expanding memberships.
#[derive(Debug, Clone)]
pub struct GitVisibilityObjects(Storage);

#[derive(Debug, Clone)]
enum Storage {
    List(Vec<String>),
    Shared(Shared),
}

impl GitVisibilityObjects {
    /// Admit exact bitmap chunks against one immutable canonical dictionary.
    /// All chunks, including a padded final chunk, cover exactly that dictionary.
    pub fn from_shared_chunks(
        dictionary: Arc<GitVisibilityDictionary>,
        chunks: Vec<Arc<[u8; VISIBILITY_CHUNK_BYTES]>>,
    ) -> Result<Self> {
        let length = dictionary.objects.len();
        if chunks.len() != length.div_ceil(CHUNK_BITS) {
            return Err(corrupt("visibility bitmap does not cover its dictionary"));
        }
        if let Some(chunk) = chunks.last() {
            let used = length % CHUNK_BITS;
            if used != 0 {
                let byte = used / 8;
                let bits = used % 8;
                if (bits != 0 && chunk[byte] >> bits != 0)
                    || chunk[byte + usize::from(bits != 0)..]
                        .iter()
                        .any(|byte| *byte != 0)
                {
                    return Err(corrupt("visibility bitmap has nonzero dictionary padding"));
                }
            }
        }
        let count = chunks
            .iter()
            .flat_map(|chunk| chunk.iter())
            .map(|byte| byte.count_ones() as usize)
            .sum();
        Ok(Self(Storage::Shared(Shared {
            dictionary,
            chunks,
            count,
        })))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        match &self.0 {
            Storage::List(objects) => objects.len(),
            Storage::Shared(set) => set.count,
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate exact OIDs in canonical order without allocating per-ref strings.
    pub fn iter(&self) -> GitVisibilityObjectIter<'_> {
        GitVisibilityObjectIter(match &self.0 {
            Storage::List(objects) => Iter::List(objects.iter()),
            Storage::Shared(set) => Iter::Shared {
                set,
                byte: 0,
                pending: 0,
                remaining: set.count,
            },
        })
    }

    #[must_use]
    pub fn contains(&self, oid: &str) -> bool {
        match &self.0 {
            Storage::List(objects) => objects
                .binary_search_by(|value| value.as_str().cmp(oid))
                .is_ok(),
            Storage::Shared(set) => set
                .dictionary
                .objects
                .binary_search_by(|value| value.as_str().cmp(oid))
                .is_ok_and(|position| {
                    set.chunks[position / CHUNK_BITS][position % CHUNK_BITS / 8]
                        & (1 << (position % 8))
                        != 0
                }),
        }
    }

    /// Return the canonical set position or insertion point, as with a sorted slice.
    pub fn binary_search(&self, oid: &str) -> std::result::Result<usize, usize> {
        match &self.0 {
            Storage::List(objects) => objects.binary_search_by(|value| value.as_str().cmp(oid)),
            Storage::Shared(set) => {
                let dictionary_position = set
                    .dictionary
                    .objects
                    .binary_search_by(|value| value.as_str().cmp(oid))
                    .unwrap_or_else(|position| position);
                let before = (0..dictionary_position / 8)
                    .map(|byte| {
                        set.chunks[byte / VISIBILITY_CHUNK_BYTES][byte % VISIBILITY_CHUNK_BYTES]
                            .count_ones() as usize
                    })
                    .sum::<usize>();
                let byte = dictionary_position / 8;
                let remainder = dictionary_position % 8;
                let bits = set
                    .chunks
                    .get(byte / VISIBILITY_CHUNK_BYTES)
                    .map_or(0, |chunk| chunk[byte % VISIBILITY_CHUNK_BYTES]);
                let before =
                    before + (bits & ((1u16 << remainder) - 1) as u8).count_ones() as usize;
                if self.contains(oid) {
                    Ok(before)
                } else {
                    Err(before)
                }
            }
        }
    }

    pub(super) fn validate(&self, field: &str) -> Result<()> {
        match &self.0 {
            Storage::List(objects) => validate_sorted_oids(objects.iter(), field),
            // Construction proved dictionary order, all ordinals and padding. Both
            // the dictionary and chunks are immutable and privately retained.
            Storage::Shared(_) => Ok(()),
        }
    }

    pub(super) fn is_disjoint(&self, other: &Self) -> bool {
        if self.is_empty() || other.is_empty() {
            return true;
        }
        if let (Storage::Shared(a), Storage::Shared(b)) = (&self.0, &other.0)
            && Arc::ptr_eq(&a.dictionary, &b.dictionary)
        {
            return a
                .chunks
                .iter()
                .zip(&b.chunks)
                .all(|(a, b)| a.iter().zip(b.iter()).all(|(a, b)| a & b == 0));
        }
        // Lists may not yet be validated. Callers validate before comparing sets.
        self.iter().all(|oid| !other.contains(oid))
    }
}

impl From<Vec<String>> for GitVisibilityObjects {
    fn from(objects: Vec<String>) -> Self {
        Self(Storage::List(objects))
    }
}
impl Default for GitVisibilityObjects {
    fn default() -> Self {
        Vec::new().into()
    }
}
impl PartialEq for GitVisibilityObjects {
    fn eq(&self, other: &Self) -> bool {
        if let (Storage::Shared(a), Storage::Shared(b)) = (&self.0, &other.0)
            && Arc::ptr_eq(&a.dictionary, &b.dictionary)
        {
            return a.chunks == b.chunks;
        }
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}
impl Eq for GitVisibilityObjects {}
impl<const N: usize> PartialEq<[String; N]> for GitVisibilityObjects {
    fn eq(&self, other: &[String; N]) -> bool {
        self.iter().eq(other.iter())
    }
}

/// Borrowed iterator over exact canonical memberships.
pub struct GitVisibilityObjectIter<'a>(Iter<'a>);
enum Iter<'a> {
    List(std::slice::Iter<'a, String>),
    Shared {
        set: &'a Shared,
        byte: usize,
        pending: u8,
        remaining: usize,
    },
}
impl<'a> Iterator for GitVisibilityObjectIter<'a> {
    type Item = &'a String;
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            Iter::List(objects) => objects.next(),
            Iter::Shared {
                set,
                byte,
                pending,
                remaining,
            } => loop {
                if *pending != 0 {
                    let bit = pending.trailing_zeros() as usize;
                    *pending &= *pending - 1;
                    *remaining -= 1;
                    return set.dictionary.objects.get((*byte - 1) * 8 + bit);
                }
                let chunk = set.chunks.get(*byte / VISIBILITY_CHUNK_BYTES)?;
                *pending = chunk[*byte % VISIBILITY_CHUNK_BYTES];
                *byte += 1;
            },
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = match &self.0 {
            Iter::List(objects) => objects.len(),
            Iter::Shared { remaining, .. } => *remaining,
        };
        (remaining, Some(remaining))
    }
}
impl ExactSizeIterator for GitVisibilityObjectIter<'_> {}
impl std::iter::FusedIterator for GitVisibilityObjectIter<'_> {}
impl<'a> IntoIterator for &'a GitVisibilityObjects {
    type Item = &'a String;
    type IntoIter = GitVisibilityObjectIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}
impl Serialize for GitVisibilityObjects {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for oid in self {
            sequence.serialize_element(oid)?;
        }
        sequence.end()
    }
}
impl<'de> Deserialize<'de> for GitVisibilityObjects {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Vec::<String>::deserialize(deserializer).map(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dictionary(count: usize) -> Arc<GitVisibilityDictionary> {
        GitVisibilityDictionary::new((0..count).map(|index| format!("{index:040x}")).collect())
            .unwrap()
    }
    fn objects(
        dictionary: Arc<GitVisibilityDictionary>,
        positions: &[usize],
    ) -> GitVisibilityObjects {
        let mut chunks =
            vec![[0; VISIBILITY_CHUNK_BYTES]; dictionary.objects.len().div_ceil(CHUNK_BITS)];
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
    fn memberships_preserve_dictionary_gaps_chunk_boundaries_and_json() {
        let dictionary = dictionary(2051);
        let set = objects(dictionary.clone(), &[0, 7, 8, 1023, 1024, 2048, 2050]);
        let expected = [0, 7, 8, 1023, 1024, 2048, 2050].map(|index| format!("{index:040x}"));
        assert_eq!(set, expected);
        assert_eq!(set.len(), expected.len());
        for position in 0..2053 {
            let oid = format!("{position:040x}");
            assert_eq!(set.binary_search(&oid), expected.binary_search(&oid));
            assert_eq!(set.contains(&oid), expected.contains(&oid));
        }
        let json = serde_json::to_vec(&set).unwrap();
        assert_eq!(json, serde_json::to_vec(&expected).unwrap());
        let decoded: GitVisibilityObjects = serde_json::from_slice(&json).unwrap();
        decoded.validate("test").unwrap();
        assert_eq!(set, decoded);
        assert!(set.is_disjoint(&objects(dictionary.clone(), &[1, 2, 1025])));
        assert!(!set.is_disjoint(&objects(dictionary, &[2050])));
    }
    #[test]
    fn rejects_noncanonical_dictionary_coverage_and_padding() {
        assert!(GitVisibilityDictionary::new(vec!["f".repeat(40), "0".repeat(40)]).is_err());
        assert!(GitVisibilityDictionary::new(vec!["a".repeat(40); 2]).is_err());
        assert!(GitVisibilityDictionary::new(vec!["invalid".into()]).is_err());
        for count in [1, 7, 8, 9, 1023, 1025] {
            let dictionary = dictionary(count);
            assert!(
                GitVisibilityObjects::from_shared_chunks(dictionary.clone(), Vec::new()).is_err()
            );
            let mut chunks = vec![[0; VISIBILITY_CHUNK_BYTES]; count.div_ceil(CHUNK_BITS)];
            chunks[count / CHUNK_BITS][count % CHUNK_BITS / 8] = 1 << (count % 8);
            assert!(
                GitVisibilityObjects::from_shared_chunks(
                    dictionary,
                    chunks.into_iter().map(Arc::new).collect()
                )
                .is_err()
            );
        }
    }
    #[test]
    fn empty_and_exact_chunk_dictionaries_have_no_padding_alias() {
        let empty = objects(dictionary(0), &[]);
        assert!(empty.is_empty());
        assert_eq!(empty.iter().next(), None);
        assert_eq!(empty.binary_search(&"0".repeat(40)), Err(0));
        let full = objects(dictionary(1024), &(0..1024).collect::<Vec<_>>());
        assert_eq!(full.len(), 1024);
        assert_eq!(full.iter().count(), 1024);
        assert_eq!(full.binary_search(&"f".repeat(40)), Err(1024));
    }
    #[test]
    fn shared_chunks_are_immutable_and_each_ref_keeps_its_own_memberships() {
        let dictionary = dictionary(10);
        let mut original = Arc::new([0u8; VISIBILITY_CHUNK_BYTES]);
        Arc::get_mut(&mut original).unwrap()[0] = 1;
        let set =
            GitVisibilityObjects::from_shared_chunks(dictionary.clone(), vec![original.clone()])
                .unwrap();
        Arc::make_mut(&mut original)[0] = 2;
        assert!(set.contains(&format!("{:040x}", 0)));
        assert!(!set.contains(&format!("{:040x}", 1)));
        let other = objects(dictionary, &[1]);
        assert!(set.is_disjoint(&other));
        let mut iter = set.iter();
        assert_eq!(iter.len(), 1);
        assert!(iter.next().is_some());
        assert_eq!(iter.len(), 0);
        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
    }
    #[test]
    fn shared_edits_match_list_edits_and_reject_conflicting_directions() {
        use super::super::{GIT_VISIBILITY_EDIT_VERSION, GitVisibilityEdit};
        let dictionary = dictionary(4);
        let mut shared = GitVisibilityEdit {
            version: GIT_VISIBILITY_EDIT_VERSION,
            old_oid: Some(format!("{:040x}", 0)),
            new_oid: format!("{:040x}", 2),
            replaces: false,
            added: objects(dictionary.clone(), &[2, 3]),
            removed: objects(dictionary.clone(), &[1]),
        };
        let list = GitVisibilityEdit::from_delta_objects(
            shared.old_oid.clone(),
            shared.new_oid.clone(),
            vec![format!("{:040x}", 2), format!("{:040x}", 3)],
            vec![format!("{:040x}", 1)],
        );
        let prior = vec![format!("{:040x}", 0), format!("{:040x}", 1)];
        assert_eq!(shared, list);
        assert_eq!(
            shared.apply(Some(&prior)).unwrap(),
            list.apply(Some(&prior)).unwrap()
        );
        shared.removed = objects(dictionary, &[2]);
        assert!(
            shared
                .validate()
                .unwrap_err()
                .to_string()
                .contains("adds and removes")
        );
        let invalid: GitVisibilityObjects = serde_json::from_str(r#"["not-an-oid"]"#).unwrap();
        assert!(invalid.validate("added").is_err());
    }
    proptest::proptest! {
        #[test]
        fn shared_membership_matches_a_sorted_oid_list(positions in proptest::collection::btree_set(0usize..3500, 0..500)) {
            let set = objects(dictionary(3500), &positions.iter().copied().collect::<Vec<_>>());
            let list: GitVisibilityObjects = positions.iter().map(|position| format!("{position:040x}")).collect::<Vec<_>>().into();
            proptest::prop_assert_eq!(&set, &list);
            proptest::prop_assert_eq!(serde_json::to_vec(&set).unwrap(),serde_json::to_vec(&list).unwrap());
        }
    }
}
