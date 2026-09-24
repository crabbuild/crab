//! Properties of the qualification decoders and the receipt gate.
//!
//! Profile, workload, and receipt bytes arrive from evidence files, CI
//! artifacts, and other machines, so the contracts that matter beyond the
//! fixtures are that a decode never panics and that the canonical form is
//! stable: re-encoding an accepted artifact and decoding it again must land on
//! the same bytes.

use crab_cell_runtime::identity::Digest;
use crab_cell_runtime::qualification::cluster::validate_cluster_receipt;
use crab_cell_runtime::qualification::{QualificationProfile, QualificationWorkload};
use proptest::prelude::*;

const SOURCE_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
const IMAGE: Digest = Digest::from_bytes([0xaa; 32]);

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    /// An accepted profile has a canonical form that re-parses and re-encodes
    /// unchanged, whatever bytes it arrived as.
    #[test]
    fn profile_decoding_has_a_stable_canonical_form(
        bytes in prop::collection::vec(any::<u8>(), 0..1_024),
    ) {
        if let Ok(profile) = QualificationProfile::decode(&bytes) {
            let encoded = profile.encode().expect("an accepted profile encodes");
            let reparsed = QualificationProfile::decode(&encoded)
                .expect("the canonical form of an accepted profile decodes");
            prop_assert_eq!(
                reparsed.encode().expect("the canonical form re-encodes"),
                encoded
            );
            prop_assert!(reparsed == profile);
        }
    }

    /// The same for the canonical workload artifact.
    #[test]
    fn workload_decoding_has_a_stable_canonical_form(
        bytes in prop::collection::vec(any::<u8>(), 0..1_024),
    ) {
        if let Ok(workload) = QualificationWorkload::decode(&bytes) {
            let encoded = workload.encode().expect("an accepted workload encodes");
            let reparsed = QualificationWorkload::decode(&encoded)
                .expect("the canonical form of an accepted workload decodes");
            prop_assert_eq!(
                reparsed.encode().expect("the canonical form re-encodes"),
                encoded
            );
        }
    }

    /// The release-evidence gate parses untrusted bytes: it may accept or
    /// reject, but it must return rather than panic.
    #[test]
    fn the_cluster_receipt_gate_is_total(
        bytes in prop::collection::vec(any::<u8>(), 0..4_096),
        published in any::<bool>(),
    ) {
        let _ = validate_cluster_receipt(&bytes, SOURCE_REVISION, IMAGE, published);
    }
}
