use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    http::{StatusCode, header},
    routing::post,
};
use bytes::Bytes;
use cellule_runtime::CellStorageLayout;
use cellule_runtime::{
    ApplicationId, ApplicationIdentity, CellAuthority, CellTarget, Control, Digest,
    Error as CellError, IncarnationId, NamespaceId, NodeAdvertisement, NodeCapacity, NodeDirectory,
    Owner, PeerRoundTrip, SessionId, TenantId, peer_wire,
};
use crab_storage::Store;
use object_store::{memory::InMemory, path::Path as ObjectPath};

use super::{PROTOBUF_MEDIA_TYPE, PeerHttpRoundTrip};
use crate::{
    peer::now_ms,
    peer_tls::{LoadedPeerTls, PeerTlsIdentity, tests::IdentityFiles},
};

#[tokio::test]
async fn preserves_ambiguous_status_and_retries_explicit_not_started() {
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
    let layout = CellStorageLayout::new(
        store.for_cellule().unwrap(),
        ObjectPath::from("root"),
        [21; 16],
    );
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
                    cellule_runtime::NodeId::from_bytes(*session.as_bytes()),
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
                    cellule_runtime::NodeFailureDomain::default(),
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

    let retry_reply = cellule_runtime::encode_peer_reply(&peer_wire::PeerReply {
        outcome: Some(peer_wire::peer_reply::Outcome::Error(peer_wire::Error {
            code: peer_wire::error::Code::Unavailable as i32,
            outcome: peer_wire::error::Outcome::NotStarted as i32,
            message: "owner moved".into(),
            retry_after_ms: 0,
            application_details: Vec::new(),
        })),
    })
    .unwrap();
    let first_layout = layout.clone();
    let first_app = Router::new().route(
        "/internal/cells/v1/forward",
        post(move |body: Bytes| {
            let layout = first_layout.clone();
            let control = second_control.clone();
            let retry_reply = retry_reply.clone();
            async move {
                layout
                    .store()
                    .put_overwrite(
                        &layout.control_path(control.cell.as_bytes()),
                        Bytes::from(control.encode().unwrap()),
                    )
                    .await
                    .unwrap();
                let (status, reply) = if body == Bytes::from_static(b"retry") {
                    (StatusCode::OK, retry_reply)
                } else {
                    (StatusCode::SERVICE_UNAVAILABLE, Vec::new())
                };
                (
                    status,
                    [
                        (header::CONTENT_TYPE, PROTOBUF_MEDIA_TYPE),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                    reply,
                )
            }
        }),
    );
    let expected = cellule_runtime::encode_peer_reply(&peer_wire::PeerReply {
        outcome: Some(peer_wire::peer_reply::Outcome::Error(peer_wire::Error {
            code: peer_wire::error::Code::NotFound as i32,
            outcome: peer_wire::error::Outcome::NotStarted as i32,
            message: "test result".into(),
            retry_after_ms: 0,
            application_details: Vec::new(),
        })),
    })
    .unwrap();
    let second_calls = Arc::new(AtomicUsize::new(0));
    let second_app = Router::new().route(
        "/internal/cells/v1/forward",
        post({
            let expected = expected.clone();
            let second_calls = Arc::clone(&second_calls);
            move || {
                let expected = expected.clone();
                let second_calls = Arc::clone(&second_calls);
                async move {
                    second_calls.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
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
        ApplicationIdentity::new(
            TenantId::from_bytes([27; 16]),
            ApplicationId::from_bytes([21; 16]),
        ),
        CellAuthority::new(layout.clone()),
        directory,
        loaded.client_identity(),
        SessionId::from_bytes([31; 16]),
    );

    let ambiguous = round_trip
        .send(target.clone(), b"unknown".to_vec(), 5_000)
        .await;
    assert!(matches!(
        ambiguous,
        Err(CellError::PeerTransportUnknown { .. })
    ));
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);

    layout
        .store()
        .put_overwrite(
            &layout.control_path(target.cell_id().as_bytes()),
            Bytes::from(first_control.encode().unwrap()),
        )
        .await
        .unwrap();
    let actual = round_trip
        .send(target, b"retry".to_vec(), 5_000)
        .await
        .unwrap();

    assert_eq!(actual, expected);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    first_stop.send(()).unwrap();
    second_stop.send(()).unwrap();
    first_server.await.unwrap().unwrap();
    second_server.await.unwrap().unwrap();
}
