use std::fmt;

use crab_cell_runtime::*;

fn roundtrip<T: WireValue + PartialEq + fmt::Debug>(value: T) {
    let mut encoder = BoundedEncoder::new(1024 * 1024).unwrap();
    value.encode(&mut encoder).unwrap();
    let bytes = encoder.finish();
    let mut decoder = BoundedDecoder::new(&bytes, 1024 * 1024).unwrap();
    assert_eq!(T::decode(&mut decoder).unwrap(), value);
    decoder.finish().unwrap();
}

#[test]
fn activity_codecs_roundtrip_claim_lease_completion_and_validation() {
    let claim = ActivityClaim {
        run_id: [1; 16],
        activity_id: [2; 16],
        activity_type: "echo".into(),
        input: b"payload".to_vec(),
        definition_digest: crab_cell_runtime::Digest::from_bytes([3; 32]),
        attempt: 1,
        token: [4; 16],
        lease_until_ms: 10_000,
    };
    roundtrip(WorkflowActivityClaimRequest {
        limit: 1,
        lease_ms: 5_000,
    });
    roundtrip(vec![claim.clone()]);
    roundtrip(WorkflowActivityExtendRequest {
        claim: claim.clone(),
        extension_ms: 5_000,
    });
    roundtrip(ActivityLeaseOutcome::Extended {
        lease_until_ms: 15_000,
    });
    roundtrip(ActivityCompletion {
        run_id: claim.run_id,
        activity_id: claim.activity_id,
        attempt: claim.attempt,
        lease_token: claim.token,
        completion_token: [5; 16],
        result: b"done".to_vec(),
        failed: false,
        retryable: false,
    });
    roundtrip(ActivityCompletionOutcome::Retrying { due_at_ms: 20_000 });
    roundtrip(WorkflowActivityValidateRequest {
        claimed: vec![claim],
    });
}

#[test]
fn activity_decoder_rejects_invalid_claim_shape() {
    let claim = ActivityClaim {
        run_id: [1; 16],
        activity_id: [2; 16],
        activity_type: "echo".into(),
        input: Vec::new(),
        definition_digest: crab_cell_runtime::Digest::from_bytes([3; 32]),
        attempt: 0,
        token: [4; 16],
        lease_until_ms: 10_000,
    };
    let mut encoder = BoundedEncoder::new(256).unwrap();
    assert!(matches!(
        claim.encode(&mut encoder),
        Err(CodecError::Invalid("invalid activity claim"))
    ));
}
