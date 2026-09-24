//! Node-log enrollment, authority, and takeover.

use super::*;

#[tokio::test]
async fn node_log_enrollment_activation_and_coverage_are_authoritative() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let first = SessionId::from_bytes([2; 16]);
    let second = SessionId::from_bytes([3; 16]);
    let created = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    for member in [first, second] {
        directory
            .create(advertisement_for(member, &key, 1, NOW_MS), NOW_MS)
            .await
            .unwrap();
    }

    let enrolled = directory
        .recruit_log(&created, 4, 1, 3, NOW_MS + 1)
        .await
        .unwrap();
    let log = enrolled.advertisement().log().unwrap();
    assert_eq!(enrolled.advertisement().generation(), 2);
    assert_eq!(log.phase(), NodeLogPhase::Open);
    assert!(!log.active());
    assert_eq!(log.members(), [node(first), node(second)]);
    directory
        .authorize_log_append(leader, node(first), 4, 0, NOW_MS + 2)
        .await
        .unwrap();
    assert!(
        directory
            .authorize_log_append(leader, NodeId::from_bytes([8; 16]), 4, 0, NOW_MS + 2)
            .await
            .is_err()
    );

    let refreshed = directory
        .refresh(
            &created,
            advertisement_for(leader, &key, 2, NOW_MS + 1_000),
            NOW_MS + 1_000,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.advertisement().generation(), 3);
    assert_eq!(refreshed.advertisement().log(), Some(log));
    let active = directory
        .activate_log(&refreshed, NOW_MS + 1_001)
        .await
        .unwrap();
    assert!(active.advertisement().log().unwrap().active());
    let covered = directory
        .advance_log_coverage(&active, 27, NOW_MS + 1_002)
        .await
        .unwrap();
    assert!(directory.log_epoch_referenced(leader, 4).await.unwrap());
    assert_eq!(covered.advertisement().log().unwrap().tiered_through(), 27);
    // An append may have been queued before coverage advanced, but its wire
    // watermark must never authorize deletion beyond the persisted prefix.
    for watermark in [0, 26, 27] {
        directory
            .authorize_log_append(leader, node(first), 4, watermark, NOW_MS + 1_003)
            .await
            .unwrap();
    }
    assert!(matches!(
        directory
            .authorize_log_append(leader, node(first), 4, 28, NOW_MS + 1_003)
            .await,
        Err(Error::PeerAuthorization(_))
    ));
    directory
        .authorize_log_retire(leader, node(first), 4, 27, NOW_MS + 1_003)
        .await
        .unwrap();
    let gate =
        crate::node::log::DurabilityGate::new(leader, node(leader), 4, [node(first), node(second)])
            .unwrap();
    let ticket = gate.issue(27).unwrap();
    gate.prove_object(ticket).unwrap();
    let rotated = crate::node::log::rotate_node_log(
        &directory,
        Arc::new(UnavailableFollowerTransport),
        &covered,
        &gate,
        1,
        3,
        NOW_MS + 1_003,
    )
    .await
    .unwrap();
    let rotated_log = rotated.enrollment.advertisement().log().unwrap();
    assert_eq!(rotated_log.epoch(), 5);
    assert_eq!(rotated_log.phase(), NodeLogPhase::Open);
    assert!(!rotated_log.active());
    assert_eq!(rotated_log.tiered_through(), 0);
    assert_eq!(rotated.gate.issue(1).unwrap().first_sequence(), 1);
    assert!(
        directory
            .authorize_log_retire(leader, node(first), 4, 27, NOW_MS + 1_004)
            .await
            .is_err()
    );
    assert!(
        directory
            .withdraw(&rotated.enrollment, NOW_MS + 1_005)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn clean_node_log_close_clears_authority_before_session_withdrawal() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let member = SessionId::from_bytes([2; 16]);
    let created = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    directory
        .create(advertisement_for(member, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&created, 4, 1, 2, NOW_MS + 1)
        .await
        .unwrap();
    let active = directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();
    let covered = directory
        .advance_log_coverage(&active, 2, NOW_MS + 3)
        .await
        .unwrap();
    let gate =
        crate::node::log::DurabilityGate::new(leader, node(leader), 4, [node(member)]).unwrap();
    let ticket = gate.issue(2).unwrap();
    gate.prove_object(ticket).unwrap();

    let closed = crate::node::log::close_node_log(
        &directory,
        Arc::new(UnavailableFollowerTransport),
        &covered,
        &gate,
        NOW_MS + 4,
    )
    .await
    .unwrap();

    assert!(closed.advertisement().log().is_none());
    assert!(!directory.log_epoch_referenced(leader, 4).await.unwrap());
    assert!(
        directory
            .authorize_log_append(leader, node(member), 4, 0, NOW_MS + 5)
            .await
            .is_err()
    );
    directory.withdraw(&closed, NOW_MS + 5).await.unwrap();
    assert!(directory.load(leader, NOW_MS + 6).await.unwrap().is_none());
    assert!(!directory.log_epoch_referenced(leader, 4).await.unwrap());
}

#[tokio::test]
async fn request_takeover_does_not_claim_active_node_log() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    let leader = SessionId::from_bytes([1; 16]);
    let member = SessionId::from_bytes([2; 16]);
    let claimant = SessionId::from_bytes([3; 16]);
    let created = directory
        .create(advertisement_for(leader, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let member_record = directory
        .create(advertisement_for(member, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let enrolled = directory
        .recruit_log(&created, 7, 1, 2, NOW_MS + 1)
        .await
        .unwrap();
    directory.activate_log(&enrolled, NOW_MS + 2).await.unwrap();
    let claimant_record = directory
        .create(advertisement_for(claimant, &key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    for (record, session) in [(&member_record, member), (&claimant_record, claimant)] {
        directory
            .refresh(
                record,
                advertisement_for(session, &key, 2, NOW_MS + 9_000),
                NOW_MS + 9_000,
            )
            .await
            .unwrap();
    }

    assert!(matches!(
        directory
            .claim_expired_for_takeover(leader, claimant, NOW_MS + 10_000)
            .await,
        Err(Error::PendingPublication)
    ));
    assert_eq!(
        directory
            .recovery_candidates(claimant, NOW_MS + 10_000, 2)
            .await
            .unwrap(),
        [leader]
    );
    assert!(
        directory
            .takeover_proof(leader, claimant, NOW_MS + 10_000)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn request_requires_the_live_sessions_mtls_certificate_and_signing_key() {
    let key = SigningKey::from_bytes(&[7; 32]);
    let directory = directory();
    directory
        .create(advertisement(&key, 1, NOW_MS), NOW_MS)
        .await
        .unwrap();
    let target = CellTarget::new(
        TenantId::from_bytes([8; 16]),
        ApplicationId::from_bytes([9; 16]),
        NamespaceId::from_bytes([10; 16]),
        b"partition",
    )
    .unwrap();
    let signer = PeerSigner::new(
        SessionId::from_bytes([1; 16]),
        Digest::from_bytes([5; 32]),
        key,
    );
    let certificate_key = signer.verifying_key().to_bytes();
    let request = signer
        .sign(
            PeerPrincipal {
                issuer: "https://identity.example".into(),
                subject: "user".into(),
                actions: vec!["repository.read".into()],
            },
            NOW_MS,
            NOW_MS + 10_000,
            5_000,
            PeerOperation::Read(peer_wire::ReadRequest {
                target: Some(peer_wire::Target {
                    tenant_id: target.tenant().as_bytes().to_vec(),
                    application_id: target.application().as_bytes().to_vec(),
                    namespace_id: target.namespace().as_bytes().to_vec(),
                    partition: target.partition().to_vec(),
                }),
                timeout_ms: 5_000,
                minimum: None,
                operation: Some(peer_wire::read_request::Operation::Describe(true)),
            }),
        )
        .unwrap();

    assert!(
        directory
            .verify_peer_request(
                &request,
                Digest::from_bytes([3; 32]),
                certificate_key,
                NOW_MS + 1,
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        directory
            .verify_peer_request(
                &request,
                Digest::from_bytes([11; 32]),
                certificate_key,
                NOW_MS + 1,
            )
            .await,
        Err(Error::PeerAuthorization(_))
    ));
    assert!(matches!(
        directory
            .verify_peer_request(&request, Digest::from_bytes([3; 32]), [12; 32], NOW_MS + 1,)
            .await,
        Err(Error::PeerAuthorization(_))
    ));
}
