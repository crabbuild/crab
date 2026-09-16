use crab_cell_runtime::{BoundedDecoder, BoundedEncoder, CodecError, WireValue};

fn roundtrip<T>(value: T, limit: u32) -> T
where
    T: WireValue,
{
    let mut encoder = BoundedEncoder::new(limit).unwrap();
    value.encode(&mut encoder).unwrap();
    let bytes = encoder.finish();
    let mut decoder = BoundedDecoder::new(&bytes, limit).unwrap();
    let decoded = T::decode(&mut decoder).unwrap();
    decoder.finish().unwrap();
    decoded
}

#[test]
fn signed_extremes_and_length_delimited_values_roundtrip_exactly() {
    assert_eq!(roundtrip(i64::MIN, 8), i64::MIN);
    assert_eq!(roundtrip(i64::MAX, 8), i64::MAX);
    assert_eq!(
        roundtrip(Some("crab".to_owned()), 16),
        Some("crab".to_owned())
    );
    assert_eq!(roundtrip(vec![0, 1, 255], 16), vec![0, 1, 255]);
}

#[test]
fn invalid_tags_noncanonical_floats_and_trailing_bytes_fail_closed() {
    let mut invalid_bool = BoundedDecoder::new(&[2], 1).unwrap();
    assert!(matches!(
        bool::decode(&mut invalid_bool),
        Err(CodecError::Invalid("invalid bool tag"))
    ));

    let negative_zero_bytes = (-0.0_f64).to_bits().to_be_bytes();
    let mut negative_zero = BoundedDecoder::new(&negative_zero_bytes, 8).unwrap();
    assert!(matches!(
        f64::decode(&mut negative_zero),
        Err(CodecError::Invalid("noncanonical f64"))
    ));

    let mut trailing = BoundedDecoder::new(&[1, 0], 2).unwrap();
    assert!(bool::decode(&mut trailing).unwrap());
    assert!(matches!(
        trailing.finish(),
        Err(CodecError::Invalid("trailing wire bytes"))
    ));
}

#[test]
fn encoder_and_decoder_enforce_declared_limits_before_allocation() {
    let mut encoder = BoundedEncoder::new(4).unwrap();
    assert!(matches!(
        b"payload".to_vec().encode(&mut encoder),
        Err(CodecError::Limit)
    ));
    assert!(matches!(
        BoundedDecoder::new(&[0; 5], 4),
        Err(CodecError::Limit)
    ));
    assert!(matches!(
        BoundedEncoder::new(1024 * 1024 + 1),
        Err(CodecError::Limit)
    ));
}
