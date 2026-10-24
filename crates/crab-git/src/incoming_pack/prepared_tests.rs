use proptest::prelude::*;

use super::prefix_suffix_delta;

#[test]
fn common_edge_boundaries_preserve_delta_reconstruction() {
    for prefix in [0, 1, 63, 64, 65, 127, 128, 129, 256] {
        for suffix in [0, 1, 63, 64, 65, 127, 128, 129, 256] {
            let mut base = vec![1; prefix];
            base.extend_from_slice(&[2; 7]);
            base.extend(vec![4; suffix]);
            let mut target = vec![1; prefix];
            target.extend_from_slice(&[3; 11]);
            target.extend(vec![4; suffix]);
            match prefix_suffix_delta(&base, &target).unwrap() {
                Some(encoded) => {
                    let delta = crate::delta::parse(&encoded, target.len()).unwrap();
                    assert_eq!(crate::delta::apply(&base, delta, || false).unwrap(), target);
                }
                None => assert!(prefix + suffix < 128),
            }
        }
    }
}

proptest! {
    #[test]
    fn common_edges_never_overlap_or_change_target_bytes(
        prefix in proptest::collection::vec(any::<u8>(), 128..1024),
        old in proptest::collection::vec(any::<u8>(), 0..128),
        new in proptest::collection::vec(any::<u8>(), 0..128),
        suffix in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let base = [prefix.as_slice(), &old, &suffix].concat();
        let target = [prefix.as_slice(), &new, &suffix].concat();
        let encoded = prefix_suffix_delta(&base, &target).unwrap().unwrap();
        let delta = crate::delta::parse(&encoded, target.len()).unwrap();
        prop_assert_eq!(crate::delta::apply(&base, delta, || false).unwrap(), target);
    }
}
