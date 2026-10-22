use super::*;

#[test]
fn label_ids_retain_the_persisted_big_endian_format() {
    assert_eq!(
        encode_label_ids(&[1, 20]).unwrap(),
        vec![0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 20]
    );
}
