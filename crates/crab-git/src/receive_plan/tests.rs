use super::*;
use crate::incoming_pack::{ReceiveLimits, object_id, quarantine};
use flate2::{Compression, write::ZlibEncoder};
use gix_pack::data::entry::Header;
use sha1::{Digest, Sha1};
use std::io::Write;

#[derive(Default)]
struct Source {
    objects: HashMap<ObjectId, (Kind, Vec<u8>)>,
    trusted: bool,
    reads: Vec<ObjectId>,
}
impl GraphSource for Source {
    fn trusted_kind(&mut self, oid: &ObjectId) -> std::result::Result<Option<Kind>, SourceError> {
        Ok(self
            .trusted
            .then(|| self.objects.get(oid).map(|(kind, _)| *kind))
            .flatten())
    }
    fn read(&mut self, oid: &ObjectId) -> std::result::Result<Option<BaseObject>, SourceError> {
        self.reads.push(*oid);
        Ok(self.objects.get(oid).map(|(kind, data)| BaseObject {
            kind: *kind,
            data: data.clone(),
        }))
    }
}
fn graph_limits() -> GraphLimits {
    GraphLimits {
        max_ref_updates: 32,
        max_graph_steps: 10000,
        max_object_bytes: 1024 * 1024,
        max_read_bytes: 16 * 1024 * 1024,
    }
}
fn policy(_: &str) -> RefPolicy {
    RefPolicy {
        allow_delete: true,
        allow_non_fast_forward: false,
    }
}
fn incoming(objects: &[(Kind, Vec<u8>)]) -> IncomingPack {
    let mut pack = b"PACK\0\0\0\x02".to_vec();
    pack.extend_from_slice(&(objects.len() as u32).to_be_bytes());
    for (kind, data) in objects {
        let header = match kind {
            Kind::Commit => Header::Commit,
            Kind::Tree => Header::Tree,
            Kind::Tag => Header::Tag,
            Kind::Blob => Header::Blob,
        };
        header.write_to(data.len() as u64, &mut pack).unwrap();
        let mut encoder = ZlibEncoder::new(&mut pack, Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap();
    }
    let hash = Sha1::digest(&pack);
    pack.extend_from_slice(&hash);
    quarantine(
        &pack[..],
        &std::env::temp_dir(),
        ReceiveLimits {
            max_pack_bytes: 16 * 1024 * 1024,
            max_objects: 1000,
            max_object_bytes: 1024 * 1024,
            max_inflated_bytes: 16 * 1024 * 1024,
            max_delta_depth: 32,
        },
        || false,
        |_| Ok(None),
    )
    .unwrap()
}
fn tree(entries: &[(&str, &[u8], ObjectId)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (mode, name, oid) in entries {
        bytes.extend_from_slice(mode.as_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.extend_from_slice(oid.as_slice());
    }
    bytes
}
fn commit(root: ObjectId, parents: &[ObjectId], message: &str) -> Vec<u8> {
    let parents = parents
        .iter()
        .map(|oid| format!("parent {oid}\n"))
        .collect::<String>();
    format!("tree {root}\n{parents}author Crab <crab@example.invalid> 1700000000 +0000\ncommitter Crab <crab@example.invalid> 1700000000 +0000\n\n{message}\n").into_bytes()
}
fn tag(oid: ObjectId, kind: Kind, name: &str) -> Vec<u8> {
    format!("object {oid}\ntype {kind}\ntag {name}\ntagger Crab <crab@example.invalid> 1700000000 +0000\n\nRelease\n").into_bytes()
}
fn base() -> (Source, ObjectId, ObjectId) {
    let root = object_id(Kind::Tree, b"");
    let body = commit(root, &[], "base");
    let oid = object_id(Kind::Commit, &body);
    (
        Source {
            objects: HashMap::from([
                (root, (Kind::Tree, Vec::new())),
                (oid, (Kind::Commit, body)),
            ]),
            trusted: true,
            reads: Vec::new(),
        },
        oid,
        root,
    )
}
fn update(name: &str, old: Option<ObjectId>, new: Option<ObjectId>) -> RefUpdate {
    RefUpdate {
        name: name.into(),
        old,
        new,
    }
}

#[test]
fn preserves_exact_fast_forward_and_annotated_tag_ids_using_committed_frontier() {
    let (mut source, old, root) = base();
    let body = commit(root, &[old], "next");
    let new = object_id(Kind::Commit, &body);
    let tag_body = tag(new, Kind::Commit, "v1");
    let tag_id = object_id(Kind::Tag, &tag_body);
    let incoming = incoming(&[(Kind::Commit, body), (Kind::Tag, tag_body)]);
    let refs = BTreeMap::from([("refs/heads/main".into(), old)]);
    let updates = [
        update("refs/heads/main", Some(old), Some(new)),
        update("refs/tags/v1", None, Some(tag_id)),
    ];
    let plan = validate(
        &incoming,
        &refs,
        &updates,
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();
    assert_eq!(
        plan.refs(),
        &BTreeMap::from([
            ("refs/heads/main".into(), new),
            ("refs/tags/v1".into(), tag_id)
        ])
    );
    assert_eq!(plan.peeled().get("refs/tags/v1"), Some(&new));
    assert_eq!(
        source.reads,
        vec![old],
        "changed-path comparison should read only the trusted parent commit"
    );
    assert_eq!(refs["refs/heads/main"], old);
}

#[test]
fn stale_duplicate_and_namespace_conflicts_do_not_read_or_modify_objects() {
    let (mut source, old, _) = base();
    let incoming = incoming(&[]);
    let refs = BTreeMap::from([("refs/heads/main".into(), old)]);
    for updates in [
        vec![update("refs/heads/main", None, Some(old))],
        vec![
            update("refs/tags/a", None, Some(old)),
            update("refs/tags/a", None, Some(old)),
        ],
        vec![update("refs/heads/main/nested", None, Some(old))],
        vec![update("main", None, Some(old))],
    ] {
        assert!(
            validate(
                &incoming,
                &refs,
                &updates,
                policy,
                &mut source,
                graph_limits(),
                || false
            )
            .is_err()
        );
        assert_eq!(refs["refs/heads/main"], old);
    }
    assert!(source.reads.is_empty());
    let updates = [
        update("refs/heads/main", Some(old), None),
        update("refs/heads/main/nested", None, Some(old)),
    ];
    let plan = validate(
        &incoming,
        &refs,
        &updates,
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();
    assert_eq!(
        plan.refs(),
        &BTreeMap::from([("refs/heads/main/nested".into(), old)])
    );
}

#[test]
fn force_and_delete_policy_are_enforced_without_commit_translation() {
    let (mut source, old, root) = base();
    let body = commit(root, &[], "replacement");
    let new = object_id(Kind::Commit, &body);
    let incoming = incoming(&[(Kind::Commit, body)]);
    let refs = BTreeMap::from([("refs/heads/main".into(), old)]);
    let updates = [update("refs/heads/main", Some(old), Some(new))];
    assert!(matches!(
        validate(
            &incoming,
            &refs,
            &updates,
            policy,
            &mut source,
            graph_limits(),
            || false
        ),
        Err(ReceivePlanError::NonFastForward { .. })
    ));
    let force = |_: &str| RefPolicy {
        allow_non_fast_forward: true,
        allow_delete: false,
    };
    assert_eq!(
        validate(
            &incoming,
            &refs,
            &updates,
            force,
            &mut source,
            graph_limits(),
            || false
        )
        .unwrap()
        .refs()["refs/heads/main"],
        new
    );
    assert!(
        validate(
            &incoming,
            &refs,
            &[update("refs/heads/main", Some(old), None)],
            force,
            &mut source,
            graph_limits(),
            || false
        )
        .is_err()
    );
}

#[test]
fn rejects_missing_and_wrong_kind_links_even_after_an_object_was_visited() {
    let (mut source, old, root) = base();
    let missing = ObjectId::Sha1([7; 20]);
    let body = commit(missing, &[old], "broken tree");
    let new = object_id(Kind::Commit, &body);
    let incoming = incoming(&[(Kind::Commit, body)]);
    assert!(
        matches!(validate(&incoming,&BTreeMap::new(),&[update("refs/heads/main",None,Some(new))],policy,&mut source,graph_limits(),||false),Err(ReceivePlanError::Missing {oid}) if oid==missing)
    );
    let wrong = tree(&[("100644", b"file", old)]);
    let wrong_id = object_id(Kind::Tree, &wrong);
    let body = commit(wrong_id, &[old], "wrong kind");
    let new = object_id(Kind::Commit, &body);
    let received = super::tests::incoming(&[(Kind::Commit, body), (Kind::Tree, wrong)]);
    assert!(
        matches!(validate(&received,&BTreeMap::new(),&[update("refs/heads/main",None,Some(new))],policy,&mut source,graph_limits(),||false),Err(ReceivePlanError::Kind {oid,expected:Kind::Blob,actual:Kind::Commit}) if oid==old)
    );
    let empty = super::tests::incoming(&[]);
    assert!(matches!(
        validate(
            &empty,
            &BTreeMap::new(),
            &[update("refs/heads/main", None, Some(root))],
            policy,
            &mut source,
            graph_limits(),
            || false
        ),
        Err(ReceivePlanError::Kind { .. })
    ));
}

#[test]
fn validates_unreachable_objects_and_all_tree_names_without_following_gitlinks() {
    let (mut source, old, _) = base();
    let blob = b"content".to_vec();
    let blob_id = object_id(Kind::Blob, &blob);
    let valid_tree = tree(&[
        ("100644", b"raw-\xff", blob_id),
        ("160000", b"submodule", ObjectId::Sha1([9; 20])),
    ]);
    let root = object_id(Kind::Tree, &valid_tree);
    let body = commit(root, &[old], "raw bytes and gitlink");
    let new = object_id(Kind::Commit, &body);
    let received = incoming(&[
        (Kind::Commit, body),
        (Kind::Tree, valid_tree),
        (Kind::Blob, blob),
    ]);
    let plan = validate(
        &received,
        &BTreeMap::new(),
        &[update("refs/heads/main", None, Some(new))],
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();
    assert_eq!(
        plan.changed_path_hashes(),
        &BTreeSet::from([
            *blake3::hash(b"raw-\xff").as_bytes(),
            *blake3::hash(b"submodule").as_bytes(),
        ])
    );
    for entries in [
        vec![
            ("100644", b"a".as_slice(), blob_id),
            ("100644", b"a.c".as_slice(), blob_id),
            ("40000", b"a".as_slice(), object_id(Kind::Tree, b"")),
        ],
        vec![("100644", b".git".as_slice(), blob_id)],
        vec![("120000", b".gitmodules".as_slice(), blob_id)],
        vec![("100644", b"../escape".as_slice(), blob_id)],
        vec![("100664", b"file".as_slice(), blob_id)],
    ] {
        let received = incoming(&[(Kind::Tree, tree(&entries))]);
        assert!(
            validate(
                &received,
                &BTreeMap::new(),
                &[],
                policy,
                &mut source,
                graph_limits(),
                || false
            )
            .is_err()
        );
    }
    let received = incoming(&[(Kind::Commit, b"invalid commit".to_vec())]);
    assert!(matches!(
        validate(
            &received,
            &BTreeMap::new(),
            &[],
            policy,
            &mut source,
            graph_limits(),
            || false
        ),
        Err(ReceivePlanError::Parse { .. })
    ));
}

#[test]
fn reports_crab_and_lfs_dependencies_and_rejects_incorrect_tag_target_kinds() {
    let pointer = crab_types::pointer::Pointer {
        file_hash: [3; 32],
        size: 42,
        shard_hint: None,
    }
    .serialize();
    let lfs = format!(
        "version {}\noid sha256:{}\nsize 42\n",
        crate::LFS_VERSION_URL,
        "ab".repeat(32)
    )
    .into_bytes();
    let ids = [object_id(Kind::Blob, &pointer), object_id(Kind::Blob, &lfs)];
    let received = incoming(&[(Kind::Blob, pointer), (Kind::Blob, lfs)]);
    let mut source = Source::default();
    let plan = validate(
        &received,
        &BTreeMap::new(),
        &[update("refs/tags/pointer", None, Some(ids[0]))],
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();
    assert_eq!(plan.pointers().len(), 2);
    assert!(plan.peeled().is_empty());
    let invalid_tag = tag(ids[0], Kind::Commit, "bad");
    let oid = object_id(Kind::Tag, &invalid_tag);
    let received = incoming(&[(Kind::Tag, invalid_tag)]);
    source
        .objects
        .insert(ids[0], (Kind::Blob, b"unused".to_vec()));
    source.trusted = true;
    assert!(matches!(
        validate(
            &received,
            &BTreeMap::new(),
            &[update("refs/tags/bad", None, Some(oid))],
            policy,
            &mut source,
            graph_limits(),
            || false
        ),
        Err(ReceivePlanError::Kind {
            expected: Kind::Commit,
            actual: Kind::Blob,
            ..
        })
    ));
}

#[test]
fn validation_budgets_and_cancellation_fail_before_publication() {
    let (mut source, old, root) = base();
    let body = commit(root, &[old], "next");
    let new = object_id(Kind::Commit, &body);
    let received = incoming(&[(Kind::Commit, body)]);
    let updates = [update("refs/heads/main", None, Some(new))];
    for bounds in [
        GraphLimits {
            max_ref_updates: 0,
            ..graph_limits()
        },
        GraphLimits {
            max_graph_steps: 0,
            ..graph_limits()
        },
        GraphLimits {
            max_object_bytes: 1,
            ..graph_limits()
        },
        GraphLimits {
            max_read_bytes: 1,
            ..graph_limits()
        },
    ] {
        assert!(matches!(
            validate(
                &received,
                &BTreeMap::new(),
                &updates,
                policy,
                &mut source,
                bounds,
                || false
            ),
            Err(ReceivePlanError::Limit(_))
        ));
    }
    assert!(matches!(
        validate(
            &received,
            &BTreeMap::new(),
            &updates,
            policy,
            &mut source,
            graph_limits(),
            || true
        ),
        Err(ReceivePlanError::Cancelled)
    ));
    source.trusted = false;
    source.objects.get_mut(&old).unwrap().1 = b"different".to_vec();
    assert!(matches!(
        validate(
            &received,
            &BTreeMap::new(),
            &updates,
            policy,
            &mut source,
            graph_limits(),
            || false
        ),
        Err(ReceivePlanError::Invalid {
            reason: "object identity mismatch",
            ..
        })
    ));
}

#[test]
fn changed_paths_include_intermediate_commits_even_when_the_tip_reverts() {
    let original = b"original".to_vec();
    let changed = b"changed".to_vec();
    let original_blob = object_id(Kind::Blob, &original);
    let changed_blob = object_id(Kind::Blob, &changed);
    let original_tree_body = tree(&[("100644", b"locked.bin", original_blob)]);
    let changed_tree_body = tree(&[("100644", b"locked.bin", changed_blob)]);
    let original_tree = object_id(Kind::Tree, &original_tree_body);
    let changed_tree = object_id(Kind::Tree, &changed_tree_body);
    let base_body = commit(original_tree, &[], "base");
    let base = object_id(Kind::Commit, &base_body);
    let changed_body = commit(changed_tree, &[base], "change locked path");
    let changed_commit = object_id(Kind::Commit, &changed_body);
    let reverted_body = commit(original_tree, &[changed_commit], "revert locked path");
    let reverted = object_id(Kind::Commit, &reverted_body);
    let mut source = Source {
        objects: HashMap::from([
            (original_blob, (Kind::Blob, original)),
            (original_tree, (Kind::Tree, original_tree_body)),
            (base, (Kind::Commit, base_body)),
        ]),
        trusted: true,
        reads: Vec::new(),
    };
    let received = incoming(&[
        (Kind::Blob, changed),
        (Kind::Tree, changed_tree_body),
        (Kind::Commit, changed_body),
        (Kind::Commit, reverted_body),
    ]);
    let refs = BTreeMap::from([("refs/heads/main".to_owned(), base)]);
    let plan = validate(
        &received,
        &refs,
        &[update("refs/heads/main", Some(base), Some(reverted))],
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();

    assert_eq!(
        plan.changed_path_hashes(),
        &BTreeSet::from([*blake3::hash(b"locked.bin").as_bytes()])
    );
}

#[test]
fn changed_paths_include_both_sides_of_a_rename() {
    let blob = b"content".to_vec();
    let blob_id = object_id(Kind::Blob, &blob);
    let old_tree_body = tree(&[("100644", b"old.bin", blob_id)]);
    let new_tree_body = tree(&[("100644", b"new.bin", blob_id)]);
    let old_tree = object_id(Kind::Tree, &old_tree_body);
    let new_tree = object_id(Kind::Tree, &new_tree_body);
    let old_body = commit(old_tree, &[], "base");
    let old = object_id(Kind::Commit, &old_body);
    let new_body = commit(new_tree, &[old], "rename");
    let new = object_id(Kind::Commit, &new_body);
    let mut source = Source {
        objects: HashMap::from([
            (blob_id, (Kind::Blob, blob)),
            (old_tree, (Kind::Tree, old_tree_body)),
            (old, (Kind::Commit, old_body)),
        ]),
        trusted: true,
        reads: Vec::new(),
    };
    let received = incoming(&[(Kind::Tree, new_tree_body), (Kind::Commit, new_body)]);
    let refs = BTreeMap::from([("refs/heads/main".to_owned(), old)]);
    let plan = validate(
        &received,
        &refs,
        &[update("refs/heads/main", Some(old), Some(new))],
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();

    assert_eq!(
        plan.changed_path_hashes(),
        &BTreeSet::from([
            *blake3::hash(b"new.bin").as_bytes(),
            *blake3::hash(b"old.bin").as_bytes(),
        ])
    );
}

#[test]
fn existing_commits_and_ref_deletions_introduce_no_changed_paths() {
    let (mut source, existing, _) = base();
    let received = incoming(&[]);
    let refs = BTreeMap::from([("refs/heads/main".to_owned(), existing)]);
    let plan = validate(
        &received,
        &refs,
        &[
            update("refs/heads/feature", None, Some(existing)),
            update("refs/heads/main", Some(existing), None),
        ],
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();

    assert!(plan.changed_path_hashes().is_empty());
}

#[test]
fn changed_paths_compare_merge_commits_with_every_parent() {
    let a = object_id(Kind::Blob, b"a");
    let b = object_id(Kind::Blob, b"b");
    let left_tree_body = tree(&[("100644", b"left.bin", a)]);
    let right_tree_body = tree(&[("100644", b"right.bin", b)]);
    let merge_tree_body = tree(&[("100644", b"left.bin", a), ("100644", b"right.bin", b)]);
    let left_tree = object_id(Kind::Tree, &left_tree_body);
    let right_tree = object_id(Kind::Tree, &right_tree_body);
    let merge_tree = object_id(Kind::Tree, &merge_tree_body);
    let left_body = commit(left_tree, &[], "left");
    let right_body = commit(right_tree, &[], "right");
    let left = object_id(Kind::Commit, &left_body);
    let right = object_id(Kind::Commit, &right_body);
    let merge_body = commit(merge_tree, &[left, right], "merge");
    let merge = object_id(Kind::Commit, &merge_body);
    let mut source = Source {
        objects: HashMap::from([
            (a, (Kind::Blob, b"a".to_vec())),
            (b, (Kind::Blob, b"b".to_vec())),
            (left_tree, (Kind::Tree, left_tree_body)),
            (right_tree, (Kind::Tree, right_tree_body)),
            (left, (Kind::Commit, left_body)),
            (right, (Kind::Commit, right_body)),
        ]),
        trusted: true,
        reads: Vec::new(),
    };
    let received = incoming(&[(Kind::Tree, merge_tree_body), (Kind::Commit, merge_body)]);
    let refs = BTreeMap::from([
        ("refs/heads/left".to_owned(), left),
        ("refs/heads/right".to_owned(), right),
    ]);
    let plan = validate(
        &received,
        &refs,
        &[update("refs/heads/left", Some(left), Some(merge))],
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();

    assert_eq!(
        plan.changed_path_hashes(),
        &BTreeSet::from([
            *blake3::hash(b"left.bin").as_bytes(),
            *blake3::hash(b"right.bin").as_bytes(),
        ])
    );
}

#[test]
fn changed_paths_cover_modes_and_tree_leaf_replacements() {
    let blob = object_id(Kind::Blob, b"content");
    let old_tree_body = tree(&[("100644", b"mode.bin", blob), ("100644", b"node", blob)]);
    let nested_body = tree(&[("100644", b"child.bin", blob)]);
    let nested = object_id(Kind::Tree, &nested_body);
    let new_tree_body = tree(&[("100755", b"mode.bin", blob), ("40000", b"node", nested)]);
    let old_tree = object_id(Kind::Tree, &old_tree_body);
    let new_tree = object_id(Kind::Tree, &new_tree_body);
    let old_body = commit(old_tree, &[], "base");
    let old = object_id(Kind::Commit, &old_body);
    let new_body = commit(new_tree, &[old], "replace");
    let new = object_id(Kind::Commit, &new_body);
    let mut source = Source {
        objects: HashMap::from([
            (blob, (Kind::Blob, b"content".to_vec())),
            (old_tree, (Kind::Tree, old_tree_body)),
            (old, (Kind::Commit, old_body)),
        ]),
        trusted: true,
        reads: Vec::new(),
    };
    let received = incoming(&[
        (Kind::Tree, nested_body),
        (Kind::Tree, new_tree_body),
        (Kind::Commit, new_body),
    ]);
    let refs = BTreeMap::from([("refs/heads/main".to_owned(), old)]);
    let plan = validate(
        &received,
        &refs,
        &[update("refs/heads/main", Some(old), Some(new))],
        policy,
        &mut source,
        graph_limits(),
        || false,
    )
    .unwrap();

    assert_eq!(
        plan.changed_path_hashes(),
        &BTreeSet::from([
            *blake3::hash(b"mode.bin").as_bytes(),
            *blake3::hash(b"node").as_bytes(),
            *blake3::hash(b"node/child.bin").as_bytes(),
        ])
    );
}

mod visibility;
