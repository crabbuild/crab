//! Authenticated effect delivery, source leases, and supervision.

use super::*;

#[tokio::test]
async fn authenticated_effect_delivery_publishes_once_and_resolves_from_inbox() {
    let fixture = fixture().await;
    let signer = PeerSigner::new(
        SessionId::from_bytes([12; 16]),
        fixture.registry.release_digest(),
        ed25519_dalek::SigningKey::from_bytes(&[13; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        SessionId::from_bytes([12; 16]),
        fixture.registry.release_digest(),
        signer.verifying_key(),
    ));
    let dispatcher = Arc::new(PeerDispatcher::new(
        Arc::clone(&fixture.registry),
        Arc::new(LocalResolver {
            target: fixture.target.clone(),
            handle: fixture.handle().clone(),
        }),
        Arc::new(RepositoryAuthorizer),
    ));
    let round_trip = Arc::new(LoopbackRoundTrip {
        verifier,
        dispatcher,
    });
    let principal = PeerPrincipal {
        issuer: "crab-runtime:test".into(),
        subject: "source-session".into(),
        actions: vec!["repository.issue.create".into()],
    };
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let source_cell = crab_cell_runtime::CellId::from_bytes([21; 32]);
    let source_incarnation = IncarnationId::from_bytes([22; 16]);
    let source_sequence = 9;
    let ordinal = 3;
    let effect_id = effect_id(source_cell, source_incarnation, source_sequence, ordinal);
    let identity = wire::EffectIdentity {
        effect_id: effect_id.to_vec(),
        source_cell: source_cell.as_bytes().to_vec(),
        source_incarnation: source_incarnation.as_bytes().to_vec(),
        source_sequence,
        ordinal,
        expires_at_ms: now_ms + 60_000,
    };
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"effect".to_vec().encode(&mut encoder).unwrap();
    let request = wire::EffectRequest {
        target: Some(wire::Target {
            tenant_id: fixture.target.tenant().as_bytes().to_vec(),
            application_id: fixture.target.application().as_bytes().to_vec(),
            namespace_id: fixture.target.namespace().as_bytes().to_vec(),
            partition: fixture.target.partition().to_vec(),
        }),
        destination_incarnation: Vec::new(),
        identity: Some(identity.clone()),
        operation: Some(wire::effect_request::Operation::CellCommand(
            wire::CellCommand {
                command_id: CreateComment::ID,
                codec_version: CreateComment::CODEC_VERSION,
                input: encoder.finish(),
            },
        )),
    };
    let operation_digest = effect_operation_digest(
        fixture.target.cell_id(),
        effect_id,
        &prost::Message::encode_to_vec(&request),
    );
    let claim = EffectClaim {
        effect_id,
        destination: fixture.target.cell_id(),
        operation: prost::Message::encode_to_vec(&request),
        operation_digest,
        attempt: 1,
        token: [23; 16],
        lease_until_ms: now_ms + 30_000,
        expires_at_ms: identity.expires_at_ms,
        created_sequence: source_sequence,
    };
    let client = EffectPeerClient::new(Arc::new(signer), principal, round_trip);
    let delivered = client.deliver(&claim, now_ms).await.unwrap();
    assert_eq!(delivered.commit_sequence(), 1);
    assert_eq!(client.deliver(&claim, now_ms + 1).await.unwrap(), delivered);
    assert_eq!(
        client.resolve(&claim, now_ms + 2).await.unwrap(),
        Resolution::Committed(delivered)
    );

    assert_eq!(
        CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone())
            .query::<CountComments>(&fixture.target, None, ())
            .await
            .unwrap()
            .output,
        1
    );
    fixture.handle().drain().await.unwrap();
}
#[tokio::test]
async fn typed_effect_source_publishes_claim_validation_ack_and_lost_lease() {
    let mut fixture = fixture().await;
    let source_cell = fixture.target.cell_id();
    let source_incarnation = fixture.incarnation;
    let source_sequence = 1;
    let ordinal = 0;
    let identity = mutation(30);
    let expected_effect_id = effect_id(source_cell, source_incarnation, source_sequence, ordinal);
    let expires_at_ms = identity.issued_at_ms + 60_000;
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"effect-source".to_vec().encode(&mut encoder).unwrap();
    let input = encoder.finish();
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());
    let committed = client
        .command::<EmitEffectComment>(&fixture.target, identity, input)
        .await
        .unwrap();
    let effect_id: [u8; 32] = committed.output.try_into().unwrap();
    assert_eq!(effect_id, expected_effect_id);
    assert_eq!(source_sequence, committed.receipt.commit_sequence);
    assert_eq!(expires_at_ms, identity.issued_at_ms + 60_000);

    let first_handle = fixture.take_handle();
    drop(first_handle);
    drop(fixture.runtime.take().expect("fixture runtime is present"));
    let stale = fixture.authority.load(source_cell).await.unwrap().unwrap();
    let successor = SessionId::from_bytes([15; 16]);
    let fenced =
        crate::support::fencing::fence_session(&fixture.layout, fixture.session, successor).await;
    let runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 4).unwrap(),
        4 * 1024 * 1024,
        successor,
    )
    .unwrap();
    let restored = runtime
        .takeover_restored(
            fixture.proof.clone(),
            fixture.replica.clone(),
            fixture.authority.clone(),
            stale,
            fenced.direct_takeover().unwrap(),
            crab_cell_runtime::recovery::manifest::RecoveryManifestStore::new(
                fixture.layout.clone(),
                Limits::default(),
            ),
            fixture._directory.path().join("effect-successor.sqlite"),
            Owner {
                session: successor,
                endpoint: "https://effect-successor.internal:8081".into(),
            },
        )
        .await
        .unwrap();

    let source = EffectSource::<RepositoryModule>::new(
        CellClient::local(Arc::clone(&fixture.registry), restored.clone()),
        fixture.target.clone(),
    );
    let claimed = source
        .claim(
            mutation(32),
            EffectClaimRequest {
                limit: 1,
                lease_ms: 5_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed.receipt.commit_sequence, 2);
    assert_eq!(claimed.output.len(), 1);
    assert_eq!(claimed.output[0].effect_id, effect_id);

    let claim = claimed.output[0].clone();
    let validated = source
        .validate(vec![claim.clone()], claimed.receipt)
        .await
        .unwrap();
    assert!(validated.output);
    assert_eq!(validated.receipt.commit_sequence, 2);

    let acknowledged = source
        .ack(mutation(33), claim.clone(), b"destination result".to_vec())
        .await
        .unwrap();
    assert_eq!(acknowledged.output, EffectLeaseOutcome::Delivered);
    assert_eq!(acknowledged.receipt.commit_sequence, 3);

    let lost = source.retry(mutation(34), claim).await.unwrap_err();
    assert!(matches!(
        lost,
        InvocationError::Rejected(ref outcome)
            if outcome.output == EffectLeaseOutcome::LeaseLost
                && outcome.receipt.commit_sequence == 4
    ));

    restored.drain().await.unwrap();
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn effect_supervisor_delivers_to_inbox_and_acknowledges_source() {
    let fixture = fixture().await;
    let source_sequence = 1;
    let identity = mutation(40);
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"supervised".to_vec().encode(&mut encoder).unwrap();
    let input = encoder.finish();
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());
    let committed = client
        .command::<EmitEffectComment>(&fixture.target, identity, input)
        .await
        .unwrap();
    assert_eq!(source_sequence, committed.receipt.commit_sequence);

    let signer = PeerSigner::new(
        SessionId::from_bytes([42; 16]),
        fixture.registry.release_digest(),
        ed25519_dalek::SigningKey::from_bytes(&[43; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        SessionId::from_bytes([42; 16]),
        fixture.registry.release_digest(),
        signer.verifying_key(),
    ));
    let dispatcher = Arc::new(PeerDispatcher::new(
        Arc::clone(&fixture.registry),
        Arc::new(LocalResolver {
            target: fixture.target.clone(),
            handle: fixture.handle().clone(),
        }),
        Arc::new(RepositoryAuthorizer),
    ));
    let peer = EffectPeerClient::new(
        Arc::new(signer),
        PeerPrincipal {
            issuer: "crab-runtime:test".into(),
            subject: "source-session".into(),
            actions: vec!["repository.issue.create".into()],
        },
        Arc::new(LoopbackRoundTrip {
            verifier,
            dispatcher,
        }),
    );
    assert!(
        fixture
            .registry
            .has_effect_runner(fixture.target.namespace())
    );
    let outcome = fixture
        .registry
        .run_effect_once(
            CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone()),
            fixture.target.clone(),
            peer,
            5_000,
        )
        .await
        .unwrap();
    assert!(
        matches!(
            &outcome,
            EffectRunOutcome::Delivered {
                destination: crab_cell_runtime::cell::executor::StoredOutcome::Success {
                    result,
                    commit_sequence: 3,
                },
                receipt: Receipt {
                    commit_sequence: 4,
                    ..
                },
            } if {
                let mut decoder = BoundedDecoder::new(result, 64).unwrap();
                let decoded = Vec::<u8>::decode(&mut decoder).unwrap();
                decoder.finish().unwrap();
                decoded == b"supervised"
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone())
            .query::<CountComments>(&fixture.target, None, ())
            .await
            .unwrap()
            .output,
        1
    );

    fixture.handle().drain().await.unwrap();
}
