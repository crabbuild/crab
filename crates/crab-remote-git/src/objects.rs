use bytes::Bytes;
use gix_hash::ObjectId;
use std::cmp::Ordering;

use crate::reader::GitObject;
use crate::{CorruptionStage, Error, GitPath, Result};

/// Exact supported Git tree-entry modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryMode {
    Tree,
    Regular,
    Executable,
    Symlink,
    Submodule,
}

impl EntryMode {
    /// Return the canonical raw Git mode.
    #[must_use]
    pub const fn raw(self) -> u32 {
        match self {
            Self::Tree => 0o040000,
            Self::Regular => 0o100644,
            Self::Executable => 0o100755,
            Self::Symlink => 0o120000,
            Self::Submodule => 0o160000,
        }
    }

    /// Return the semantic entry kind.
    #[must_use]
    pub const fn kind(self) -> EntryKind {
        match self {
            Self::Tree => EntryKind::Tree,
            Self::Regular | Self::Executable => EntryKind::Blob,
            Self::Symlink => EntryKind::Symlink,
            Self::Submodule => EntryKind::Submodule,
        }
    }
}

impl TryFrom<u32> for EntryMode {
    type Error = Error;

    fn try_from(raw: u32) -> Result<Self> {
        match raw {
            0o040000 => Ok(Self::Tree),
            0o100644 => Ok(Self::Regular),
            0o100755 => Ok(Self::Executable),
            0o120000 => Ok(Self::Symlink),
            0o160000 => Ok(Self::Submodule),
            _ => Err(Error::InvalidTreeMode { raw }),
        }
    }
}

/// Semantic kind represented by a tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    Tree,
    Blob,
    Symlink,
    Submodule,
}

/// Owned Git actor identity and timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    /// Actor name bytes.
    pub name: Bytes,
    /// Actor email bytes without angle brackets.
    pub email: Bytes,
    /// Seconds since the Unix epoch.
    pub seconds: i64,
    /// Signed UTC offset in seconds.
    pub offset_seconds: i32,
}

/// Preserved multi-line cryptographic-signature header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHeader {
    /// Header name such as `gpgsig`.
    pub name: Bytes,
    /// Exact unfolded header value bytes.
    pub value: Bytes,
}

/// Verified, owned commit metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Verified commit object ID.
    pub oid: ObjectId,
    /// Root tree object ID.
    pub tree: ObjectId,
    /// Parent commit IDs in stored order.
    pub parents: Vec<ObjectId>,
    /// Author identity and time.
    pub author: Signature,
    /// Committer identity and time.
    pub committer: Signature,
    /// Optional declared message encoding.
    pub encoding: Option<Bytes>,
    /// Exact commit message bytes.
    pub message: Bytes,
    /// Preserved cryptographic-signature headers.
    pub signature_headers: Vec<SignatureHeader>,
}

/// Verified, owned annotated-tag metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotatedTag {
    /// Verified tag object ID.
    pub oid: ObjectId,
    /// Direct target object ID.
    pub target: ObjectId,
    /// Declared direct target kind.
    pub target_kind: gix_object::Kind,
    /// Exact tag name bytes.
    pub name: Bytes,
    /// Optional tagger identity and time.
    pub tagger: Option<Signature>,
    /// Exact tag message bytes.
    pub message: Bytes,
    /// Preserved detached signature bytes, when present.
    pub signature: Option<Bytes>,
}

/// One validated immediate entry from a Git tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Full repository path to this immediate entry.
    pub path: crate::GitPath,
    /// Referenced object ID.
    pub oid: ObjectId,
    /// Exact supported Git mode.
    pub mode: EntryMode,
    /// Semantic entry kind.
    pub kind: EntryKind,
    /// Blob size when explicitly requested and read.
    pub size: Option<u64>,
}

/// Metadata for one verified Git blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobMetadata {
    /// Verified blob object ID.
    pub oid: ObjectId,
    /// Git representation size in bytes.
    pub git_size: u64,
    /// Logical materialized size, known for ordinary blobs and valid pointers.
    pub logical_size: Option<u64>,
    /// Exact mode of the tree entry selecting this blob.
    pub mode: EntryMode,
    /// Semantic kind of the selected tree entry.
    pub kind: EntryKind,
    /// Classification of the committed Git representation.
    pub classification: ContentClassification,
}

/// Classification of bytes stored in a Git blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentClassification {
    /// Ordinary Git content whose representation is the logical content.
    OrdinaryGit,
    /// A valid Crab pointer declaring separately stored logical content.
    CrabPointer,
    /// A valid Git LFS pointer declaring separately stored logical content.
    LfsPointer,
}

/// A verified ordinary Git blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    /// Verified object identity and size.
    pub metadata: BlobMetadata,
    /// Exact Git representation bytes.
    pub bytes: Bytes,
}

pub(crate) fn parse_commit(object: &GitObject) -> Result<Commit> {
    require_kind(object, gix_object::Kind::Commit)?;
    let parsed = gix_object::CommitRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(|source| Error::CommitParse {
            oid: object.oid,
            source,
        })?
        .into_owned()
        .map_err(|source| Error::CommitParse {
            oid: object.oid,
            source,
        })?;
    let author = Signature {
        name: Bytes::copy_from_slice(&parsed.author.name),
        email: Bytes::copy_from_slice(&parsed.author.email),
        seconds: parsed.author.time.seconds,
        offset_seconds: parsed.author.time.offset,
    };
    let committer = Signature {
        name: Bytes::copy_from_slice(&parsed.committer.name),
        email: Bytes::copy_from_slice(&parsed.committer.email),
        seconds: parsed.committer.time.seconds,
        offset_seconds: parsed.committer.time.offset,
    };
    let signature_headers = parsed
        .extra_headers
        .into_iter()
        .filter(|(name, _)| name.starts_with(b"gpgsig"))
        .map(|(name, value)| SignatureHeader {
            name: Bytes::copy_from_slice(&name),
            value: Bytes::copy_from_slice(&value),
        })
        .collect();
    Ok(Commit {
        oid: object.oid,
        tree: parsed.tree,
        parents: parsed.parents.into_vec(),
        author,
        committer,
        encoding: parsed.encoding.map(|value| Bytes::copy_from_slice(&value)),
        message: Bytes::copy_from_slice(&parsed.message),
        signature_headers,
    })
}

pub(crate) fn parse_tag(object: &GitObject) -> Result<AnnotatedTag> {
    require_kind(object, gix_object::Kind::Tag)?;
    let parsed = gix_object::TagRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map_err(|source| Error::TagParse {
            oid: object.oid,
            source,
        })?
        .into_owned()
        .map_err(|source| Error::TagParse {
            oid: object.oid,
            source,
        })?;
    let tagger = parsed.tagger.map(|tagger| Signature {
        name: Bytes::copy_from_slice(&tagger.name),
        email: Bytes::copy_from_slice(&tagger.email),
        seconds: tagger.time.seconds,
        offset_seconds: tagger.time.offset,
    });
    Ok(AnnotatedTag {
        oid: object.oid,
        target: parsed.target,
        target_kind: parsed.target_kind,
        name: Bytes::copy_from_slice(&parsed.name),
        tagger,
        message: Bytes::copy_from_slice(&parsed.message),
        signature: parsed
            .pgp_signature
            .map(|value| Bytes::copy_from_slice(&value)),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawTreeEntry {
    pub(crate) name: Bytes,
    pub(crate) oid: ObjectId,
    pub(crate) mode: EntryMode,
    pub(crate) kind: EntryKind,
}

pub(crate) fn parse_tree_raw(object: &GitObject) -> Result<Vec<RawTreeEntry>> {
    require_kind(object, gix_object::Kind::Tree)?;
    let parsed =
        gix_object::TreeRef::from_bytes(&object.data, gix_hash::Kind::Sha1).map_err(|source| {
            Error::TreeParse {
                oid: object.oid,
                source,
            }
        })?;
    if parsed
        .entries
        .windows(2)
        .any(|entries| entries[0] >= entries[1] || entries[0].filename == entries[1].filename)
    {
        return Err(Error::Corrupt {
            stage: CorruptionStage::Tree,
        });
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(parsed.entries.len())
        .map_err(|source| Error::Allocation {
            requested: parsed
                .entries
                .len()
                .saturating_mul(std::mem::size_of::<RawTreeEntry>()),
            source,
        })?;
    for entry in parsed.entries {
        let mode = EntryMode::try_from(u32::from(entry.mode.value()))?;
        entries.push(RawTreeEntry {
            name: Bytes::copy_from_slice(entry.filename.as_ref()),
            oid: entry.oid.to_owned(),
            mode,
            kind: mode.kind(),
        });
    }
    Ok(entries)
}

pub(crate) fn materialize_tree(
    entries: &[RawTreeEntry],
    parent: &GitPath,
) -> Result<Vec<TreeEntry>> {
    let mut materialized = Vec::new();
    materialized
        .try_reserve_exact(entries.len())
        .map_err(|source| Error::Allocation {
            requested: entries
                .len()
                .saturating_mul(std::mem::size_of::<TreeEntry>()),
            source,
        })?;
    for entry in entries {
        materialized.push(TreeEntry {
            path: parent.join(&entry.name)?,
            oid: entry.oid,
            mode: entry.mode,
            kind: entry.kind,
            size: None,
        });
    }
    Ok(materialized)
}

pub(crate) fn find_tree_entry(
    entries: &[RawTreeEntry],
    parent: &GitPath,
    name: &[u8],
) -> Result<(Option<TreeEntry>, u64)> {
    let mut comparisons = 0u64;
    for target_is_tree in [false, true] {
        let result = entries.binary_search_by(|entry| {
            comparisons = comparisons.saturating_add(1);
            tree_name_cmp(
                &entry.name,
                entry.kind == EntryKind::Tree,
                name,
                target_is_tree,
            )
        });
        if let Ok(index) = result {
            let entry = &entries[index];
            if entry.name.as_ref() == name {
                return Ok((
                    Some(TreeEntry {
                        path: parent.join(&entry.name)?,
                        oid: entry.oid,
                        mode: entry.mode,
                        kind: entry.kind,
                        size: None,
                    }),
                    comparisons,
                ));
            }
        }
    }
    Ok((None, comparisons))
}

fn tree_name_cmp(left: &[u8], left_is_tree: bool, right: &[u8], right_is_tree: bool) -> Ordering {
    let compared = left.len().min(right.len());
    let ordering = left[..compared].cmp(&right[..compared]);
    if ordering != Ordering::Equal {
        return ordering;
    }
    let left_end = left
        .get(compared)
        .copied()
        .unwrap_or(if left_is_tree { b'/' } else { 0 });
    let right_end = right
        .get(compared)
        .copied()
        .unwrap_or(if right_is_tree { b'/' } else { 0 });
    left_end.cmp(&right_end)
}

#[cfg(test)]
fn parse_tree(object: &GitObject, parent: &GitPath) -> Result<Vec<TreeEntry>> {
    materialize_tree(&parse_tree_raw(object)?, parent)
}

pub(crate) fn parse_blob(object: GitObject, mode: EntryMode) -> Result<Blob> {
    require_kind(&object, gix_object::Kind::Blob)?;
    let (classification, logical_size) = match crab_git::classify(&object.data) {
        crab_git::PointerKind::Crab(pointer) => {
            (ContentClassification::CrabPointer, Some(pointer.size))
        }
        crab_git::PointerKind::Lfs(pointer) => {
            (ContentClassification::LfsPointer, Some(pointer.size))
        }
        crab_git::PointerKind::NotAPointer => (
            ContentClassification::OrdinaryGit,
            Some(object.data.len() as u64),
        ),
    };
    Ok(Blob {
        metadata: BlobMetadata {
            oid: object.oid,
            git_size: object.data.len() as u64,
            logical_size,
            mode,
            kind: mode.kind(),
            classification,
        },
        bytes: object.data,
    })
}

fn require_kind(object: &GitObject, expected: gix_object::Kind) -> Result<()> {
    if object.kind == expected {
        Ok(())
    } else {
        Err(Error::ObjectKind {
            oid: object.oid,
            expected,
            actual: object.kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use sha1::{Digest, Sha1};

    use super::*;

    fn object(kind: gix_object::Kind, data: &[u8]) -> GitObject {
        let mut hasher = Sha1::new();
        hasher.update(kind.as_bytes());
        hasher.update(b" ");
        hasher.update(data.len().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update(data);
        let digest: [u8; 20] = hasher.finalize().into();
        GitObject {
            oid: ObjectId::from(digest),
            kind,
            data: Bytes::copy_from_slice(data),
        }
    }

    fn tree(entries: &[(&[u8], u32)]) -> GitObject {
        let mut bytes = Vec::new();
        for (index, (name, mode)) in entries.iter().enumerate() {
            bytes.extend_from_slice(format!("{mode:o} ").as_bytes());
            bytes.extend_from_slice(name);
            bytes.push(0);
            bytes.extend_from_slice(&[index as u8 + 1; 20]);
        }
        object(gix_object::Kind::Tree, &bytes)
    }

    #[test]
    fn exact_modes_round_trip_and_map_to_kinds() {
        for (raw, mode, kind) in [
            (0o040000, EntryMode::Tree, EntryKind::Tree),
            (0o100644, EntryMode::Regular, EntryKind::Blob),
            (0o100755, EntryMode::Executable, EntryKind::Blob),
            (0o120000, EntryMode::Symlink, EntryKind::Symlink),
            (0o160000, EntryMode::Submodule, EntryKind::Submodule),
        ] {
            let parsed = EntryMode::try_from(raw).expect("supported mode");
            assert_eq!(parsed, mode);
            assert_eq!(parsed.raw(), raw);
            assert_eq!(parsed.kind(), kind);
        }
    }

    #[test]
    fn broader_git_blob_modes_are_not_coerced() {
        assert!(matches!(
            EntryMode::try_from(0o100664),
            Err(Error::InvalidTreeMode { raw: 0o100664 })
        ));
    }

    #[test]
    fn commit_parser_preserves_root_identity_message_and_signature_header() {
        let tree_oid = "1111111111111111111111111111111111111111";
        let mut bytes = format!(
            "tree {tree_oid}\nauthor Author <author@crab.invalid> 1 +0130\ncommitter Committer <committer@crab.invalid> 2 -0230\nencoding ISO-8859-1\ngpgsig signed\n continuation\n\nmessage"
        )
        .into_bytes();
        bytes.push(0xff);
        let commit = parse_commit(&object(gix_object::Kind::Commit, &bytes)).expect("parse commit");
        assert!(commit.parents.is_empty());
        assert_eq!(commit.author.offset_seconds, 90 * 60);
        assert_eq!(commit.committer.offset_seconds, -150 * 60);
        assert_eq!(commit.encoding.as_deref(), Some(b"ISO-8859-1".as_slice()));
        assert_eq!(commit.signature_headers.len(), 1);
        assert!(commit.message.ends_with(b"\xff"));
    }

    #[test]
    fn malformed_commit_identity_preserves_parser_failure() {
        let bytes = b"tree 1111111111111111111111111111111111111111\nauthor malformed\ncommitter Committer <committer@crab.invalid> 2 +0000\n\nmessage";
        assert!(matches!(
            parse_commit(&object(gix_object::Kind::Commit, bytes)),
            Err(Error::CommitParse { .. })
        ));
    }

    #[test]
    fn typed_parser_rejects_wrong_object_kind() {
        assert!(matches!(
            parse_commit(&object(gix_object::Kind::Blob, b"blob")),
            Err(Error::ObjectKind {
                expected: gix_object::Kind::Commit,
                actual: gix_object::Kind::Blob,
                ..
            })
        ));
    }

    #[test]
    fn tree_parser_rejects_duplicate_unsorted_and_non_exact_modes() {
        for object in [
            tree(&[(b"same", 0o100644), (b"same", 0o100644)]),
            tree(&[(b"z", 0o100644), (b"a", 0o100644)]),
        ] {
            assert!(matches!(
                parse_tree(&object, &GitPath::root()),
                Err(Error::Corrupt {
                    stage: CorruptionStage::Tree
                })
            ));
        }
        assert!(matches!(
            parse_tree(&tree(&[(b"mode", 0o100664)]), &GitPath::root()),
            Err(Error::InvalidTreeMode { raw: 0o100664 })
        ));
    }

    #[test]
    fn tree_parser_preserves_non_utf8_names() {
        let entries = parse_tree(
            &tree(&[(b"a", 0o100644), (b"\xff", 0o100755)]),
            &GitPath::root(),
        )
        .expect("parse tree");
        assert_eq!(entries[1].path.as_bytes(), b"\xff");
        assert_eq!(entries[1].mode, EntryMode::Executable);
    }

    #[test]
    fn exact_tree_lookup_uses_git_directory_order() {
        let entries = parse_tree_raw(&tree(&[
            (b"foo.c", 0o100644),
            (b"foo", 0o040000),
            (b"foo0", 0o100755),
        ]))
        .expect("parse tree");
        for (name, kind) in [
            (b"foo.c".as_slice(), EntryKind::Blob),
            (b"foo".as_slice(), EntryKind::Tree),
            (b"foo0".as_slice(), EntryKind::Blob),
        ] {
            let (entry, comparisons) =
                find_tree_entry(&entries, &GitPath::root(), name).expect("find entry");
            assert_eq!(entry.expect("matching entry").kind, kind);
            assert!(comparisons <= 6);
        }
        let (entry, comparisons) =
            find_tree_entry(&entries, &GitPath::root(), b"missing").expect("missing entry");
        assert!(entry.is_none());
        assert!(comparisons <= 6);

        let entries = parse_tree_raw(&tree(&[
            (b"README.md", 0o100644),
            (b"README.md.extra", 0o100644),
        ]))
        .expect("parse interposed names");
        let (entry, _) = find_tree_entry(&entries, &GitPath::root(), b"README.md")
            .expect("find interposed file");
        assert_eq!(
            entry.expect("matching interposed file").path.as_bytes(),
            b"README.md"
        );
    }

    proptest! {
        #[test]
        fn only_exact_supported_modes_round_trip(raw in any::<u32>()) {
            let supported = [0o040000, 0o100644, 0o100755, 0o120000, 0o160000];
            match EntryMode::try_from(raw) {
                Ok(mode) => prop_assert!(supported.contains(&mode.raw())),
                Err(Error::InvalidTreeMode { raw: rejected }) => {
                    prop_assert_eq!(rejected, raw);
                    prop_assert!(!supported.contains(&raw));
                }
                Err(error) => prop_assert!(false, "unexpected error: {error}"),
            }
        }

        #[test]
        fn byte_path_order_matches_exact_bytes(
            left in proptest::collection::vec(1u8..=254, 1..32),
            right in proptest::collection::vec(1u8..=254, 1..32),
        ) {
            prop_assume!(!left.contains(&b'/') && !right.contains(&b'/'));
            let left_path = GitPath::new(Bytes::from(left.clone())).expect("left path");
            let right_path = GitPath::new(Bytes::from(right.clone())).expect("right path");
            prop_assert_eq!(left_path.cmp(&right_path), left.cmp(&right));
        }

        #[test]
        fn duplicate_tree_names_are_always_rejected(name in proptest::collection::vec(1u8..=254, 1..32)) {
            prop_assume!(!name.contains(&0) && !name.contains(&b'/'));
            let object = tree(&[(&name, 0o100644), (&name, 0o100755)]);
            let rejected = matches!(
                parse_tree(&object, &GitPath::root()),
                Err(Error::Corrupt { stage: CorruptionStage::Tree })
            );
            prop_assert!(rejected);
        }
    }
}
