//! Primitive operation telemetry as observed through the typed client.

use super::*;

struct RecordingPrimitiveTelemetry {
    operations: Mutex<
        Vec<(
            &'static str,
            PrimitiveOperationKind,
            PrimitiveOperationOutcome,
        )>,
    >,
}
impl Default for RecordingPrimitiveTelemetry {
    fn default() -> Self {
        Self {
            operations: Mutex::new(Vec::new()),
        }
    }
}
impl CellTelemetry for RecordingPrimitiveTelemetry {
    fn primitive_operation(
        &self,
        module: &'static str,
        kind: PrimitiveOperationKind,
        outcome: PrimitiveOperationOutcome,
        _elapsed: Duration,
    ) {
        self.operations
            .lock()
            .unwrap()
            .push((module, kind, outcome));
    }
}
#[tokio::test]
async fn primitive_operations_are_reported_for_local_and_peer_execution() {
    let fixture = fixture().await;
    let recording = Arc::new(RecordingPrimitiveTelemetry::default());
    fixture
        .runtime
        .as_ref()
        .expect("fixture runtime")
        .install_telemetry(recording.clone())
        .unwrap();
    let telemetry = fixture
        .runtime
        .as_ref()
        .expect("fixture runtime")
        .telemetry_handle();

    let client = CellClient::local_with_telemetry(
        Arc::clone(&fixture.registry),
        fixture.handle().clone(),
        telemetry.clone(),
    );
    client
        .command::<CreateComment>(&fixture.target, mutation(31), b"reported".to_vec())
        .await
        .unwrap();
    assert!(
        client
            .command::<RejectComment>(&fixture.target, mutation(32), b"moderated".to_vec())
            .await
            .is_err()
    );
    client
        .query::<CountComments>(&fixture.target, None, ())
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
    let dispatcher = Arc::new(
        PeerDispatcher::new(
            Arc::clone(&fixture.registry),
            Arc::new(LocalResolver {
                target: fixture.target.clone(),
                handle: fixture.handle().clone(),
            }),
            Arc::new(RepositoryAuthorizer),
        )
        .with_telemetry(telemetry),
    );
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
    peer.command::<CreateComment>(&fixture.target, mutation(33), b"peer".to_vec())
        .await
        .unwrap();

    let reported = recording.operations.lock().unwrap().clone();
    assert!(
        reported.contains(&(
            MODULE,
            PrimitiveOperationKind::Command,
            PrimitiveOperationOutcome::Success
        )),
        "local command outcome is reported: {reported:?}"
    );
    assert!(
        reported.contains(&(
            MODULE,
            PrimitiveOperationKind::Command,
            PrimitiveOperationOutcome::Rejected
        )),
        "rejected command outcome is reported: {reported:?}"
    );
    assert!(
        reported.contains(&(
            MODULE,
            PrimitiveOperationKind::Query,
            PrimitiveOperationOutcome::Success
        )),
        "query outcome is reported: {reported:?}"
    );
    assert!(
        reported
            .iter()
            .filter(
                |(_, kind, outcome)| *kind == PrimitiveOperationKind::Command
                    && *outcome == PrimitiveOperationOutcome::Success
            )
            .count()
            >= 2,
        "peer-routed command is reported beside the local one: {reported:?}"
    );

    fixture.handle().drain().await.unwrap();
}
