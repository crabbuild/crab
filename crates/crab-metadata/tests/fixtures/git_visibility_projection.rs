use super::*;

#[test]
fn dense_visibility_clones_preserve_membership_and_isolation() {
    const OBJECTS: usize = 65_536;
    const REFS: usize = 2_000;
    const COPIES: usize = 4;
    let objects = (0..OBJECTS)
        .map(|value| {
            let mut oid = [0; 20];
            oid[12..].copy_from_slice(&(value as u64).to_be_bytes());
            oid
        })
        .collect::<Vec<_>>();
    let refs = (0..REFS)
        .map(|reference| {
            (
                format!("refs/heads/shared-{reference:04}"),
                GitVisibilityClosure::Bitmap(vec![u8::MAX; OBJECTS / 8].into()),
            )
        })
        .collect();
    let index = GitVisibilityIndex::from_parts(
        GIT_VISIBILITY_INDEX_VERSION,
        7,
        "a".repeat(64),
        "b".repeat(64),
        objects,
        refs,
        BTreeMap::new(),
    )
    .unwrap();
    let mut elapsed_ms = Vec::new();
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let mut copies: Vec<GitVisibilityIndex> = (0..COPIES)
            .map(|_| std::hint::black_box(&index).clone())
            .collect();
        elapsed_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        for copy in &copies {
            assert_eq!(copy, &index);
        }
        copies[0]
            .refs
            .get_mut("refs/heads/shared-0000")
            .unwrap()
            .remove_positions(&[0])
            .unwrap();
        assert!(!copies[0].refs["refs/heads/shared-0000"].contains(0));
        assert!(index.refs["refs/heads/shared-0000"].contains(0));
        for copy in &copies[1..] {
            assert_eq!(copy, &index);
        }
        for reference in 1..REFS {
            let name = format!("refs/heads/shared-{reference:04}");
            assert_eq!(copies[0].refs[&name], index.refs[&name]);
        }
        copies[0]
            .refs
            .get_mut("refs/heads/shared-0000")
            .unwrap()
            .add_positions(&[0], OBJECTS)
            .unwrap();
        assert_eq!(copies[0], index);
        for copy in &copies {
            copy.validate().unwrap();
        }
    }
    eprintln!(
        "dense visibility clones: objects={OBJECTS} refs={REFS} copies={COPIES} elapsed_ms={elapsed_ms:?}"
    );
}

#[test]
fn dictionary_growth_and_ref_edits_preserve_other_snapshots() {
    let oid = |value: usize| format!("{value:040x}");
    let main = "refs/heads/main";
    let other = "refs/heads/other";
    let archive = "refs/heads/archive";
    let mut index = GitVisibilityIndex::new(
        7,
        "a".repeat(64),
        "b".repeat(64),
        BTreeMap::from([
            (main.to_owned(), (0..16).map(oid).collect()),
            (other.to_owned(), (16..48).map(oid).collect()),
            (archive.to_owned(), (0..48).map(oid).collect()),
        ]),
    )
    .unwrap();
    index
        .apply_ref_edit(
            main.to_owned(),
            &GitVisibilityEdit::from_delta_objects(
                Some(oid(15)),
                oid(63),
                (48..64).map(oid).collect(),
                Vec::new(),
            ),
        )
        .unwrap();
    // The retained fast-forward transition and history are dense too; growing
    // one snapshot must not resize or extend another snapshot's proof.
    assert!(matches!(
        index.transitions[main][0].objects,
        GitVisibilityClosure::Bitmap(_)
    ));
    assert!(matches!(
        index.incremental_history[main][0].objects,
        GitVisibilityClosure::Bitmap(_)
    ));
    let original = index.clone();
    let mut grown = index.clone();
    for (name, previous, next) in [(main, 63, 64), (other, 47, 65)] {
        grown
            .apply_ref_edit(
                name.to_owned(),
                &GitVisibilityEdit::from_delta_objects(
                    Some(oid(previous)),
                    oid(next),
                    vec![oid(next)],
                    Vec::new(),
                ),
            )
            .unwrap();
    }
    let mut removed = index.clone();
    removed
        .apply_ref_edit(
            main.to_owned(),
            &GitVisibilityEdit::from_delta_objects(
                Some(oid(63)),
                oid(63),
                Vec::new(),
                vec![oid(0)],
            ),
        )
        .unwrap();
    assert_eq!(index, original);
    assert_eq!(index.objects.len(), 64);
    assert_eq!(grown.objects.len(), 66);
    assert_eq!(
        grown.objects_for_ref(main).unwrap(),
        (0..16).chain(48..65).map(oid).collect::<Vec<_>>()
    );
    assert_eq!(
        grown.objects_for_ref(other).unwrap(),
        (16..48).chain([65]).map(oid).collect::<Vec<_>>()
    );
    assert_eq!(
        removed.objects_for_ref(main).unwrap(),
        (1..16).chain(48..64).map(oid).collect::<Vec<_>>()
    );
    for snapshot in [&index, &grown, &removed] {
        snapshot.validate().unwrap();
        assert_eq!(
            snapshot.objects_for_ref(archive).unwrap(),
            (0..48).map(oid).collect::<Vec<_>>()
        );
    }
    assert!(index.contains_hex_in_ref(main, &oid(0)));
    assert!(!index.contains_hex_in_ref(main, &oid(64)));
    assert!(!index.contains_hex_in_ref(other, &oid(65)));
}

#[test]
fn dense_ordinal_projection_preserves_shared_ref_membership() {
    // A checkpoint's sorted dictionary is extended with objects from new
    // capsules. Canonicalization inserts those objects into long stretches
    // of old ordinals shared by many ref closures.
    const ORIGINAL: usize = 65_536;
    const APPENDED: usize = 512;
    const REFS: usize = 2_000;
    let count = ORIGINAL + APPENDED;
    let oid = |value: usize| {
        let mut oid = [0_u8; 20];
        oid[12..].copy_from_slice(&(value as u64).to_be_bytes());
        oid
    };
    let objects = (0..ORIGINAL)
        .map(|index| oid(2 * index + 2))
        .chain((0..APPENDED).map(|index| oid(256 * index + 1)))
        .collect::<Vec<_>>();
    let expected_ordinal = |position: usize| {
        if position < ORIGINAL {
            position + (position / 128 + 1).min(APPENDED)
        } else {
            (position - ORIGINAL) * 129
        }
    };
    let patterns = [0xff, 0x55, 0xaa, 0x81];
    let templates = patterns
        .into_iter()
        .map(|pattern| vec![pattern; count.div_ceil(8)])
        .collect::<Vec<_>>();
    let expected = templates
        .iter()
        .map(|bitmap| {
            let mut expected = vec![0_u8; bitmap.len()];
            for position in 0..count {
                if bitmap[position / 8] & (1 << (position % 8)) != 0 {
                    let ordinal = expected_ordinal(position);
                    expected[ordinal / 8] |= 1 << (ordinal % 8);
                }
            }
            GitVisibilityClosure::Bitmap(expected.into())
        })
        .collect::<Vec<_>>();
    let refs = (0..REFS)
        .map(|index| {
            (
                format!("refs/heads/shared-{index:04}"),
                GitVisibilityClosure::Bitmap(templates[index % templates.len()].clone().into()),
            )
        })
        .collect();
    let index = GitVisibilityIndex::from_parts(
        GIT_VISIBILITY_INDEX_VERSION,
        7,
        "a".repeat(64),
        "b".repeat(64),
        objects,
        refs,
        BTreeMap::new(),
    )
    .expect("shared dense ref fixture");
    let mut elapsed_ms = Vec::new();
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let parts = std::hint::black_box(&index).ordinal_parts().unwrap();
        elapsed_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        assert_eq!(parts.objects.len(), count);
        assert!(parts.objects.windows(2).all(|pair| pair[0] < pair[1]));
        for position in 0..count {
            let ordinal = expected_ordinal(position);
            assert_eq!(parts.remap[position] as usize, ordinal);
            assert_eq!(parts.objects[ordinal], index.objects[position]);
        }
        assert_eq!(parts.refs.len(), REFS);
        for reference in 0..REFS {
            assert_eq!(
                parts.refs[&format!("refs/heads/shared-{reference:04}")],
                expected[reference % expected.len()],
                "complete canonical bitmap for ref {reference}"
            );
        }
        assert!(parts.transitions.is_empty());
        assert!(parts.incremental_history.is_empty());
    }
    eprintln!("dense ordinal projection: objects={count} refs={REFS} elapsed_ms={elapsed_ms:?}");
}

#[test]
fn dense_ordinal_projection_keeps_layered_snapshot_wire_identity() {
    use crate::capsule_protocol::{LayeredObjectMember, LayeredVisibilitySnapshot};

    let raw_objects = (0..1_024)
        .map(|index| format!("{:040x}", 2 * index + 2))
        .chain((0..7).map(|index| format!("{:040x}", 256 * index + 1)))
        .collect::<Vec<_>>();
    let memberships = BTreeMap::from([
        (
            "refs/heads/all".to_owned(),
            (0..raw_objects.len()).collect::<Vec<_>>(),
        ),
        (
            "refs/heads/dense".to_owned(),
            (0..raw_objects.len()).step_by(3).collect(),
        ),
        ("refs/heads/sparse".to_owned(), vec![0, 127, 1_030]),
    ]);
    let expected = GitVisibilityIndex::new(
        4,
        "a".repeat(64),
        "b".repeat(64),
        memberships
            .iter()
            .map(|(name, positions)| {
                (
                    name.clone(),
                    positions
                        .iter()
                        .map(|position| raw_objects[*position].clone())
                        .collect(),
                )
            })
            .collect(),
    )
    .unwrap();
    let index = GitVisibilityIndex::from_parts(
        GIT_VISIBILITY_INDEX_VERSION,
        4,
        "a".repeat(64),
        "b".repeat(64),
        raw_objects
            .iter()
            .map(|oid| decode_oid(oid).unwrap())
            .collect(),
        memberships
            .into_iter()
            .map(|(name, positions)| {
                (
                    name,
                    GitVisibilityClosure::from_positions(
                        positions
                            .into_iter()
                            .map(|position| position as u32)
                            .collect(),
                        raw_objects.len(),
                    )
                    .unwrap(),
                )
            })
            .collect(),
        BTreeMap::new(),
    )
    .unwrap();
    let member = |oid: &GitVisibilityOid| {
        let position = index
            .objects
            .iter()
            .position(|object| object == oid)
            .unwrap();
        LayeredObjectMember::new(
            u16::try_from(position / 512).unwrap(),
            u16::try_from(position % 512).unwrap(),
        )
    };
    for with_members in [false, true] {
        let snapshot = |visibility: &GitVisibilityIndex| {
            if with_members {
                LayeredVisibilitySnapshot::from_index_with_member_admission(
                    visibility,
                    &"c".repeat(64),
                    visibility.objects.iter().map(member).collect(),
                )
            } else {
                LayeredVisibilitySnapshot::from_index(visibility, &"c".repeat(64))
            }
            .unwrap()
        };
        let actual = snapshot(&index).encode().unwrap();
        let expected = snapshot(&expected).encode().unwrap();
        assert_eq!(
            actual, expected,
            "exact wire bytes with_members={with_members}"
        );
        assert_eq!(
            LayeredVisibilitySnapshot::decode(&actual)
                .unwrap()
                .encode()
                .unwrap(),
            actual
        );
    }
}
