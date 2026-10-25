use std::{
    collections::{HashMap, hash_map::Entry},
    sync::{Arc, LazyLock},
};

use bytes::Bytes;
use crab_git::incoming_pack::GeneratedObject;
use futures_util::future::join_all;
use gix_hash::ObjectId;
use gix_object::{Kind, bstr::BString, tree};
use tokio::sync::Semaphore;

use super::{
    Change, DirectoryState, Error, MutableTree, Mutation, Result, TreeEntryEdit,
    append_only_entries, append_tree_entry, encoded_tree_entry_bytes,
};

static HASH_WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| {
    Arc::new(Semaphore::new(
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
    ))
});

#[derive(Default)]
pub(super) struct PreparedAppends {
    blobs: HashMap<usize, GeneratedObject>,
    trees: HashMap<(ObjectId, Bytes), GeneratedObject>,
}

impl PreparedAppends {
    pub(super) fn blob(&self, bytes: &Bytes) -> Option<GeneratedObject> {
        // The cached object retains these immutable bytes, preventing address
        // reuse. Matching the start and length preserves the checked blob ID.
        self.blobs
            .get(&(bytes.as_ptr() as usize))
            .filter(|object| object.data().len() == bytes.len())
            .cloned()
    }

    pub(super) fn tree(&self, base: ObjectId, suffix: &Bytes) -> Option<GeneratedObject> {
        // Exact bytes and a verified base bind the full resulting tree, even
        // when earlier FIFO edits invalidate the speculative directory state.
        self.trees.get(&(base, suffix.clone())).cloned()
    }
}

struct AppendWork {
    base: GeneratedObject,
    inherited: Bytes,
    entry: tree::Entry,
    contents: Bytes,
}

impl AppendWork {
    fn new(state: &DirectoryState, name: &[u8], mutation: &Mutation) -> Option<Self> {
        let Change::Put {
            bytes,
            track_lfs: false,
            ..
        } = &mutation.change
        else {
            return None;
        };
        if !state.append_only_since_anchor || state.generated_in_batch {
            return None;
        }
        let base = &state.delta_anchor.as_ref()?.object;
        let entry = tree::Entry {
            mode: tree::EntryKind::Blob.into(),
            filename: BString::from(name),
            oid: ObjectId::empty_blob(gix_hash::Kind::Sha1),
        };
        let edit = TreeEntryEdit {
            name: name.to_vec(),
            replacement: Some(entry.clone()),
        };
        append_only_entries(&state.entries, &[edit])?;
        state.encoding.bytes.get(base.data().len()..)?;
        Some(Self {
            base: base.clone(),
            inherited: state.encoding.bytes.slice(base.data().len()..),
            entry,
            contents: bytes.clone(),
        })
    }

    fn run(mut self) -> Result<PreparedAppend> {
        let blob = GeneratedObject::new(Kind::Blob, self.contents)?;
        self.entry.oid = blob.oid();
        let mut suffix =
            Vec::with_capacity(self.inherited.len() + encoded_tree_entry_bytes(&self.entry));
        suffix.extend_from_slice(&self.inherited);
        append_tree_entry(&mut suffix, &self.entry);
        let suffix = Bytes::from(suffix);
        let tree = GeneratedObject::append(&self.base, &suffix)?;
        Ok(PreparedAppend {
            base: self.base.oid(),
            suffix,
            blob,
            tree,
        })
    }
}

struct PreparedAppend {
    base: ObjectId,
    suffix: Bytes,
    blob: GeneratedObject,
    tree: GeneratedObject,
}

pub(super) async fn prepare(tree: &MutableTree, mutations: &[Mutation]) -> Result<PreparedAppends> {
    let mut directories = HashMap::new();
    for mutation in mutations {
        let mut components = mutation.path.as_bytes().rsplitn(2, |byte| *byte == b'/');
        let name = components.next().unwrap_or_default();
        let directory = components.next().unwrap_or_default();
        match directories.entry(directory.to_vec()) {
            Entry::Occupied(mut entry) => {
                // Any same-directory mutation can change the prefix first.
                // Overlapping edits stay on the sequential builder.
                entry.insert(None);
            }
            Entry::Vacant(entry) => {
                let work = tree
                    .directories
                    .get(directory)
                    .and_then(|state| AppendWork::new(state, name, mutation));
                entry.insert(work);
            }
        }
    }
    let work = directories.into_values().flatten().collect::<Vec<_>>();
    if work.len() < 2 {
        return Ok(PreparedAppends::default());
    }
    // Only CPU preparation overlaps. Join every worker before handling errors;
    // the admitted batch still owns FIFO checks, commits and publication.
    let results = join_all(work.into_iter().map(|work| async move {
        let permit = Arc::clone(&HASH_WORKERS)
            .acquire_owned()
            .await
            .map_err(|_| Error::AdmissionState)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work.run()
        })
        .await
        .map_err(Error::from)
    }))
    .await;
    let mut prepared = PreparedAppends::default();
    for result in results {
        // Speculative hashes cannot reject unused work after a FIFO condition
        // fails or state changes. Canonical construction reports any relevant error.
        let Ok(append) = result? else {
            continue;
        };
        prepared
            .blobs
            .insert(append.blob.data().as_ptr() as usize, append.blob);
        prepared
            .trees
            .insert((append.base, append.suffix), append.tree);
    }
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn directory(entries: Vec<tree::Entry>) -> DirectoryState {
        let encoding = TreeEncoding::new(&entries);
        let object = GeneratedObject::new(Kind::Tree, encoding.bytes.clone()).unwrap();
        DirectoryState {
            oid: Some(object.oid()),
            entries,
            encoding,
            delta_anchor: Some(TreeDeltaAnchor {
                object,
                reused_batches: 0,
                reuse_limit: GENERATED_TREE_ANCHOR_REUSE_BATCHES,
            }),
            pending_object: None,
            delta_ancestors: Vec::new(),
            generated_in_batch: false,
            append_only_since_anchor: true,
        }
    }

    fn fixture(writers: usize, files: usize) -> WarmTree {
        let mut directories = HashMap::new();
        let mut root = Vec::new();
        for writer in 0..writers {
            let entries = (0..files)
                .map(|file| tree::Entry {
                    mode: tree::EntryKind::Blob.into(),
                    filename: format!("object-{file:08}").into(),
                    oid: ObjectId::from(
                        <[u8; 20]>::try_from(&blake3::hash(&file.to_le_bytes()).as_bytes()[..20])
                            .unwrap(),
                    ),
                })
                .collect();
            let state = directory(entries);
            let name = format!("writer-{writer:02}");
            root.push(tree::Entry {
                mode: tree::EntryKind::Tree.into(),
                filename: name.clone().into(),
                oid: state.oid.unwrap(),
            });
            directories.insert(name.into_bytes(), state);
        }
        let root = directory(root);
        let root_oid = root.oid;
        directories.insert(Vec::new(), root);
        let estimated_bytes = directories
            .iter()
            .map(|(path, state)| estimated_directory_bytes(path, state))
            .sum();
        WarmTree {
            root_oid,
            directories,
            estimated_bytes,
            ..Default::default()
        }
    }

    fn put(path: &str, bytes: Bytes, condition: PutCondition) -> Mutation {
        Mutation {
            path: crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
            change: Change::Put {
                bytes,
                track_lfs: false,
                attributes: Box::default(),
                condition,
            },
            principal: "user".to_owned(),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn independent_appends_preserve_each_commits_canonical_tree() {
        const WRITERS: usize = 16;
        const FILES: usize = 5000;
        let warm = fixture(WRITERS, FILES);
        let mutations = (0..WRITERS)
            .map(|writer| {
                put(
                    &format!("writer-{writer:02}/object-{FILES:08}"),
                    Bytes::from(format!("content-{writer}")),
                    PutCondition::None,
                )
            })
            .collect::<Vec<_>>();
        let mut expected_root = warm.directories[&Vec::new()].entries.clone();
        let mut expected = Vec::new();
        for (writer, mutation) in mutations.iter().enumerate() {
            let Change::Put { bytes, .. } = &mutation.change else {
                unreachable!()
            };
            let blob = GeneratedObject::new(Kind::Blob, bytes.clone()).unwrap();
            let mut leaf = warm.directories[format!("writer-{writer:02}").as_bytes()]
                .entries
                .clone();
            leaf.push(tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: format!("object-{FILES:08}").into(),
                oid: blob.oid(),
            });
            expected_root[writer].oid = object_id(Kind::Tree, &encode_tree(&leaf)).unwrap();
            expected.push(object_id(Kind::Tree, &encode_tree(&expected_root)).unwrap());
        }
        let batch = build_batch(
            None,
            None,
            Some(ObjectId::from([1; 20])),
            attributes::Manifest::empty_complete(),
            Some(warm),
            0,
            &mutations,
        )
        .await
        .unwrap();
        for (commit, tree) in batch.commits.iter().zip(expected) {
            let bytes = commit
                .objects
                .iter()
                .find(|object| object.oid() == commit.commit)
                .unwrap()
                .data();
            assert!(bytes.starts_with(format!("tree {tree}\n").as_bytes()));
        }
        assert_eq!(batch.commits.len(), WRITERS);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prepared_identities_require_exact_buffer_base_and_suffix() {
        let tree = MutableTree::new(None, Some(fixture(2, 1)));
        let contents = Bytes::from_static(b"contents");
        let mutations = [
            put(
                "writer-00/object-00000001",
                contents.clone(),
                PutCondition::None,
            ),
            put(
                "writer-01/object-00000002",
                contents.clone(),
                PutCondition::None,
            ),
        ];
        let prepared = super::prepare(&tree, &mutations).await.unwrap();
        let blob = prepared.blob(&contents).unwrap();
        assert_eq!(blob.oid(), object_id(Kind::Blob, &contents).unwrap());
        assert!(prepared.blob(&contents.slice(..3)).is_none());
        assert!(prepared.blob(&Bytes::copy_from_slice(&contents)).is_none());

        let state = &tree.directories[b"writer-00".as_slice()];
        let base = state.delta_anchor.as_ref().unwrap().object.oid();
        let mut suffix = Vec::new();
        append_tree_entry(
            &mut suffix,
            &tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: "object-00000001".into(),
                oid: blob.oid(),
            },
        );
        let suffix = Bytes::from(suffix);
        let cached = prepared.tree(base, &suffix).unwrap();
        let mut objects = Vec::new();
        encode_tree_with_edits(
            state,
            &[TreeEntryEdit {
                name: b"object-00000001".to_vec(),
                replacement: Some(tree::Entry {
                    mode: tree::EntryKind::Blob.into(),
                    filename: "object-00000001".into(),
                    oid: blob.oid(),
                }),
            }],
            &mut objects,
            &mut Vec::new(),
            &prepared,
        )
        .unwrap();
        assert_eq!(objects[0].data().as_ptr(), cached.data().as_ptr());
        assert!(prepared.tree(ObjectId::from([9; 20]), &suffix).is_none());
        assert!(
            prepared
                .tree(base, &suffix.slice(..suffix.len() - 1))
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn changed_leaf_state_cannot_reuse_a_stale_prepared_tree() {
        let mut tree = MutableTree::new(None, Some(fixture(2, 1)));
        let mutations = [
            put(
                "writer-00/object-00000002",
                Bytes::from_static(b"later"),
                PutCondition::None,
            ),
            put(
                "writer-01/object-00000002",
                Bytes::from_static(b"other"),
                PutCondition::None,
            ),
        ];
        tree.prepared_appends = super::prepare(&tree, &mutations).await.unwrap();
        let mut manifest = attributes::Manifest::empty_complete();
        for mutation in [
            put(
                "writer-00/object-00000001",
                Bytes::from_static(b"earlier"),
                PutCondition::None,
            ),
            mutations[0].clone(),
        ] {
            build_commit(
                &mut tree,
                None,
                None,
                &mutation.path,
                mutation.change,
                &mutation.principal,
                &mut manifest,
            )
            .await
            .unwrap();
        }
        let leaf = &tree.directories[b"writer-00".as_slice()];
        let expected = [
            fixture(1, 1).directories[b"writer-00".as_slice()].entries[0].clone(),
            tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: "object-00000001".into(),
                oid: object_id(Kind::Blob, b"earlier").unwrap(),
            },
            tree::Entry {
                mode: tree::EntryKind::Blob.into(),
                filename: "object-00000002".into(),
                oid: object_id(Kind::Blob, b"later").unwrap(),
            },
        ];
        assert_eq!(
            leaf.pending_object.as_ref().unwrap().data(),
            encode_tree(&expected)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn speculative_appends_preserve_fifo_conditions_and_reject_unused_edits() {
        let mutations = [
            put(
                "writer-00/object-00000001",
                Bytes::from_static(b"rejected"),
                PutCondition::IfMatch {
                    object: ObjectId::from([1; 20]),
                    etag: "absent".to_owned(),
                    attributes_present: true,
                },
            ),
            put(
                "writer-01/object-00000001",
                Bytes::from_static(b"first"),
                PutCondition::IfNoneMatchAny,
            ),
            put(
                "writer-01/object-00000001",
                Bytes::from_static(b"rejected"),
                PutCondition::IfNoneMatchAny,
            ),
            put(
                "writer-01/object-00000001",
                Bytes::from_static(b"replacement"),
                PutCondition::IfMatch {
                    object: object_id(Kind::Blob, b"first").unwrap(),
                    etag: crate::gateway::md5_hex(b"first"),
                    attributes_present: true,
                },
            ),
            put(
                "writer-02/object-00000001",
                Bytes::from_static(b"independent"),
                PutCondition::IfNoneMatchAny,
            ),
        ];
        let batch = build_batch(
            None,
            None,
            None,
            attributes::Manifest::empty_complete(),
            Some(fixture(3, 1)),
            0,
            &mutations,
        )
        .await
        .unwrap();
        assert_eq!(
            batch.outcomes.iter().map(Result::is_ok).collect::<Vec<_>>(),
            [false, true, false, true, true]
        );
        assert!(matches!(&batch.outcomes[0], Err(Error::PreconditionFailed)));
        assert!(matches!(&batch.outcomes[2], Err(Error::PreconditionFailed)));
        assert_eq!(batch.commits.len(), 3);
        assert_eq!(batch.commits[1].parent, Some(batch.commits[0].commit));
        assert_eq!(batch.commits[2].parent, Some(batch.commits[1].commit));
        let directories = &batch.tree.directories;
        assert!(
            find_entry(
                &directories[b"writer-00".as_slice()].entries,
                b"object-00000001"
            )
            .is_none()
        );
        assert_eq!(
            find_entry(
                &directories[b"writer-01".as_slice()].entries,
                b"object-00000001"
            )
            .unwrap()
            .oid,
            object_id(Kind::Blob, b"replacement").unwrap()
        );
        assert_eq!(
            find_entry(
                &directories[b"writer-02".as_slice()].entries,
                b"object-00000001"
            )
            .unwrap()
            .oid,
            object_id(Kind::Blob, b"independent").unwrap()
        );
    }
}
