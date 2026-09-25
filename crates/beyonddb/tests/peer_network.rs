#![cfg(unix)]

use std::{
    collections::HashMap,
    process::Command,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use aws_credential_types::Credentials;
use aws_sdk_dynamodb::types::AttributeValue as AwsAttributeValue;
use beyonddb::{
    Beyonddb, BeyonddbPeerScope, CellAuthorizationStore, CellCredentialStore,
    CellInitialPartitionProvisioner, CreateTable, CreateTableOutcome, DescribeTable, Json,
    NodeLeasePublisher, ReadTableRoute, TableSpec, account_target, build_http_state,
    build_peer_client, peer_router,
};
use crab_cell_app::CellApplication;
use crab_cell_host::{CellNode, CellNodeBuilder};
use crab_cell_peer_http::{LoadedPeerTls, PeerHttpRoundTrip, PeerTlsIdentity};
use crab_cell_runtime::client::CellClient;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::ltx::{CellStorageLayout, DiskBudget, Host};
use crab_cell_runtime::node::{NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain};
use crab_cell_runtime::peer::{PeerPrincipal, PeerSigner};
use crab_cell_runtime::registry::BuildDescriptor;
use crab_cell_runtime::{
    SqlWorkerPool,
    identity::{Digest, NodeId, SessionId},
};
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use extenddb_auth::StoredCredential;
use extenddb_core::types::{
    AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
};
use object_store::memory::InMemory;
use tokio_util::sync::CancellationToken;

fn run(command: &mut Command) {
    assert!(command.output().unwrap().status.success());
}

fn tls_files(
    directory: &std::path::Path,
) -> (
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let ca_key = directory.join("ca.key");
    let ca = directory.join("ca.crt");
    run(Command::new("openssl")
        .args(["genpkey", "-algorithm", "ED25519", "-out"])
        .arg(&ca_key));
    run(Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-new",
            "-days",
            "1",
            "-subj",
            "/CN=BeyondDB Test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-key",
        ])
        .arg(&ca_key)
        .arg("-out")
        .arg(&ca));
    let (owner_certificate, owner_key) = peer_tls_files(directory, "owner", &ca, &ca_key);
    let (remote_certificate, remote_key) = peer_tls_files(directory, "remote", &ca, &ca_key);
    (
        owner_certificate,
        owner_key,
        remote_certificate,
        remote_key,
        ca,
    )
}

fn peer_tls_files(
    directory: &std::path::Path,
    name: &str,
    ca: &std::path::Path,
    ca_key: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let key = directory.join(format!("{name}.key"));
    let request = directory.join(format!("{name}.csr"));
    let certificate = directory.join(format!("{name}.crt"));
    let extensions = directory.join(format!("{name}.ext"));
    run(Command::new("openssl")
        .args(["genpkey", "-algorithm", "ED25519", "-out"])
        .arg(&key));
    run(Command::new("openssl")
        .args(["req", "-new", "-subj", "/CN=localhost", "-key"])
        .arg(&key)
        .arg("-out")
        .arg(&request));
    std::fs::write(&extensions, "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost\n").unwrap();
    run(Command::new("openssl")
        .args(["x509", "-req", "-days", "1", "-CAcreateserial", "-in"])
        .arg(&request)
        .arg("-CA")
        .arg(ca)
        .arg("-CAkey")
        .arg(ca_key)
        .arg("-extfile")
        .arg(&extensions)
        .arg("-out")
        .arg(&certificate));
    (certificate, key)
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

async fn start_node(
    application: Arc<crab_cell_app::CompiledApplication>,
    directory: NodeDirectory,
    session: SessionId,
    endpoint: String,
    certificate: Digest,
    signing_key: SigningKey,
    node_byte: u8,
    cancellation: CancellationToken,
) -> CellNode {
    let node = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build()
        .unwrap();
    let tasks = node
        .install_task_group(cancellation.clone(), CancellationToken::new())
        .unwrap();
    let fleet = directory.fleet();
    let release = application.registry().release_digest();
    let published = NodeLeasePublisher::new(directory, move |now, expires| {
        NodeAdvertisement::sign(
            NodeId::from_bytes([node_byte; 16]),
            session,
            endpoint.clone(),
            fleet,
            certificate,
            Digest::from_bytes([90; 32]),
            release,
            &signing_key,
            1,
            now,
            expires,
            vec![Digest::from_bytes([91; 32])],
            vec![1],
            NodeFailureDomain::default(),
            NodeCapacity {
                free_memory_bytes: 16 * 1024 * 1024,
                free_disk_bytes: 1 << 30,
                job_credits: 8,
                ..NodeCapacity::default()
            },
        )
    })
    .publish()
    .await
    .unwrap();
    node.install_node_lease_for_startup(published.guard())
        .unwrap();
    tasks
        .spawn(async move { published.run(&cancellation).await })
        .unwrap();
    node.start().unwrap();
    node
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_sdk_request_routes_across_two_owners_and_survives_restart() {
    let files = tempfile::tempdir().unwrap();
    let (owner_certificate, owner_key, remote_certificate, remote_key, ca) =
        tls_files(files.path());
    let tls = LoadedPeerTls::load(&owner_certificate, &owner_key, &ca, "localhost").unwrap();
    let owner_certificate_digest = tls.certificate();
    let owner_signing_key = tls.signing_key().clone();
    let client_tls =
        LoadedPeerTls::load(&remote_certificate, &remote_key, &ca, "localhost").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner_endpoint = format!("https://{}", listener.local_addr().unwrap());
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "peer-network-test".into(),
            cargo_lock_digest: Digest::from_bytes([92; 32]),
        })
        .unwrap(),
    );
    let account = account_target("123456789012").unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("beyonddb-peer-network-test"),
        *account.application().as_bytes(),
    );
    let peer_directory = NodeDirectory::new(
        layout.clone(),
        tls.fleet(),
        Digest::from_bytes([90; 32]),
        application.registry().release_digest(),
    );
    let owner_session = SessionId::from_bytes([93; 16]);
    let owner_lease = CancellationToken::new();
    let owner = start_node(
        Arc::clone(&application),
        peer_directory.clone(),
        owner_session,
        owner_endpoint.clone(),
        owner_certificate_digest,
        owner_signing_key,
        95,
        owner_lease.clone(),
    )
    .await;
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            owner.runtime(),
            Arc::clone(&application),
            layout.clone(),
            owner_session,
            owner_endpoint,
            files.path().join("data"),
        )
        .unwrap(),
    );
    provisioner.admit_account("123456789012").await.unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let router = peer_router(&owner, layout.clone(), peer_directory.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            tls.listener(listener),
            router.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .await
    });
    let remote_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_endpoint = format!("https://{}", remote_listener.local_addr().unwrap());
    let remote_session = SessionId::from_bytes([96; 16]);
    let remote_signing_key = client_tls.signing_key().clone();
    let remote_lease = CancellationToken::new();
    let remote = start_node(
        Arc::clone(&application),
        peer_directory.clone(),
        remote_session,
        remote_endpoint.clone(),
        client_tls.certificate(),
        remote_signing_key,
        98,
        remote_lease.clone(),
    )
    .await;
    let remote_provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            remote.runtime(),
            Arc::clone(&application),
            layout.clone(),
            remote_session,
            remote_endpoint,
            files.path().join("remote-data"),
        )
        .unwrap(),
    );
    let remote_listener_tls =
        LoadedPeerTls::load(&remote_certificate, &remote_key, &ca, "localhost").unwrap();
    let (remote_shutdown, remote_cancel) = tokio::sync::oneshot::channel();
    let remote_router = peer_router(&remote, layout.clone(), peer_directory.clone());
    let remote_server = tokio::spawn(async move {
        axum::serve(
            remote_listener_tls.listener(remote_listener),
            remote_router.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = remote_cancel.await;
        })
        .await
    });
    let client = build_peer_client(
        &remote,
        layout.clone(),
        peer_directory.clone(),
        remote_session,
        &client_tls,
    )
    .unwrap();
    let remote_account = remote
        .application_handle::<Beyonddb>(client.clone(), account.tenant(), account.application())
        .unwrap();
    let issued_at_ms = now_ms();
    let created = remote_account
        .command::<CreateTable>(
            &account,
            crab_cell_runtime::MutationIdentity {
                request_id: crab_cell_runtime::identity::RequestId::from_bytes([99; 16]),
                issued_at_ms,
                expires_at_ms: issued_at_ms + 60_000,
            },
            Json(TableSpec {
                table_name: "RemoteTable".into(),
                key_schema: vec![KeySchemaElement {
                    attribute_name: "id".into(),
                    key_type: KeyType::Hash,
                }],
                attribute_definitions: vec![AttributeDefinition {
                    attribute_name: "id".into(),
                    attribute_type: ScalarAttributeType::S,
                }],
                billing_mode: BillingMode::PayPerRequest,
                provisioned_throughput: None,
                deletion_protection_enabled: false,
                initial_tags: Vec::new(),
                resource_arn: None,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert!(matches!(created, CreateTableOutcome::Created(_)));
    let read = remote_account
        .query::<DescribeTable>(&account, None, Json("RemoteTable".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(read.table_name, "RemoteTable");
    let wrong_principal = CellClient::runtime_with_peer(
        application.registry(),
        remote.runtime(),
        layout.clone(),
        Arc::new(PeerSigner::new(
            remote_session,
            application.registry().release_digest(),
            client_tls.signing_key().clone(),
        )),
        PeerPrincipal {
            issuer: "another fleet".into(),
            subject: "remote".into(),
            actions: vec!["beyonddb.cell.invoke".into()],
        },
        Arc::new(PeerHttpRoundTrip::new(
            Arc::new(BeyonddbPeerScope),
            CellAuthority::new(layout.clone()),
            peer_directory.clone(),
            Arc::new(client_tls.client_identity()),
            remote_session,
        )),
    );
    let wrong_account = remote
        .application_handle::<Beyonddb>(wrong_principal, account.tenant(), account.application())
        .unwrap();
    assert!(
        wrong_account
            .query::<DescribeTable>(&account, None, Json("RemoteTable".into()))
            .await
            .is_err()
    );
    const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const ENCRYPTION_KEY: [u8; 32] = [38; 32];
    let credential = provisioner.admit_credential(ACCESS_KEY).await.unwrap();
    CellCredentialStore::new(
        CellClient::local(application.registry(), credential),
        layout.clone(),
        ENCRYPTION_KEY,
    )
    .put_credential(
        ACCESS_KEY,
        StoredCredential {
            secret_key: SECRET_KEY.into(),
            account_id: "123456789012".into(),
            principal_name: "network-user".into(),
            session_name: None,
            is_session: false,
            session_token: None,
            is_active: true,
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let authorization = CellAuthorizationStore::new(CellClient::local_runtime(
        application.registry(),
        owner.runtime(),
        layout.clone(),
    ));
    authorization
        .put_user_policy(
            "123456789012",
            "network-user",
            "tables",
            &serde_json::json!({
                "Version": "2012-10-17",
                "Statement": [{
                    "Effect": "Allow",
                    "Action": "dynamodb:*",
                    "Resource": "arn:aws:dynamodb:us-east-1:123456789012:table/NetworkData"
                }]
            })
            .to_string(),
        )
        .await
        .unwrap();
    let public_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public_endpoint = format!("http://{}", public_listener.local_addr().unwrap());
    let state = build_http_state(
        &remote,
        client,
        layout.clone(),
        Arc::clone(&remote_provisioner),
        ENCRYPTION_KEY,
        "us-east-1",
        public_endpoint.clone(),
    )
    .unwrap();
    let public_server = tokio::spawn(extenddb_server::start_server(
        public_listener,
        state,
        None,
        None,
    ));
    let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            ACCESS_KEY,
            SECRET_KEY,
            None,
            None,
            "beyonddb-network-test",
        ))
        .endpoint_url(public_endpoint)
        .load()
        .await;
    let sdk = aws_sdk_dynamodb::Client::new(&sdk_config);
    sdk.create_table()
        .table_name("NetworkData")
        .key_schema(
            aws_sdk_dynamodb::types::KeySchemaElement::builder()
                .attribute_name("id")
                .key_type(aws_sdk_dynamodb::types::KeyType::Hash)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            aws_sdk_dynamodb::types::AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(aws_sdk_dynamodb::types::ScalarAttributeType::S)
                .build()
                .unwrap(),
        )
        .billing_mode(aws_sdk_dynamodb::types::BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap();
    let item = HashMap::from([
        ("id".into(), AwsAttributeValue::S("remote".into())),
        ("value".into(), AwsAttributeValue::S("committed".into())),
    ]);
    sdk.put_item()
        .table_name("NetworkData")
        .set_item(Some(item.clone()))
        .send()
        .await
        .unwrap();
    let read = sdk
        .get_item()
        .table_name("NetworkData")
        .key("id", AwsAttributeValue::S("remote".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(read.item(), Some(&item));
    shutdown_tx.send(()).unwrap();
    server.await.unwrap().unwrap();
    owner_lease.cancel();
    tokio::time::sleep(std::time::Duration::from_secs(11)).await;

    let replacement_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let replacement_endpoint = format!("https://{}", replacement_listener.local_addr().unwrap());
    let replacement_tls =
        LoadedPeerTls::load(&owner_certificate, &owner_key, &ca, "localhost").unwrap();
    let replacement_session = SessionId::from_bytes([100; 16]);
    let replacement = start_node(
        Arc::clone(&application),
        peer_directory.clone(),
        replacement_session,
        replacement_endpoint.clone(),
        replacement_tls.certificate(),
        replacement_tls.signing_key().clone(),
        101,
        CancellationToken::new(),
    )
    .await;
    let replacement_provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            replacement.runtime(),
            Arc::clone(&application),
            layout.clone(),
            replacement_session,
            replacement_endpoint,
            files.path().join("restored"),
        )
        .unwrap(),
    );
    let replacement_account = replacement_provisioner
        .takeover_expired_account("123456789012", &peer_directory)
        .await
        .unwrap();
    let local_account = replacement
        .application_handle::<Beyonddb>(
            CellClient::local(application.registry(), replacement_account),
            account.tenant(),
            account.application(),
        )
        .unwrap();
    let table = local_account
        .query::<DescribeTable>(&account, None, Json("NetworkData".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let route = local_account
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    for partition in &route.partitions {
        let target =
            beyonddb::data_target("123456789012", &table.id, &partition.partition_id).unwrap();
        let authority = CellAuthority::new(layout.clone())
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            authority.value().owner.as_ref().unwrap().session,
            remote_session
        );
    }
    replacement_provisioner
        .takeover_expired_credential(ACCESS_KEY, &peer_directory)
        .await
        .unwrap();
    let (replacement_shutdown, replacement_cancel) = tokio::sync::oneshot::channel();
    let replacement_client = build_peer_client(
        &replacement,
        layout.clone(),
        peer_directory.clone(),
        replacement_session,
        &replacement_tls,
    )
    .unwrap();
    let replacement_router = peer_router(&replacement, layout.clone(), peer_directory.clone());
    let replacement_server = tokio::spawn(async move {
        axum::serve(
            replacement_tls.listener(replacement_listener),
            replacement_router.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = replacement_cancel.await;
        })
        .await
    });
    let recovered = sdk
        .get_item()
        .table_name("NetworkData")
        .key("id", AwsAttributeValue::S("remote".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(recovered.item(), Some(&item));
    let replacement_public_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let replacement_public_endpoint = format!(
        "http://{}",
        replacement_public_listener.local_addr().unwrap()
    );
    let replacement_state = build_http_state(
        &replacement,
        replacement_client,
        layout,
        Arc::clone(&replacement_provisioner),
        ENCRYPTION_KEY,
        "us-east-1",
        replacement_public_endpoint.clone(),
    )
    .unwrap();
    let replacement_public_server = tokio::spawn(extenddb_server::start_server(
        replacement_public_listener,
        replacement_state,
        None,
        None,
    ));
    let replacement_sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            ACCESS_KEY,
            SECRET_KEY,
            None,
            None,
            "beyonddb-replacement-test",
        ))
        .endpoint_url(replacement_public_endpoint)
        .load()
        .await;
    let replacement_sdk = aws_sdk_dynamodb::Client::new(&replacement_sdk_config);
    let cross_owner_read = replacement_sdk
        .get_item()
        .table_name("NetworkData")
        .key("id", AwsAttributeValue::S("remote".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_owner_read.item(), Some(&item));
    assert!(
        replacement_provisioner
            .takeover_expired_partition(
                "123456789012",
                &table.id,
                &route.partitions[0].partition_id,
                &peer_directory,
            )
            .await
            .is_err()
    );
    public_server.abort();
    remote_shutdown.send(()).unwrap();
    remote_server.await.unwrap().unwrap();
    remote_lease.cancel();
    tokio::time::sleep(std::time::Duration::from_secs(11)).await;
    for partition in route.partitions {
        replacement_provisioner
            .takeover_expired_partition(
                "123456789012",
                &table.id,
                &partition.partition_id,
                &peer_directory,
            )
            .await
            .unwrap();
    }
    let moved_owner_read = replacement_sdk
        .get_item()
        .table_name("NetworkData")
        .key("id", AwsAttributeValue::S("remote".into()))
        .send()
        .await
        .unwrap();
    assert_eq!(moved_owner_read.item(), Some(&item));
    replacement_shutdown.send(()).unwrap();
    replacement_server.await.unwrap().unwrap();
    replacement_public_server.abort();
    owner.shutdown().await.unwrap();
    remote.shutdown().await.unwrap();
    replacement.shutdown().await.unwrap();
}
