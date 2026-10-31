use super::*;

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
            GitVisibilityClosure::Bitmap(expected)
        })
        .collect::<Vec<_>>();
    let refs = (0..REFS)
        .map(|index| {
            (
                format!("refs/heads/shared-{index:04}"),
                GitVisibilityClosure::Bitmap(templates[index % templates.len()].clone()),
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
