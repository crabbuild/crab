use super::*;

#[test]
fn label_ids_retain_the_persisted_big_endian_format() {
    assert_eq!(
        encode_label_ids(&[1, 20]).unwrap(),
        vec![0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 20]
    );
}

#[test]
fn assignees_retain_the_persisted_length_prefixed_format() {
    let stored = vec![0, 0, 0, 1, 0, 0, 0, 5, b'a', b'l', b'i', b'c', b'e'];
    assert_eq!(decode_assignees(&stored).unwrap(), vec!["alice".to_owned()]);
    assert_eq!(encode_assignees(&["alice".to_owned()]).unwrap(), stored);
}
