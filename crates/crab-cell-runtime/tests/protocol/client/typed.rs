//! Typed client command, query, digest, and conflict semantics.

use super::*;

fn verified_description(target: &CellTarget) -> VerifiedPeerRequest {
    let session = SessionId::from_bytes([12; 16]);
    let release = Digest::from_bytes([13; 32]);
    let signer = PeerSigner::new(
        session,
        release,
        ed25519_dalek::SigningKey::from_bytes(&[14; 32]),
    );
    let signed = signer
        .sign(
            PeerPrincipal {
                issuer: "https://identity.example".into(),
                subject: "alice".into(),
                actions: vec!["repository.issue.create".into()],
            },
            1_000,
            61_000,
            30_000,
            crab_cell_runtime::peer::PeerOperation::Read(wire::ReadRequest {
                target: Some(wire::Target {
                    tenant_id: target.tenant().as_bytes().to_vec(),
                    application_id: target.application().as_bytes().to_vec(),
                    namespace_id: target.namespace().as_bytes().to_vec(),
                    partition: target.partition().to_vec(),
                }),
                timeout_ms: 30_000,
                minimum: None,
                operation: Some(wire::read_request::Operation::Describe(true)),
            }),
        )
        .unwrap();
    PeerVerifier::new(session, release, signer.verifying_key())
        .verify(&signed, 1_000)
        .unwrap()
}

#[tokio::test]
async fn resolved_peer_dispatch_uses_the_receivers_handle() {
    let fixture = fixture().await;
    let other = CellTarget::new(
        fixture.target.tenant(),
        fixture.target.application(),
        fixture.target.namespace(),
        b"another-repository",
    )
    .unwrap();
    let dispatcher = PeerDispatcher::new(
        Arc::clone(&fixture.registry),
        Arc::new(LocalResolver {
            target: other,
            handle: fixture.handle().clone(),
        }),
        Arc::new(RepositoryAuthorizer),
    );
    let request = verified_description(&fixture.target);
    let reply = dispatcher
        .dispatch_resolved(&request, 1_000, Ok(fixture.handle().clone()))
        .await;
    assert!(matches!(
        reply.outcome,
        Some(wire::peer_reply::Outcome::Read(_))
    ));
    fixture.handle().drain().await.unwrap();
}

#[tokio::test]
async fn resolved_peer_dispatch_rejects_a_handle_for_another_target() {
    let fixture = fixture().await;
    let other = CellTarget::new(
        fixture.target.tenant(),
        fixture.target.application(),
        fixture.target.namespace(),
        b"another-repository",
    )
    .unwrap();
    let dispatcher = PeerDispatcher::new(
        Arc::clone(&fixture.registry),
        Arc::new(LocalResolver {
            target: fixture.target.clone(),
            handle: fixture.handle().clone(),
        }),
        Arc::new(RepositoryAuthorizer),
    );
    let request = verified_description(&other);
    let reply = dispatcher
        .dispatch_resolved(&request, 1_000, Ok(fixture.handle().clone()))
        .await;
    assert!(matches!(
        reply.outcome,
        Some(wire::peer_reply::Outcome::Error(_))
    ));
    fixture.handle().drain().await.unwrap();
}

#[tokio::test]
async fn typed_client_publishes_replays_rejections_and_receipted_reads() {
    let fixture = fixture().await;
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());
    let identity = mutation_identity(7);

    let committed = client
        .command::<CreateComment>(&fixture.target, identity, b"first".to_vec())
        .await
        .unwrap();
    assert_eq!(committed.output, b"first");
    assert_eq!(committed.receipt.commit_sequence, 1);

    let replay = client
        .command::<CreateComment>(&fixture.target, identity, b"first".to_vec())
        .await
        .unwrap();
    assert_eq!(replay, committed);

    let rejected = client
        .command::<RejectComment>(&fixture.target, mutation_identity(8), b"hidden".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(
        rejected,
        InvocationError::Rejected(ref outcome)
            if outcome.output == b"moderated" && outcome.receipt.commit_sequence == 2
    ));

    let observed = client
        .query::<CountComments>(&fixture.target, Some(committed.receipt), ())
        .await
        .unwrap();
    assert_eq!(observed.output, 1);
    assert_eq!(observed.receipt.commit_sequence, 2);

    fixture.handle().drain().await.unwrap();
}
#[tokio::test]
async fn state_stream_serializes_local_queries_across_a_new_commit() {
    let fixture = fixture().await;
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());
    let mut stream = client
        .open_state_stream::<CountComments>(
            &fixture.target,
            std::time::Instant::now() + std::time::Duration::from_secs(5),
        )
        .await
        .unwrap();

    let first = stream.emit(()).await.unwrap();
    assert_eq!(first.output, 0);
    let committed = client
        .command::<CreateComment>(&fixture.target, mutation_identity(18), b"streamed".to_vec())
        .await
        .unwrap();
    let second = stream.emit(()).await.unwrap();
    assert_eq!(second.output, 1);
    assert!(second.receipt.commit_sequence >= committed.receipt.commit_sequence);
    assert_eq!(stream.last_receipt(), Some(second.receipt));

    stream.finish();
    fixture.handle().drain().await.unwrap();
}
#[tokio::test]
async fn local_and_peer_command_share_digest_dedup_and_query_state() {
    let fixture = fixture().await;
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());
    let identity = mutation_identity(14);
    let committed = client
        .command::<CreateComment>(&fixture.target, identity, b"same".to_vec())
        .await
        .unwrap();

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
    let peer = CellClient::peer(
        Arc::clone(&fixture.registry),
        Arc::new(signer),
        PeerPrincipal {
            issuer: "https://identity.example".into(),
            subject: "alice".into(),
            actions: vec!["repository.issue.create".into()],
        },
        Arc::new(LoopbackRoundTrip {
            verifier,
            dispatcher,
        }),
    );
    assert_eq!(
        peer.command::<CreateComment>(&fixture.target, identity, b"same".to_vec())
            .await
            .unwrap(),
        committed
    );
    let observed = peer
        .query::<CountComments>(&fixture.target, Some(committed.receipt), ())
        .await
        .unwrap();
    assert_eq!(observed.output, 1);
    assert_eq!(observed.receipt.commit_sequence, 1);

    fixture.handle().drain().await.unwrap();
}
#[tokio::test]
async fn typed_client_rejects_conflicting_identity_receipt_and_module_before_execution() {
    let fixture = fixture().await;
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());
    let identity = mutation_identity(9);
    let committed = client
        .command::<CreateComment>(&fixture.target, identity, b"first".to_vec())
        .await
        .unwrap();

    assert!(matches!(
        client
            .command::<CreateComment>(&fixture.target, identity, b"different".to_vec())
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::RequestConflict
        ))
    ));
    assert!(matches!(
        client
            .query::<CountComments>(
                &fixture.target,
                Some(Receipt {
                    incarnation: IncarnationId::from_bytes([99; 16]),
                    ..committed.receipt
                }),
                (),
            )
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Command("minimum receipt does not match Cell")
        ))
    ));
    assert!(matches!(
        client
            .command::<WrongModuleCommand>(&fixture.target, mutation_identity(10), ())
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Registry("operation module does not own namespace")
        ))
    ));
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    assert!(matches!(
        client
            .command::<CreateComment>(
                &fixture.target,
                MutationIdentity {
                    request_id: RequestId::from_bytes([11; 16]),
                    issued_at_ms: now_ms - 2,
                    expires_at_ms: now_ms - 1,
                },
                b"expired".to_vec(),
            )
            .await,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Command("invalid mutation identity lifetime")
        ))
    ));

    fixture.handle().drain().await.unwrap();
}
#[tokio::test]
async fn command_effects_require_declared_same_application_targets() {
    let fixture = fixture().await;
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());

    let result = client
        .command::<EmitUndeclaredEffect>(
            &fixture.target,
            mutation_identity(50),
            b"foreign".to_vec(),
        )
        .await;
    assert!(matches!(
        result,
        Err(InvocationError::NotStarted(
            crab_cell_runtime::Error::Command("effect target is not declared")
        ))
    ));
    assert_eq!(
        client
            .query::<CountComments>(&fixture.target, None, ())
            .await
            .unwrap()
            .output,
        0
    );

    fixture.handle().drain().await.unwrap();
}
#[tokio::test]
async fn invalid_typed_result_preserves_the_published_receipt() {
    let fixture = fixture().await;
    let client = CellClient::local(Arc::clone(&fixture.registry), fixture.handle().clone());

    let result = client
        .command::<InvalidResultComment>(&fixture.target, mutation_identity(12), b"stored".to_vec())
        .await;
    assert!(matches!(
        result,
        Err(InvocationError::InvalidPublishedResult {
            receipt: Receipt {
                commit_sequence: 1,
                ..
            },
            ..
        })
    ));
    assert_eq!(
        client
            .query::<CountComments>(&fixture.target, None, ())
            .await
            .unwrap()
            .output,
        1
    );

    fixture.handle().drain().await.unwrap();
}
#[test]
fn command_digest_is_canonical_and_binds_incarnation_identity_and_input() {
    let description = CellDescription {
        cell: crab_cell_runtime::CellId::from_bytes([1; 32]),
        incarnation: IncarnationId::from_bytes([2; 16]),
        code: Digest::from_bytes([3; 32]),
        schema: 1,
    };
    let identity = MutationIdentity {
        request_id: RequestId::from_bytes([4; 16]),
        issued_at_ms: 5,
        expires_at_ms: 6,
    };
    let mut encoder = BoundedEncoder::new(64).unwrap();
    b"input".to_vec().encode(&mut encoder).unwrap();
    let input = encoder.finish();
    let digest = command_operation_digest::<CreateComment>(description, identity, &input).unwrap();
    assert_eq!(
        *digest.as_bytes(),
        [
            220, 198, 211, 0, 239, 12, 190, 71, 247, 29, 242, 5, 254, 27, 25, 124, 91, 249, 225,
            28, 47, 142, 161, 104, 78, 125, 152, 205, 181, 67, 244, 134,
        ]
    );
    assert_ne!(
        digest,
        command_operation_digest::<CreateComment>(
            CellDescription {
                incarnation: IncarnationId::from_bytes([9; 16]),
                ..description
            },
            identity,
            &input,
        )
        .unwrap()
    );
    assert_ne!(
        digest,
        command_operation_digest::<CreateComment>(description, identity, b"other").unwrap()
    );
}
