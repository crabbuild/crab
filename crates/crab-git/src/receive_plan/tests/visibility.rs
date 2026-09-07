use super::*;

struct Visibility {
    source: Source,
    prior: Option<ObjectId>,
    members: HashSet<ObjectId>,
}
impl GraphSource for Visibility {
    fn trusted_kind(&mut self, oid: &ObjectId) -> std::result::Result<Option<Kind>, SourceError> {
        self.source.trusted_kind(oid)
    }
    fn read(&mut self, oid: &ObjectId) -> std::result::Result<Option<BaseObject>, SourceError> {
        self.source.read(oid)
    }
}
impl VisibilitySource for Visibility {
    fn prior_tip(&self) -> Option<ObjectId> {
        self.prior
    }
    fn in_prior_closure(&mut self, oid: &ObjectId) -> std::result::Result<bool, SourceError> {
        Ok(self.members.contains(oid))
    }
}
fn sorted(objects: impl IntoIterator<Item = ObjectId>) -> Vec<ObjectId> {
    objects
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[test]
fn additive_visibility_reuses_only_the_reached_prior_tip_and_excludes_unreachable_uploads() {
    let (source, old, root) = base();
    let mut source = Visibility {
        source,
        prior: Some(old),
        members: HashSet::from([old, root]),
    };
    let body = commit(root, &[old], "new");
    let new = object_id(Kind::Commit, &body);
    let tag_body = tag(new, Kind::Commit, "v1");
    let tagged = object_id(Kind::Tag, &tag_body);
    let incoming = incoming(&[
        (Kind::Commit, body),
        (Kind::Tag, tag_body),
        (Kind::Blob, b"unreachable".to_vec()),
    ]);
    for (tip, added) in [(new, vec![new]), (tagged, sorted([new, tagged]))] {
        assert_eq!(
            plan_visibility(&incoming, tip, &mut source, graph_limits(), || false).unwrap(),
            RefVisibility::Additive { base: old, added }
        );
    }
    assert!(
        source.source.reads.is_empty(),
        "reuse must not read old object bodies"
    );
}

#[test]
fn rewrite_expands_shared_subtrees_without_retaining_old_history_or_unrelated_visibility() {
    let shared = object_id(Kind::Blob, b"shared");
    let removed = object_id(Kind::Blob, b"removed");
    let subtree = tree(&[("100644", b"shared", shared)]);
    let subtree_oid = object_id(Kind::Tree, &subtree);
    let old_tree = tree(&[
        ("100644", b"removed", removed),
        ("40000", b"sub", subtree_oid),
    ]);
    let old_root = object_id(Kind::Tree, &old_tree);
    let old_commit = commit(old_root, &[], "old");
    let old = object_id(Kind::Commit, &old_commit);
    let unrelated_body = commit(old_root, &[], "other ref");
    let unrelated = object_id(Kind::Commit, &unrelated_body);
    let objects = HashMap::from([
        (shared, (Kind::Blob, b"shared".to_vec())),
        (removed, (Kind::Blob, b"removed".to_vec())),
        (subtree_oid, (Kind::Tree, subtree)),
        (old_root, (Kind::Tree, old_tree)),
        (old, (Kind::Commit, old_commit)),
        (unrelated, (Kind::Commit, unrelated_body)),
    ]);
    let mut source = Visibility {
        source: Source {
            objects,
            trusted: true,
            reads: Vec::new(),
        },
        prior: Some(old),
        members: HashSet::from([old, old_root, subtree_oid, shared, removed]),
    };
    let new_tree = tree(&[("40000", b"sub", subtree_oid)]);
    let root = object_id(Kind::Tree, &new_tree);
    let body = commit(root, &[], "rewritten root");
    let new = object_id(Kind::Commit, &body);
    let incoming = incoming(&[(Kind::Tree, new_tree), (Kind::Commit, body)]);
    assert_eq!(
        plan_visibility(&incoming, new, &mut source, graph_limits(), || false).unwrap(),
        RefVisibility::Replacement {
            objects: sorted([new, root, subtree_oid, shared])
        }
    );
    assert_eq!(
        source.source.reads,
        vec![subtree_oid],
        "only the shared tree needs expansion; proven blobs are leaves"
    );
}

#[test]
fn replacement_traverses_trusted_objects_without_a_prior_ref_and_excludes_gitlinks() {
    let (source, old, root) = base();
    let mut source = Visibility {
        source,
        prior: None,
        members: HashSet::new(),
    };
    let linked_tree = tree(&[("160000", b"submodule", ObjectId::from([7; 20]))]);
    let linked_root = object_id(Kind::Tree, &linked_tree);
    let body = commit(linked_root, &[old], "new");
    let new = object_id(Kind::Commit, &body);
    let incoming = incoming(&[(Kind::Tree, linked_tree), (Kind::Commit, body)]);
    assert_eq!(
        plan_visibility(&incoming, new, &mut source, graph_limits(), || false).unwrap(),
        RefVisibility::Replacement {
            objects: sorted([new, linked_root, old, root])
        }
    );
    assert_eq!(
        source.source.reads.iter().copied().collect::<HashSet<_>>(),
        HashSet::from([old, root])
    );
}

#[test]
fn visibility_rejects_wrong_typed_edges_and_missing_or_corrupt_remote_objects() {
    let (source, old, root) = base();
    let mut source = Visibility {
        source,
        prior: None,
        members: HashSet::new(),
    };
    let wrong = tree(&[("100644", b"a", old)]);
    let oid = object_id(Kind::Tree, &wrong);
    let incoming = incoming(&[(Kind::Tree, wrong)]);
    assert!(
        matches!(plan_visibility(&incoming, oid, &mut source, graph_limits(), || false),
        Err(ReceivePlanError::Kind { oid, .. }) if oid == old)
    );
    source.source.objects.remove(&root);
    assert!(
        matches!(plan_visibility(&incoming, old, &mut source, graph_limits(), || false),
        Err(ReceivePlanError::Missing { oid }) if oid == root)
    );
    source
        .source
        .objects
        .insert(root, (Kind::Tree, b"bad".to_vec()));
    assert!(matches!(
        plan_visibility(&incoming, old, &mut source, graph_limits(), || false),
        Err(ReceivePlanError::Invalid {
            reason: "object identity mismatch",
            ..
        })
    ));
}

#[test]
fn visibility_honors_bounds_cancellation_and_prior_binding() {
    let (source, old, root) = base();
    let mut source = Visibility {
        source,
        prior: Some(old),
        members: HashSet::from([root]),
    };
    let incoming = incoming(&[]);
    assert!(matches!(
        plan_visibility(&incoming, old, &mut source, graph_limits(), || false),
        Err(ReceivePlanError::Invalid {
            reason: "prior visibility does not contain its ref tip",
            ..
        })
    ));
    source.prior = None;
    for limits in [
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
            plan_visibility(&incoming, old, &mut source, limits, || false),
            Err(ReceivePlanError::Limit(_))
        ));
    }
    assert!(matches!(
        plan_visibility(&incoming, old, &mut source, graph_limits(), || true),
        Err(ReceivePlanError::Cancelled)
    ));
}
