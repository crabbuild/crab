use super::*;

#[test]
fn canonical_assignments_preserve_actor_ids_and_names() {
    let assigned = vec![Assignee {
        id: Uuid::from_u128(1),
        name: "Alice".into(),
    }];
    assert_eq!(decode(&encode(&assigned).unwrap()).unwrap(), assigned);
}
#[test]
fn invalid_or_duplicate_actor_ids_are_rejected() {
    for ids in [
        [Uuid::nil(), Uuid::from_u128(2)],
        [Uuid::from_u128(1), Uuid::from_u128(1)],
        [Uuid::from_u128(2), Uuid::from_u128(1)],
    ] {
        assert!(
            validate(&ids.map(|id| Assignee {
                id,
                name: "Alice".into()
            }))
            .is_err()
        );
    }
}
#[test]
fn serving_does_not_decode_retired_subject_bytes() {
    assert!(decode(&[0, 0, 0, 1, 0, 0, 0, 5, b'a', b'l', b'i', b'c', b'e']).is_err());
}

#[test]
fn empty_assignment_preserves_the_canonical_sql_default() {
    assert_eq!(
        encode(&decode(&[0, 0, 0, 0]).unwrap()).unwrap(),
        [0, 0, 0, 0]
    );
}
