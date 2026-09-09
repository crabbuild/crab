//! Git object preparation shared by remote mutation callers.

use std::future::Future;

use gix_hash::ObjectId;
use gix_object::{Kind, WriteTo, tree};

/// Failures reading, encoding or hashing Git objects during remote preparation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("repository read failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("Git object decoding failed")]
    Decode(#[from] gix_object::decode::Error),
    #[error("Git object encoding failed")]
    Io(#[from] std::io::Error),
    #[error("Git object hashing failed")]
    Hash(#[from] gix_hash::hasher::Error),
    #[error("invalid Git commit identity: {0}")]
    Identity(&'static str),
    #[error("invalid tree edit: {0}")]
    Edit(&'static str),
}

/// Exact identity and minute-resolution timestamp for a commit header.
#[derive(Clone, Copy, Debug)]
pub struct Signature<'a> {
    pub name: &'a str,
    pub email: &'a str,
    pub seconds: i64,
    pub offset_minutes: i16,
}

impl Signature<'_> {
    fn validate(&self) -> Result<(), Error> {
        if [self.name, self.email].iter().any(|value| {
            value
                .bytes()
                .any(|byte| matches!(byte, b'<' | b'>' | b'\n' | b'\r' | 0))
        }) {
            return Err(Error::Identity("name or email contains a header delimiter"));
        }
        // Git's raw timestamp uses two hour digits and two minute digits.
        if self.offset_minutes.unsigned_abs() > 99 * 60 + 59 {
            return Err(Error::Identity("timezone offset exceeds four digits"));
        }
        Ok(())
    }

    fn write(&self, output: &mut impl std::io::Write) -> Result<(), Error> {
        let offset = self.offset_minutes.unsigned_abs();
        let sign = if self.offset_minutes < 0 { '-' } else { '+' };
        writeln!(
            output,
            "{} <{}> {} {sign}{:02}{:02}",
            self.name,
            self.email,
            self.seconds,
            offset / 60,
            offset % 60
        )?;
        Ok(())
    }
}

/// Encode a commit with exact parent order, independent identities and message bytes.
///
/// Names and emails cannot contain header delimiters. Offsets must fit Git's
/// signed four-digit format. Callers bound the parent list and message before
/// encoding and validate graph connectivity before publication. No newline is
/// appended to the message, and no identity normalization is performed.
pub fn encode_commit(
    tree: ObjectId,
    parents: &[ObjectId],
    author: Signature<'_>,
    committer: Signature<'_>,
    message: &[u8],
) -> Result<Vec<u8>, Error> {
    use std::io::Write;
    author.validate()?;
    committer.validate()?;
    let mut output = Vec::new();
    writeln!(output, "tree {tree}")?;
    for parent in parents {
        writeln!(output, "parent {parent}")?;
    }
    output.write_all(b"author ")?;
    author.write(&mut output)?;
    output.write_all(b"committer ")?;
    committer.write(&mut output)?;
    output.write_all(b"\n")?;
    output.write_all(message)?;
    Ok(output)
}

/// Read and decode a tree through the caller's budgeted remote operation.
///
/// The caller owns operation completion and cancellation; non-tree objects fail.
pub async fn read_tree(
    operation: &crab_remote_git::OperationContext,
    oid: ObjectId,
) -> Result<Vec<tree::Entry>, Error> {
    let object = operation.read_object(oid).await?;
    if object.kind != Kind::Tree {
        return Err(crab_remote_git::Error::InternalInvariant {
            invariant: "commit tree path resolved to a non-tree object",
        }
        .into());
    }
    gix_object::TreeRef::from_bytes(&object.data, gix_hash::Kind::Sha1)
        .map(gix_object::TreeRef::into_owned)
        .map(|tree| tree.entries)
        .map_err(Error::from)
}

/// Non-overlapping edits applied together against one immutable base tree.
///
/// Callers bound edit count, path depth and output bytes, and validate names and
/// publication policy. File bodies remain outside this structure.
#[derive(Default)]
pub struct TreeEdits {
    children: std::collections::BTreeMap<Vec<u8>, TreeChange>,
}

trait TreeSource {
    fn read(
        &mut self,
        oid: ObjectId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<tree::Entry>, Error>> + Send + '_>>;
}

struct OperationTreeSource<'a>(&'a crab_remote_git::OperationContext);

impl TreeSource for OperationTreeSource<'_> {
    fn read(
        &mut self,
        oid: ObjectId,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<tree::Entry>, Error>> + Send + '_>> {
        Box::pin(read_tree(self.0, oid))
    }
}

enum TreeChange {
    Directory(TreeEdits),
    Entry(Option<(tree::EntryMode, ObjectId)>),
}

impl TreeEdits {
    /// Insert a replacement or deletion, rejecting root, duplicate and overlapping paths.
    pub fn insert(
        &mut self,
        path: &crab_remote_git::GitPath,
        replacement: Option<(tree::EntryMode, ObjectId)>,
    ) -> Result<(), Error> {
        use std::collections::btree_map::Entry;
        let components = path.components().collect::<Vec<_>>();
        let (leaf, directories) = components
            .split_last()
            .ok_or(Error::Edit("root cannot be replaced"))?;
        let mut current = self;
        for component in directories {
            let child = current
                .children
                .entry(component.to_vec())
                .or_insert_with(|| TreeChange::Directory(Self::default()));
            let TreeChange::Directory(directory) = child else {
                return Err(Error::Edit("edit paths overlap"));
            };
            current = directory;
        }
        match current.children.entry(leaf.to_vec()) {
            Entry::Vacant(entry) => {
                entry.insert(TreeChange::Entry(replacement));
            }
            Entry::Occupied(_) => return Err(Error::Edit("edit paths overlap")),
        }
        Ok(())
    }

    /// Rebuild changed ancestors once, retaining private output and pruning empty directories.
    ///
    /// Reads use the operation budget. Callers own completion and must discard
    /// output on failure. An unchanged empty edit set returns the original root.
    pub async fn apply(
        &self,
        operation: &crab_remote_git::OperationContext,
        root: ObjectId,
        objects: &mut Vec<(Kind, Vec<u8>)>,
    ) -> Result<ObjectId, Error> {
        if self.children.is_empty() {
            return Ok(root);
        }
        let mut source = OperationTreeSource(operation);
        self.apply_from(&mut source, root, objects).await
    }

    /// Build a root tree for an initial commit without reading remote objects.
    pub async fn apply_to_empty(
        &self,
        objects: &mut Vec<(Kind, Vec<u8>)>,
    ) -> Result<ObjectId, Error> {
        struct Empty;
        impl TreeSource for Empty {
            fn read(
                &mut self,
                _oid: ObjectId,
            ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<tree::Entry>, Error>> + Send + '_>>
            {
                Box::pin(async { Err(Error::Edit("initial tree attempted a remote read")) })
            }
        }
        self.build(&mut Empty, None, objects)
            .await?
            .map_or_else(|| encode_tree(Vec::new(), objects), Ok)
    }

    async fn apply_from(
        &self,
        source: &mut (impl TreeSource + Send),
        root: ObjectId,
        objects: &mut Vec<(Kind, Vec<u8>)>,
    ) -> Result<ObjectId, Error> {
        self.build(source, Some(root), objects)
            .await?
            .map_or_else(|| encode_tree(Vec::new(), objects), Ok)
    }

    async fn build(
        &self,
        source: &mut (impl TreeSource + Send),
        current: Option<ObjectId>,
        objects: &mut Vec<(Kind, Vec<u8>)>,
    ) -> Result<Option<ObjectId>, Error> {
        let entries = match current {
            Some(oid) => source.read(oid).await?,
            None => Vec::new(),
        };
        let mut by_name = std::collections::BTreeMap::new();
        for entry in entries {
            if by_name
                .insert(entry.filename.as_slice().to_vec(), entry)
                .is_some()
            {
                return Err(Error::Edit("base tree contains duplicate names"));
            }
        }
        let mut entries = by_name;
        for (name, change) in &self.children {
            let replacement = match change {
                TreeChange::Entry(replacement) => {
                    if replacement.is_none() && !entries.contains_key(name) {
                        return Err(Error::Edit("deleted path does not exist"));
                    }
                    *replacement
                }
                TreeChange::Directory(children) => {
                    let existing = match entries.get(name) {
                        Some(entry) if entry.mode.is_tree() => Some(entry.oid),
                        Some(_) => return Err(Error::Edit("parent component is not a tree")),
                        None => None,
                    };
                    // One descent per changed directory avoids rereading shared
                    // ancestors or looking up newly encoded trees in remote storage.
                    Box::pin(children.build(source, existing, objects))
                        .await?
                        .map(|oid| (tree::EntryKind::Tree.into(), oid))
                }
            };
            match replacement {
                Some((mode, oid)) => {
                    entries.insert(
                        name.clone(),
                        tree::Entry {
                            mode,
                            oid,
                            filename: name.clone().into(),
                        },
                    );
                }
                None => {
                    entries.remove(name);
                }
            }
        }
        if entries.is_empty() {
            return Ok(None);
        }
        encode_tree(entries.into_values().collect(), objects).map(Some)
    }
}

/// Sort and encode tree entries, retaining the encoded object for publication.
///
/// Callers validate entry names, uniqueness and object identities and bound the
/// entry collection before calling; this function neither reads nor publishes.
pub fn encode_tree(
    mut entries: Vec<tree::Entry>,
    objects: &mut Vec<(Kind, Vec<u8>)>,
) -> Result<ObjectId, Error> {
    entries.sort();
    let tree = gix_object::Tree { entries };
    let mut bytes = Vec::new();
    tree.write_to(&mut bytes)?;
    let oid = object_id(Kind::Tree, &bytes)?;
    objects.push((Kind::Tree, bytes));
    Ok(oid)
}

/// Compute the canonical SHA-1 identity of a Git object body.
pub fn object_id(kind: Kind, bytes: &[u8]) -> Result<ObjectId, gix_hash::hasher::Error> {
    gix_object::compute_hash(gix_hash::Kind::Sha1, kind, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_edits_reject_overlapping_paths_in_either_order() {
        for paths in [["a", "a/b"], ["a/b", "a"], ["a/b", "a/b"]] {
            let mut edits = TreeEdits::default();
            let first = crab_remote_git::GitPath::new(paths[0].as_bytes().to_vec()).unwrap();
            let second = crab_remote_git::GitPath::new(paths[1].as_bytes().to_vec()).unwrap();
            edits.insert(&first, None).unwrap();
            assert!(matches!(edits.insert(&second, None), Err(Error::Edit(_))));
        }
    }

    #[test]
    fn tree_edits_share_ancestors_without_merging_distinct_byte_names() {
        let mut edits = TreeEdits::default();
        for path in [b"a/b".as_slice(), b"a/c", b"a/\xff", b"ab/c"] {
            let path = crab_remote_git::GitPath::new(path.to_vec()).unwrap();
            edits.insert(&path, None).unwrap();
        }
        assert!(matches!(
            edits.insert(&crab_remote_git::GitPath::root(), None),
            Err(Error::Edit(_))
        ));
    }

    #[tokio::test]
    async fn tree_edits_rebuild_shared_ancestors_once_and_preserve_exact_names() {
        struct Source {
            trees: std::collections::BTreeMap<ObjectId, Vec<tree::Entry>>,
            reads: Vec<ObjectId>,
        }
        impl TreeSource for Source {
            fn read(
                &mut self,
                oid: ObjectId,
            ) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<tree::Entry>, Error>> + Send + '_>>
            {
                self.reads.push(oid);
                let result = self
                    .trees
                    .get(&oid)
                    .cloned()
                    .ok_or(Error::Edit("test tree missing"));
                Box::pin(async move { result })
            }
        }
        fn entry(name: &[u8], kind: tree::EntryKind, oid: ObjectId) -> tree::Entry {
            tree::Entry {
                mode: kind.into(),
                filename: name.to_vec().into(),
                oid,
            }
        }
        let old_b = ObjectId::from([1; 20]);
        let old_remove = ObjectId::from([2; 20]);
        let stable = ObjectId::from([3; 20]);
        let mut base_objects = Vec::new();
        let a = encode_tree(
            vec![
                entry(b"b", tree::EntryKind::Blob, old_b),
                entry(b"remove", tree::EntryKind::Blob, old_remove),
            ],
            &mut base_objects,
        )
        .unwrap();
        let root = encode_tree(
            vec![
                entry(b"a", tree::EntryKind::Tree, a),
                entry(b"stable", tree::EntryKind::BlobExecutable, stable),
            ],
            &mut base_objects,
        )
        .unwrap();
        let trees = base_objects
            .into_iter()
            .map(|(_, bytes)| {
                let oid = object_id(Kind::Tree, &bytes).unwrap();
                let entries = gix_object::TreeRef::from_bytes(&bytes, gix_hash::Kind::Sha1)
                    .unwrap()
                    .into_owned()
                    .entries;
                (oid, entries)
            })
            .collect();
        let mut source = Source {
            trees,
            reads: Vec::new(),
        };
        let new_b = ObjectId::from([4; 20]);
        let new_c = ObjectId::from([5; 20]);
        let new_sibling = ObjectId::from([6; 20]);
        let mut edits = TreeEdits::default();
        for (path, replacement) in [
            (
                b"a/b".as_slice(),
                Some((tree::EntryKind::Blob.into(), new_b)),
            ),
            (b"a/c", Some((tree::EntryKind::Blob.into(), new_c))),
            (b"a/remove", None),
            (b"ab/c", Some((tree::EntryKind::Blob.into(), new_sibling))),
        ] {
            edits
                .insert(
                    &crab_remote_git::GitPath::new(path.to_vec()).unwrap(),
                    replacement,
                )
                .unwrap();
        }
        let mut objects = Vec::new();
        let new_root = edits
            .apply_from(&mut source, root, &mut objects)
            .await
            .unwrap();
        assert_eq!(source.reads, vec![root, a]);
        let encoded: std::collections::BTreeMap<_, _> = objects
            .iter()
            .map(|(_, bytes)| (object_id(Kind::Tree, bytes).unwrap(), bytes))
            .collect();
        let root_tree =
            gix_object::TreeRef::from_bytes(encoded[&new_root], gix_hash::Kind::Sha1).unwrap();
        let root_entries = root_tree.entries;
        assert_eq!(
            root_entries
                .iter()
                .map(|entry| (entry.filename.as_ref(), entry.mode.kind()))
                .collect::<Vec<_>>(),
            vec![
                (b"a".as_slice(), tree::EntryKind::Tree),
                (b"ab".as_slice(), tree::EntryKind::Tree),
                (b"stable".as_slice(), tree::EntryKind::BlobExecutable),
            ]
        );
        let a_oid = root_entries[0].oid.to_owned();
        let a_tree =
            gix_object::TreeRef::from_bytes(encoded[&a_oid], gix_hash::Kind::Sha1).unwrap();
        assert_eq!(
            a_tree
                .entries
                .into_iter()
                .map(|entry| (entry.filename.to_vec(), entry.oid.to_owned()))
                .collect::<Vec<_>>(),
            vec![(b"b".to_vec(), new_b), (b"c".to_vec(), new_c)]
        );
    }

    #[test]
    fn commit_preserves_independent_signatures_parent_order_and_message() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let tree = ObjectId::from([1; 20]);
        let parents = [ObjectId::from([2; 20]), ObjectId::from([3; 20])];
        let author = Signature {
            name: "Zoë",
            email: "author@example.test",
            seconds: 1_234_567_890,
            offset_minutes: -210,
        };
        let committer = Signature {
            name: "Committer",
            email: "committer@example.test",
            seconds: 1_234_567_999,
            offset_minutes: 345,
        };
        let message = b"subject\n\nbody without trailing newline";
        let bytes = encode_commit(tree, &parents, author, committer, message).unwrap();
        let decoded = gix_object::CommitRef::from_bytes(&bytes, gix_hash::Kind::Sha1).unwrap();
        let decoded_author = decoded.author().unwrap();
        let decoded_committer = decoded.committer().unwrap();
        assert_eq!(
            (
                decoded.tree(),
                decoded.parents().collect::<Vec<_>>(),
                decoded.message.as_ref()
            ),
            (tree, parents.to_vec(), message.as_slice())
        );
        for (actual, expected) in [(decoded_author, author), (decoded_committer, committer)] {
            let time = actual.time().unwrap();
            assert_eq!(
                (
                    actual.name.as_ref(),
                    actual.email.as_ref(),
                    time.seconds,
                    time.offset
                ),
                (
                    expected.name.as_bytes(),
                    expected.email.as_bytes(),
                    expected.seconds,
                    i32::from(expected.offset_minutes) * 60
                )
            );
        }
        let mut child = Command::new("git")
            .args(["hash-object", "-t", "commit", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            object_id(Kind::Commit, &bytes).unwrap().to_string()
        );
    }

    #[test]
    fn commit_rejects_identity_header_injection_and_unrepresentable_offsets() {
        let identity = Signature {
            name: "Name",
            email: "email@example.test",
            seconds: 0,
            offset_minutes: 0,
        };
        for invalid in [
            Signature {
                name: "Name\nparent injected",
                ..identity
            },
            Signature {
                email: "email> injected",
                ..identity
            },
            Signature {
                name: "Name\0",
                ..identity
            },
            Signature {
                email: "email\r",
                ..identity
            },
            Signature {
                offset_minutes: i16::MIN,
                ..identity
            },
        ] {
            for (author, committer) in [(invalid, identity), (identity, invalid)] {
                assert!(matches!(
                    encode_commit(ObjectId::from([1; 20]), &[], author, committer, b"message"),
                    Err(Error::Identity(_))
                ));
            }
        }
    }
}
