mod codec;
mod creation;
mod deletion;
mod directories;
mod discovery;
mod index_splits;
mod ownership_race;
mod placement;
mod provisioning;
mod rebalance;
mod reclamation;
mod recovery;
mod splits;
mod statistics;
mod usage;

use crate::*;
use crab_cell_runtime::cell::actor::CellHandle;
use extenddb_core::types::{AttributeValue, Item};
use tracing_subscriber::prelude::*;

type SdkItem = HashMap<String, AwsAttributeValue>;

struct Fixture {
    _files: tempfile::TempDir,
    node: CellNode,
    tasks: Arc<CellNodeTaskGroup>,
    application: Arc<crab_cell_app::CompiledApplication>,
    provisioner: Arc<CellInitialPartitionProvisioner>,
    layout: CellStorageLayout,
    session: SessionId,
    directory: NodeDirectory,
    remote_tls: LoadedPeerTls,
    endpoint: String,
    sdk: aws_sdk_dynamodb::Client,
    client: CellClient,
    data: Vec<(CellHandle, SdkItem)>,
    public_server: tokio::task::JoinHandle<()>,
    peer_server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_partition_count(2).await
    }

    async fn with_partition_count(partitions: u16) -> Self {
        Self::with_store(partitions, Arc::new(InMemory::new())).await
    }

    async fn with_capacity(partitions: u16, cell_capacity: usize) -> Self {
        Self::with_store_capacity(partitions, Arc::new(InMemory::new()), cell_capacity).await
    }

    async fn with_store(partitions: u16, store: Arc<dyn object_store::ObjectStore>) -> Self {
        Self::with_store_capacity(partitions, store, 8).await
    }

    async fn with_store_capacity(
        partitions: u16,
        store: Arc<dyn object_store::ObjectStore>,
        cell_capacity: usize,
    ) -> Self {
        // SDK errors deliberately hide storage details. Retain server warnings
        // in the test output so CI failures identify the underlying boundary.
        let diagnostics = tracing_subscriber::filter::Targets::new()
            .with_default(tracing::Level::WARN)
            .with_target("beyonddb::server::peer_receiver", tracing::Level::DEBUG);
        let _ = tracing_subscriber::registry()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_test_writer()
                    .with_filter(diagnostics),
            )
            .try_init();
        let files = tempfile::tempdir().unwrap();
        let (certificate, key, remote_certificate, remote_key, ca) = tls_files(files.path());
        let remote_tls =
            LoadedPeerTls::load(&remote_certificate, &remote_key, &ca, "localhost").unwrap();
        let tls = LoadedPeerTls::load(&certificate, &key, &ca, "localhost").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let application = Arc::new(
            Beyonddb::compile(BuildDescriptor {
                source_revision: "residency-test".into(),
                cargo_lock_digest: Digest::from_bytes([92; 32]),
            })
            .unwrap(),
        );
        let account = account_target("123456789012").unwrap();
        let layout = CellStorageLayout::new(
            Store::new(store),
            object_store::path::Path::from("beyonddb-residency"),
            *account.application().as_bytes(),
        );
        let directory = NodeDirectory::new(
            layout.clone(),
            tls.fleet(),
            Digest::from_bytes([90; 32]),
            application.registry().release_digest(),
        );
        let session = SessionId::from_bytes([93; 16]);
        let lease = CancellationToken::new();
        let (node, tasks) = start_node(
            Arc::clone(&application),
            cell_capacity,
            directory.clone(),
            session,
            endpoint.clone(),
            &tls,
            95,
            lease.clone(),
        )
        .await;
        let peers = Arc::new(
            BeyonddbPeers::new(&node, layout.clone(), directory.clone(), session, &tls).unwrap(),
        );
        let provisioner = Arc::new(
            CellInitialPartitionProvisioner::new(
                node.runtime(),
                Arc::clone(&application),
                layout.clone(),
                session,
                endpoint.clone(),
                files.path().join("data"),
            )
            .unwrap()
            .with_initial_partition_count(partitions)
            .unwrap()
            .with_peers(peers.clone()),
        );
        provisioner.admit_account("123456789012").await.unwrap();
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
        let client = peers.client(provisioner.clone());
        CellAuthorizationStore::new(client.clone())
            .put_user_policy(
                "123456789012",
                "network-user",
                "tables",
                &serde_json::json!({
                    "Version": "2012-10-17",
                    "Statement": [{
                        "Effect": "Allow",
                        "Action": "dynamodb:*",
                        "Resource": "arn:aws:dynamodb:us-east-1:123456789012:table/Residency*"
                    }]
                })
                .to_string(),
            )
            .await
            .unwrap();
        let router = peers.router(provisioner.clone());
        let peer_server = tokio::spawn(async move {
            axum::serve(
                tls.listener(listener),
                router.into_make_service_with_connect_info::<PeerTlsIdentity>(),
            )
            .await
            .unwrap();
        });
        let public_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public_endpoint = format!("http://{}", public_listener.local_addr().unwrap());
        let state = build_http_state(
            &node,
            client.clone(),
            layout.clone(),
            provisioner.clone(),
            ENCRYPTION_KEY,
            "us-east-1",
            public_endpoint.clone(),
        )
        .unwrap();
        let public_server = tokio::spawn(async move {
            extenddb_server::start_server(public_listener, state, None, None)
                .await
                .unwrap();
        });
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .credentials_provider(Credentials::new(
                ACCESS_KEY,
                SECRET_KEY,
                None,
                None,
                "beyonddb-residency-test",
            ))
            .endpoint_url(public_endpoint)
            .load()
            .await;
        let sdk = aws_sdk_dynamodb::Client::new(&config);
        sdk.create_table()
            .table_name("Residency")
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
        let app = node
            .application_handle::<Beyonddb>(client.clone(), account.tenant(), account.application())
            .unwrap();
        let table = app
            .query::<DescribeTable>(&account, None, Json("Residency".into()))
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        let route = crate::single_leaf_route(&client, &account, &table.id.clone())
            .await
            .unwrap();

        assert_eq!(route.partitions.len(), usize::from(partitions));
        let mut data = Vec::new();
        for partition in route.partitions {
            let id = (0..1_000)
                .map(|index| format!("persisted-{index}"))
                .find(|id| {
                    let key = Item::from([("id".into(), AttributeValue::S(id.clone()))]);
                    let hash = beyonddb::data_key_hash(&table.id, &key, &table.key_schema).unwrap();
                    partition.lower.is_none_or(|lower| hash >= lower)
                        && partition.upper.is_none_or(|upper| hash < upper)
                })
                .unwrap();
            let item = HashMap::from([
                ("id".into(), AwsAttributeValue::S(id)),
                ("value".into(), AwsAttributeValue::S("committed".into())),
            ]);
            sdk.put_item()
                .table_name("Residency")
                .set_item(Some(item.clone()))
                .send()
                .await
                .unwrap();
            let handle = provisioner
                .admit_existing_partition("123456789012", &table.id, &partition.partition_id)
                .await
                .unwrap();
            data.push((handle, item));
        }
        Self {
            _files: files,
            node,
            tasks,
            application,
            provisioner,
            layout,
            session,
            directory,
            remote_tls,
            endpoint,
            sdk,
            client,
            data,
            public_server,
            peer_server,
        }
    }

    async fn shutdown(self) {
        self.public_server.abort();
        self.peer_server.abort();
        let _ = self.public_server.await;
        let _ = self.peer_server.await;
        self.node.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_read_reacquires_released_cells() {
    read_after_release(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_read_resumes_interrupted_cell_acquisition() {
    read_after_release(true).await;
}

async fn read_after_release(interrupted_acquisition: bool) {
    let fixture = Fixture::new().await;
    let mut handles = fixture
        .data
        .iter()
        .cloned()
        .map(|(handle, item)| ("data", handle, item))
        .collect::<Vec<_>>();
    let expected = fixture.data[0].1.clone();
    handles.push((
        "account",
        fixture
            .provisioner
            .admit_account("123456789012")
            .await
            .unwrap(),
        expected.clone(),
    ));
    handles.push((
        "credentials",
        fixture
            .provisioner
            .admit_credential("AKIAIOSFODNN7EXAMPLE")
            .await
            .unwrap(),
        expected,
    ));
    let mut reads = Vec::new();
    for (role, handle, item) in handles {
        let cell = handle.cell_id();
        handle.drain().await.unwrap();
        if interrupted_acquisition {
            // Leave the durable boundary a canceled acquisition can expose:
            // ownership claimed, published root unchanged, actor not admitted.
            let authority = CellAuthority::new(fixture.layout.clone());
            let observed = authority.load(cell).await.unwrap().unwrap();
            let claimed = observed
                .value()
                .takeover(crab_cell_runtime::control::Owner {
                    session: fixture.session,
                    endpoint: fixture.endpoint.clone(),
                })
                .unwrap();
            authority
                .transition(
                    &observed,
                    claimed,
                    crab_cell_runtime::control::Transition::Takeover,
                )
                .await
                .unwrap();
        }
        let read = fixture
            .sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await;
        let owner = CellAuthority::new(fixture.layout.clone())
            .load(cell)
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .as_ref()
            .map(|owner| owner.session);
        let failed = read.is_err();
        reads.push((role, read, item, owner));
        if failed {
            break;
        }
    }
    let session = fixture.session;
    fixture.shutdown().await;
    for (role, read, item, owner) in reads {
        assert_eq!(
            read.unwrap_or_else(|error| panic!("{role}: {error:?}"))
                .item,
            Some(item)
        );
        assert_eq!(owner, Some(session), "{role} must regain ownership");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_transaction_reacquires_both_released_participants() {
    use aws_sdk_dynamodb::types::{Get, TransactGetItem, TransactWriteItem, Update};

    let fixture = Fixture::new().await;
    assert_ne!(fixture.data[0].0.cell_id(), fixture.data[1].0.cell_id());
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    for (handle, item) in &fixture.data {
        handle.drain().await.unwrap();
        writes.push(
            TransactWriteItem::builder()
                .update(
                    Update::builder()
                        .table_name("Residency")
                        .key("id", item["id"].clone())
                        .update_expression("SET #v = :after")
                        .condition_expression("#v = :before")
                        .expression_attribute_names("#v", "value")
                        .expression_attribute_values(
                            ":before",
                            AwsAttributeValue::S("committed".into()),
                        )
                        .expression_attribute_values(
                            ":after",
                            AwsAttributeValue::S("transaction".into()),
                        )
                        .build()
                        .unwrap(),
                )
                .build(),
        );
        reads.push(
            TransactGetItem::builder()
                .get(
                    Get::builder()
                        .table_name("Residency")
                        .key("id", item["id"].clone())
                        .build()
                        .unwrap(),
                )
                .build(),
        );
    }
    // Both original participants are idle when BEGIN starts. Prepare and
    // resolution must restore them through the same request routing path.
    let write = fixture
        .sdk
        .transact_write_items()
        .client_request_token("released-participants")
        .set_transact_items(Some(writes))
        .send()
        .await;
    let read = fixture
        .sdk
        .transact_get_items()
        .set_transact_items(Some(reads))
        .send()
        .await;
    let expected = fixture
        .data
        .iter()
        .map(|(_, item)| {
            let mut item = item.clone();
            item.insert("value".into(), AwsAttributeValue::S("transaction".into()));
            item
        })
        .collect::<Vec<_>>();
    fixture.shutdown().await;
    write.unwrap();
    let actual = read
        .unwrap()
        .responses
        .unwrap()
        .into_iter()
        .map(|response| response.item.unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}
