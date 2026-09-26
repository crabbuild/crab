use std::sync::{
    Arc,
    atomic::{AtomicU16, AtomicUsize, Ordering},
};

use axum::{
    Router,
    http::{StatusCode, header},
    routing::post,
};
use bytes::Bytes;
use crab_cell_runtime::cell::application::ApplicationIdentity;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::control::{Control, Owner};
use crab_cell_runtime::identity::IncarnationId;
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, NamespaceId, SessionId, TenantId,
};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::node::{NodeAdvertisement, NodeCapacity, NodeDirectory};
use crab_cell_runtime::peer::{PeerRoundTrip, wire as peer_wire};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path as ObjectPath};

use super::{PROTOBUF_MEDIA_TYPE, PeerHttpRoundTrip};
use crate::{
    peer::now_ms,
    peer_tls::{LoadedPeerTls, PeerTlsIdentity, tests::IdentityFiles},
};

#[tokio::test]
async fn enrollment_io_releases_codec_capacity_and_obeys_the_received_budget() {
    use axum::extract::ConnectInfo;
    use crab_cell_runtime::peer::{PeerOperation, PeerPrincipal, PeerSigner};
    use crab_cell_runtime::{CellRuntime, SqlWorkerPool};
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use std::time::{Duration, Instant};

    let files = IdentityFiles::generate();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let loaded = LoadedPeerTls::load(&files.config(url::Url::parse(&endpoint).unwrap())).unwrap();
    let provider = ThrottledStore::new(
        InMemory::new(),
        ThrottleConfig {
            wait_get_per_call: Duration::from_secs(1),
            ..ThrottleConfig::default()
        },
    );
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(provider)),
        ObjectPath::from("enrollment"),
        [1; 16],
    );
    let release = Digest::from_bytes([2; 32]);
    let image = Digest::from_bytes([3; 32]);
    let session = SessionId::from_bytes([4; 16]);
    let directory = NodeDirectory::new(layout, loaded.fleet(), image, release);
    let now = now_ms().unwrap();
    directory
        .create(
            NodeAdvertisement::sign(
                crab_cell_runtime::identity::NodeId::from_bytes([5; 16]),
                session,
                endpoint.clone(),
                loaded.fleet(),
                loaded.certificate(),
                image,
                release,
                loaded.signing_key(),
                1,
                now,
                now + 15_000,
                vec![Digest::from_bytes([6; 32])],
                vec![1],
                crab_cell_runtime::node::NodeFailureDomain::default(),
                NodeCapacity::default(),
            )
            .unwrap(),
            now,
        )
        .await
        .unwrap();
    let runtime = CellRuntime::new(SqlWorkerPool::new(1, 1).unwrap(), 1 << 20, session).unwrap();
    let handler_runtime = runtime.clone();
    let app = Router::new().route(
        "/verify",
        post(
            move |ConnectInfo(identity): ConnectInfo<PeerTlsIdentity>, body: Bytes| {
                let runtime = handler_runtime.clone();
                let directory = directory.clone();
                async move {
                    let verification = crate::peer::verify_forwarded_request(
                        &runtime,
                        &directory,
                        &identity,
                        &body,
                        Instant::now(),
                    );
                    tokio::pin!(verification);
                    assert!(futures_util::poll!(&mut verification).is_pending());
                    let available = runtime.try_reserve_worker_job().unwrap();
                    assert!(
                        available.is_some(),
                        "provider I/O must not retain the only codec slot"
                    );
                    drop(available);
                    match verification.await {
                        Ok(_) => StatusCode::OK,
                        Err(crate::Error::Cell(crab_cell_runtime::Error::Deadline)) => {
                            StatusCode::GATEWAY_TIMEOUT
                        }
                        Err(error) => panic!("unexpected verification result: {error}"),
                    }
                }
            },
        ),
    );
    let tls = loaded.listener(listener);
    let (stop, done) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            tls,
            app.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async {
            let _ = done.await;
        })
        .await
        .unwrap();
    });
    let client = loaded
        .client_identity()
        .client(
            loaded.certificate(),
            loaded.signing_key().verifying_key().to_bytes(),
        )
        .unwrap();
    let signer = PeerSigner::new(session, release, loaded.signing_key().clone());
    for (remaining, expected) in [(5_000, StatusCode::OK), (10, StatusCode::GATEWAY_TIMEOUT)] {
        let now = now_ms().unwrap();
        let request = signer
            .sign(
                PeerPrincipal {
                    issuer: "https://identity.example".into(),
                    subject: "caller".into(),
                    actions: vec!["repository.read".into()],
                },
                now,
                now + 10_000,
                remaining,
                PeerOperation::Read(peer_wire::ReadRequest {
                    target: Some(peer_wire::Target {
                        tenant_id: vec![7; 16],
                        application_id: vec![1; 16],
                        namespace_id: vec![8; 16],
                        partition: b"repository".to_vec(),
                    }),
                    expected: None,
                    timeout_ms: remaining,
                    minimum: None,
                    operation: Some(peer_wire::read_request::Operation::Describe(true)),
                }),
            )
            .unwrap();
        let response = client
            .post(format!("{endpoint}/verify"))
            .body(request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert_eq!(runtime.stats().primitive_jobs(), 0);
    }
    stop.send(()).unwrap();
    server.await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn reloads_a_stale_owner_and_pins_mtls_identity() {
    let files = IdentityFiles::generate();
    let first_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_endpoint = format!(
        "https://localhost:{}",
        first_listener.local_addr().unwrap().port()
    );
    let second_endpoint = format!(
        "https://localhost:{}",
        second_listener.local_addr().unwrap().port()
    );
    let loaded =
        LoadedPeerTls::load(&files.config(url::Url::parse(&first_endpoint).unwrap())).unwrap();
    let store = Store::new(Arc::new(InMemory::new()));
    let layout = CellStorageLayout::new(store, ObjectPath::from("root"), [21; 16]);
    let image = Digest::from_bytes([22; 32]);
    let release = Digest::from_bytes([23; 32]);
    let directory = NodeDirectory::new(layout.clone(), loaded.fleet(), image, release);
    let first_session = SessionId::from_bytes([24; 16]);
    let second_session = SessionId::from_bytes([25; 16]);
    let now_ms = now_ms().unwrap();
    for (session, endpoint, progress) in [
        (first_session, first_endpoint.clone(), 1),
        (second_session, second_endpoint.clone(), 2),
    ] {
        directory
            .create(
                NodeAdvertisement::sign(
                    crab_cell_runtime::identity::NodeId::from_bytes(*session.as_bytes()),
                    session,
                    endpoint,
                    loaded.fleet(),
                    loaded.certificate(),
                    image,
                    release,
                    loaded.signing_key(),
                    progress,
                    now_ms,
                    now_ms + 15_000,
                    vec![Digest::from_bytes([26; 32])],
                    vec![1],
                    crab_cell_runtime::node::NodeFailureDomain::default(),
                    NodeCapacity {
                        free_memory_bytes: 1_000,
                        free_disk_bytes: 2_000,
                        job_credits: 1,
                        ..NodeCapacity::default()
                    },
                )
                .unwrap(),
                now_ms,
            )
            .await
            .unwrap();
    }
    let target = CellTarget::new(
        TenantId::from_bytes([27; 16]),
        ApplicationId::from_bytes([21; 16]),
        NamespaceId::from_bytes([28; 16]),
        b"repository",
    )
    .unwrap();
    let first_control = Control::initial(
        target.cell_id(),
        IncarnationId::from_bytes([29; 16]),
        Owner {
            session: first_session,
            endpoint: first_endpoint,
        },
        Digest::from_bytes([30; 32]),
        1,
    )
    .unwrap();
    let second_control = Control::initial(
        target.cell_id(),
        IncarnationId::from_bytes([29; 16]),
        Owner {
            session: second_session,
            endpoint: second_endpoint,
        },
        Digest::from_bytes([30; 32]),
        1,
    )
    .unwrap();
    layout
        .store()
        .put_overwrite(
            &layout.control_path(target.cell_id().as_bytes()),
            Bytes::from(first_control.encode().unwrap()),
        )
        .await
        .unwrap();

    let first_layout = layout.clone();
    let first_app = Router::new().route(
        "/internal/cells/v1/forward",
        post(move || {
            let layout = first_layout.clone();
            let control = second_control.clone();
            async move {
                layout
                    .store()
                    .put_overwrite(
                        &layout.control_path(control.cell.as_bytes()),
                        Bytes::from(control.encode().unwrap()),
                    )
                    .await
                    .unwrap();
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    );
    let expected = crab_cell_runtime::peer::encode_peer_reply(&peer_wire::PeerReply {
        outcome: Some(peer_wire::peer_reply::Outcome::Error(peer_wire::Error {
            code: peer_wire::error::Code::NotFound as i32,
            outcome: peer_wire::error::Outcome::NotStarted as i32,
            message: "test result".into(),
            retry_after_ms: 0,
            application_details: Vec::new(),
        })),
    })
    .unwrap();
    let reply_status = Arc::new(AtomicU16::new(StatusCode::OK.as_u16()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let second_app = Router::new().route(
        "/internal/cells/v1/forward",
        post({
            let expected = expected.clone();
            let reply_status = reply_status.clone();
            let attempts = attempts.clone();
            move || {
                let expected = expected.clone();
                let status = StatusCode::from_u16(reply_status.load(Ordering::Relaxed)).unwrap();
                attempts.fetch_add(1, Ordering::Relaxed);
                async move {
                    (
                        status,
                        [
                            (header::CONTENT_TYPE, PROTOBUF_MEDIA_TYPE),
                            (header::CACHE_CONTROL, "no-store"),
                        ],
                        expected,
                    )
                }
            }
        }),
    );
    let (first_stop, first_done) = tokio::sync::oneshot::channel();
    let (second_stop, second_done) = tokio::sync::oneshot::channel();
    let first_tls = loaded.listener(first_listener);
    let second_tls = loaded.listener(second_listener);
    let first_server = tokio::spawn(async move {
        axum::serve(
            first_tls,
            first_app.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = first_done.await;
        })
        .await
    });
    let second_server = tokio::spawn(async move {
        axum::serve(
            second_tls,
            second_app.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = second_done.await;
        })
        .await
    });
    let round_trip = PeerHttpRoundTrip::new(
        crate::peer::PeerOwnerHints::default(),
        ApplicationIdentity::new(
            TenantId::from_bytes([27; 16]),
            ApplicationId::from_bytes([21; 16]),
        ),
        CellAuthority::new(layout),
        directory,
        loaded.client_identity(),
        SessionId::from_bytes([31; 16]),
    );

    let actual = round_trip
        .send(target.clone(), vec![1, 2, 3], 5_000)
        .await
        .unwrap();

    assert_eq!(actual, expected);
    assert!(!round_trip.has_owner_hint(target.cell_id()));
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::FORBIDDEN,
        StatusCode::BAD_GATEWAY,
        StatusCode::BAD_REQUEST,
    ] {
        round_trip.owner(&target, false).await.unwrap();
        assert!(round_trip.has_owner_hint(target.cell_id()));
        // This fixture's control is still recovering without a published root;
        // it must not become a routable application description.
        assert!(
            round_trip
                .owner_hints
                .description(target.cell_id(), now_ms)
                .is_none()
        );
        reply_status.store(status.as_u16(), Ordering::Relaxed);
        let before = attempts.load(Ordering::Relaxed);
        let result = round_trip.send(target.clone(), vec![1, 2, 3], 5_000).await;
        if status == StatusCode::BAD_GATEWAY {
            assert!(matches!(
                result,
                Err(crab_cell_runtime::Error::PeerTransportUnknown { .. })
            ));
        } else {
            assert!(result.is_err());
        }
        assert_eq!(attempts.load(Ordering::Relaxed) - before, 1);
        assert!(!round_trip.has_owner_hint(target.cell_id()), "{status}");
    }
    first_stop.send(()).unwrap();
    second_stop.send(()).unwrap();
    first_server.await.unwrap().unwrap();
    second_server.await.unwrap().unwrap();
}
