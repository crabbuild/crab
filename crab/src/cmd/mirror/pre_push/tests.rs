use super::*;

fn oid(value: char) -> String {
    value.to_string().repeat(40)
}

fn update(name: &str, old: Option<char>, new: Option<char>) -> PrePushUpdate {
    PrePushUpdate {
        local_oid: new.map(oid),
        remote_ref: name.to_owned(),
        remote_oid: old.map(oid),
    }
}

#[test]
fn admission_captures_crab_values_instead_of_collaboration_values() {
    let updates = vec![
        update("refs/heads/new", Some('a'), Some('b')),
        update("refs/tags/retry", Some('a'), Some('b')),
        update("refs/heads/delete", Some('a'), None),
    ];
    let crab = BTreeMap::from([
        ("refs/tags/retry".into(), oid('b')),
        ("refs/heads/delete".into(), oid('a')),
        ("refs/heads/crab-only".into(), oid('c')),
    ]);
    assert_eq!(
        admit_updates(&updates, &crab).unwrap(),
        BTreeMap::from([
            ("refs/heads/new".into(), None),
            ("refs/tags/retry".into(), Some(oid('b'))),
            ("refs/heads/delete".into(), Some(oid('a'))),
        ])
    );
}

#[test]
fn divergent_or_crab_ahead_ref_rejects_the_entire_batch() {
    for new in [Some('b'), None] {
        let updates = vec![
            update("refs/heads/new", None, Some('a')),
            update("refs/heads/conflict", Some('a'), new),
        ];
        let crab = BTreeMap::from([("refs/heads/conflict".into(), oid('c'))]);
        assert!(admit_updates(&updates, &crab).is_err());
    }
}

#[test]
fn rewrite_is_admitted_only_with_an_independent_matching_crab_snapshot() {
    let update = update("refs/heads/release", Some('b'), Some('a'));
    let crab = BTreeMap::from([("refs/heads/release".into(), oid('b'))]);
    assert_eq!(
        admit_updates(&[update], &crab).unwrap(),
        BTreeMap::from([("refs/heads/release".into(), Some(oid('b')))])
    );
}

#[test]
fn sha256_snapshot_is_refused_before_native_publication() {
    let mut update = update("refs/heads/main", None, Some('a'));
    update.local_oid = Some("a".repeat(64));
    assert!(admit_updates(&[update], &BTreeMap::new()).is_err());
}
