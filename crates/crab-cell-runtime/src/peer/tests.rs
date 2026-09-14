use super::*;

const NOW_MS: i64 = 1_000_000;

fn signer() -> PeerSigner {
    PeerSigner::new(
        SessionId::from_bytes([1; 16]),
        Digest::from_bytes([2; 32]),
        SigningKey::from_bytes(&[3; 32]),
    )
}

fn principal() -> PeerPrincipal {
    PeerPrincipal {
        issuer: "https://identity.example".into(),
        subject: "alice".into(),
        actions: vec!["repository.issue.create".into()],
    }
}

fn target() -> wire::Target {
    wire::Target {
        tenant_id: vec![4; 16],
        application_id: vec![5; 16],
        namespace_id: vec![6; 16],
        partition: b"repository-42".to_vec(),
    }
}

fn mutation() -> wire::MutationRequest {
    wire::MutationRequest {
        target: Some(target()),
        identity: Some(wire::MutationIdentity {
            request_id: vec![7; 16],
            incarnation: vec![8; 16],
            issued_at_ms: NOW_MS,
            expires_at_ms: NOW_MS + 60_000,
        }),
        timeout_ms: 30_000,
        operation: Some(wire::mutation_request::Operation::CellCommand(
            wire::CellCommand {
                command_id: 7,
                codec_version: 1,
                input: b"command-input".to_vec(),
            },
        )),
    }
}

fn verifier(signer: &PeerSigner) -> PeerVerifier {
    PeerVerifier::new(
        SessionId::from_bytes([1; 16]),
        Digest::from_bytes([2; 32]),
        signer.verifying_key(),
    )
}

#[test]
fn signed_request_verifies_and_forward_preserves_payload() {
    let signer = signer();
    let encoded = signer
        .sign(
            principal(),
            NOW_MS,
            NOW_MS + 60_000,
            30_000,
            PeerOperation::Mutate(mutation()),
        )
        .unwrap();
    let verifier = verifier(&signer);
    let verified = verifier.verify(&encoded, NOW_MS + 1_000).unwrap();
    assert_eq!(verified.hop_count(), 1);
    assert_eq!(verified.remaining_ms(), 30_000);
    assert!(verified.permits("repository.issue.create"));
    assert_eq!(verified.target().partition(), b"repository-42");

    let forwarded = verified.forward(20_000).unwrap();
    let forwarded = verifier.verify(&forwarded, NOW_MS + 2_000).unwrap();
    assert_eq!(forwarded.hop_count(), 2);
    assert_eq!(forwarded.remaining_ms(), 20_000);
    assert!(forwarded.forward(10_000).is_err());
}

#[test]
fn payload_tampering_fails_before_dispatch() {
    let signer = signer();
    let mut encoded = signer
        .sign(
            principal(),
            NOW_MS,
            NOW_MS + 60_000,
            30_000,
            PeerOperation::Mutate(mutation()),
        )
        .unwrap();
    let last = encoded.last_mut().unwrap();
    *last ^= 1;
    assert!(verifier(&signer).verify(&encoded, NOW_MS + 1_000).is_err());
}

#[test]
fn unknown_and_duplicate_fields_are_rejected() {
    let signer = signer();
    let encoded = signer
        .sign(
            principal(),
            NOW_MS,
            NOW_MS + 60_000,
            30_000,
            PeerOperation::Mutate(mutation()),
        )
        .unwrap();
    let verifier = verifier(&signer);

    let mut unknown = encoded.clone();
    encode_varint_field(&mut unknown, 99, 1);
    assert!(verifier.verify(&unknown, NOW_MS + 1_000).is_err());

    let mut duplicate = encoded;
    encode_varint_field(&mut duplicate, 1, 1);
    assert!(verifier.verify(&duplicate, NOW_MS + 1_000).is_err());
}

#[test]
fn reordered_protobuf_payload_retains_exact_signature_binding() {
    let signer = signer();
    let mutation = mutation();
    let mut payload = Vec::new();
    let command = match mutation.operation.as_ref().unwrap() {
        wire::mutation_request::Operation::CellCommand(command) => command.encode_to_vec(),
        _ => unreachable!(),
    };
    encode_bytes_field(&mut payload, 10, &command).unwrap();
    encode_bytes_field(
        &mut payload,
        2,
        &mutation.identity.as_ref().unwrap().encode_to_vec(),
    )
    .unwrap();
    encode_bytes_field(
        &mut payload,
        1,
        &mutation.target.as_ref().unwrap().encode_to_vec(),
    )
    .unwrap();
    encode_varint_field(&mut payload, 3, u64::from(mutation.timeout_ms));

    let digest = blake3::hash(&payload);
    let principal = principal();
    let mut authorization = wire::PeerAuthorization {
        origin_session: vec![1; 16],
        principal_issuer: principal.issuer,
        principal_subject: principal.subject,
        actions: principal.actions,
        release_digest: vec![2; 32],
        issued_at_ms: NOW_MS,
        expires_at_ms: NOW_MS + 60_000,
        payload_digest: digest.as_bytes().to_vec(),
        signature: Vec::new(),
    };
    authorization.signature = signer
        .key
        .sign(&signing_bytes(10, &authorization).unwrap())
        .to_bytes()
        .to_vec();
    let encoded = encode_request(authorization, 1, 30_000, 10, &payload).unwrap();
    let verified = verifier(&signer).verify(&encoded, NOW_MS + 1_000).unwrap();
    assert_eq!(verified.operation_tag(), 10);
}

#[test]
fn unsorted_actions_and_expired_authorization_are_rejected() {
    let signer = signer();
    let mut invalid = principal();
    invalid.actions = vec!["z".into(), "a".into()];
    assert!(
        signer
            .sign(
                invalid,
                NOW_MS,
                NOW_MS + 60_000,
                30_000,
                PeerOperation::Mutate(mutation()),
            )
            .is_err()
    );

    let encoded = signer
        .sign(
            principal(),
            NOW_MS,
            NOW_MS + 60_000,
            30_000,
            PeerOperation::Mutate(mutation()),
        )
        .unwrap();
    assert!(verifier(&signer).verify(&encoded, NOW_MS + 60_000).is_err());
}
