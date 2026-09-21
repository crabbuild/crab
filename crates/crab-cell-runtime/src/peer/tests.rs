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

fn effect_identity() -> wire::EffectIdentity {
    let source_cell = crate::CellId::from_bytes([9; 32]);
    let source_incarnation = IncarnationId::from_bytes([10; 16]);
    let source_sequence = 3;
    let ordinal = 2;
    wire::EffectIdentity {
        effect_id: crate::effect_id(source_cell, source_incarnation, source_sequence, ordinal)
            .to_vec(),
        source_cell: source_cell.as_bytes().to_vec(),
        source_incarnation: source_incarnation.as_bytes().to_vec(),
        source_sequence,
        ordinal,
        expires_at_ms: NOW_MS + 5 * 60_000,
    }
}

fn effect() -> wire::EffectRequest {
    wire::EffectRequest {
        target: Some(target()),
        destination_incarnation: vec![8; 16],
        identity: Some(effect_identity()),
        operation: Some(wire::effect_request::Operation::CellCommand(
            wire::CellCommand {
                command_id: 7,
                codec_version: 1,
                input: b"effect-input".to_vec(),
            },
        )),
    }
}

fn migration() -> wire::MigrationRequest {
    wire::MigrationRequest {
        target: Some(target()),
        incarnation: vec![8; 16],
        from_code: vec![9; 32],
        from_schema: 1,
        to_code: vec![10; 32],
        to_schema: 2,
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
    assert_eq!(
        claimed_peer_session(&encoded).unwrap(),
        SessionId::from_bytes([1; 16])
    );
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
fn signed_effect_delivery_and_resolve_bind_derived_identity() {
    let signer = signer();
    let delivery = effect();
    let encoded = signer
        .sign(
            principal(),
            NOW_MS,
            NOW_MS + 60_000,
            30_000,
            PeerOperation::DeliverEffect(delivery.clone()),
        )
        .unwrap();
    let verified = verifier(&signer).verify(&encoded, NOW_MS + 1_000).unwrap();
    assert_eq!(verified.operation_tag(), 13);
    assert!(matches!(
        verified.operation(),
        Some(wire::peer_request::Operation::DeliverEffect(_))
    ));

    let digest = crate::effect_operation_digest(
        verified.target().cell_id(),
        <[u8; 32]>::try_from(effect_identity().effect_id).unwrap(),
        &delivery.encode_to_vec(),
    );
    let resolve = wire::EffectResolveRequest {
        target: delivery.target,
        destination_incarnation: delivery.destination_incarnation,
        identity: delivery.identity,
        operation_digest: digest.as_bytes().to_vec(),
    };
    let encoded = signer
        .sign(
            principal(),
            NOW_MS,
            NOW_MS + 60_000,
            30_000,
            PeerOperation::ResolveEffect(resolve),
        )
        .unwrap();
    assert_eq!(
        verifier(&signer)
            .verify(&encoded, NOW_MS + 1_000)
            .unwrap()
            .operation_tag(),
        14
    );

    let mut invalid = effect();
    invalid.identity.as_mut().unwrap().effect_id[0] ^= 1;
    assert!(
        signer
            .sign(
                principal(),
                NOW_MS,
                NOW_MS + 60_000,
                30_000,
                PeerOperation::DeliverEffect(invalid),
            )
            .is_err()
    );
}

#[test]
fn signed_migration_binds_source_and_successor_versions() {
    let signer = signer();
    let encoded = signer
        .sign(
            PeerPrincipal {
                issuer: "crab-runtime:test".into(),
                subject: "release-operator".into(),
                actions: vec!["cell.release.migrate".into()],
            },
            NOW_MS,
            NOW_MS + 60_000,
            30_000,
            PeerOperation::Migrate(migration()),
        )
        .unwrap();
    let verified = verifier(&signer).verify(&encoded, NOW_MS + 1_000).unwrap();
    assert_eq!(verified.operation_tag(), 15);
    assert!(matches!(
        verified.operation(),
        Some(wire::peer_request::Operation::Migrate(request))
            if request.from_schema == 1 && request.to_schema == 2
    ));

    let mut unchanged = migration();
    unchanged.to_code = unchanged.from_code.clone();
    unchanged.to_schema = unchanged.from_schema;
    assert!(
        signer
            .sign(
                principal(),
                NOW_MS,
                NOW_MS + 60_000,
                30_000,
                PeerOperation::Migrate(unchanged),
            )
            .is_err()
    );
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
fn primitive_specific_wire_fields_are_rejected() {
    let mut payload = mutation().encode_to_vec();
    encode_bytes_field(&mut payload, 11, b"unsupported-primitive").unwrap();
    assert!(super::protobuf::validate_operation(10, &payload).is_err());
}

#[test]
fn reordered_protobuf_payload_retains_exact_signature_binding() {
    let signer = signer();
    let mutation = mutation();
    let mut payload = Vec::new();
    let command = match mutation.operation.as_ref() {
        Some(wire::mutation_request::Operation::CellCommand(command)) => command.encode_to_vec(),
        None => unreachable!(),
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

#[test]
fn reply_codec_rejects_unknown_fields_and_invalid_enums() {
    let reply = wire::PeerReply {
        outcome: Some(wire::peer_reply::Outcome::Read(wire::ReadReply {
            receipt: None,
            result: Some(wire::read_reply::Result::Description(
                wire::CellDescription {
                    cell_id: vec![1; 32],
                    incarnation: vec![2; 16],
                    code: vec![3; 32],
                    schema: 1,
                },
            )),
        })),
    };
    let encoded = encode_peer_reply(&reply).unwrap();
    assert!(matches!(
        decode_peer_reply(&encoded).unwrap().outcome,
        Some(wire::peer_reply::Outcome::Read(_))
    ));

    let mut unknown = encoded;
    encode_varint_field(&mut unknown, 99, 1);
    assert!(decode_peer_reply(&unknown).is_err());

    let invalid = wire::PeerReply {
        outcome: Some(wire::peer_reply::Outcome::Error(wire::Error {
            code: 99,
            outcome: wire::error::Outcome::Rejected as i32,
            message: "invalid".into(),
            retry_after_ms: 0,
            application_details: Vec::new(),
        })),
    };
    assert!(encode_peer_reply(&invalid).is_err());

    let migration = wire::PeerReply {
        outcome: Some(wire::peer_reply::Outcome::Migration(wire::MigrationReply {
            description: Some(wire::CellDescription {
                cell_id: vec![1; 32],
                incarnation: vec![2; 16],
                code: vec![3; 32],
                schema: 2,
            }),
        })),
    };
    assert!(matches!(
        decode_peer_reply(&encode_peer_reply(&migration).unwrap())
            .unwrap()
            .outcome,
        Some(wire::peer_reply::Outcome::Migration(_))
    ));
}
