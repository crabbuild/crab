use super::*;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, GlobalSecondaryIndex, KeySchemaElement, KeyType, Projection,
    ProjectionType, ScalarAttributeType,
};
use crab_cell_runtime::{
    cell::catalog::{CatalogEntry, CatalogRole, CellCatalog},
    control::Owner,
    identity::{CellTarget, IncarnationId},
};

pub(super) struct Remote {
    pub(super) node: CellNode,
    _tasks: Arc<CellNodeTaskGroup>,
    pub(super) session: SessionId,
    endpoint: String,
    lease: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl Remote {
    pub(super) async fn new(fixture: &Fixture) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("https://{}", listener.local_addr().unwrap());
        let session = SessionId::from_bytes([96; 16]);
        let lease = CancellationToken::new();
        let (remote, _tasks) = start_node(
            fixture.application.clone(),
            fixture.directory.clone(),
            session,
            endpoint.clone(),
            fixture.remote_tls.certificate(),
            fixture.remote_tls.signing_key().clone(),
            98,
            lease.clone(),
        )
        .await;
        let peers = Arc::new(
            BeyonddbPeers::new(
                &remote,
                fixture.layout.clone(),
                fixture.directory.clone(),
                session,
                &fixture.remote_tls,
            )
            .unwrap(),
        );
        let provisioner = Arc::new(
            CellInitialPartitionProvisioner::new(
                remote.runtime(),
                fixture.application.clone(),
                fixture.layout.clone(),
                session,
                endpoint.clone(),
                fixture._files.path().join("remote-data"),
            )
            .unwrap()
            .with_initial_partition_count(2)
            .unwrap()
            .with_peers(peers.clone()),
        );
        let router = peers.router(provisioner.clone());
        let tls = LoadedPeerTls::load(
            &fixture._files.path().join("remote.crt"),
            &fixture._files.path().join("remote.key"),
            &fixture._files.path().join("ca.crt"),
            "localhost",
        )
        .unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                tls.listener(listener),
                router.into_make_service_with_connect_info::<PeerTlsIdentity>(),
            )
            .await
            .unwrap();
        });
        Self {
            node: remote,
            _tasks,
            session,
            endpoint,
            lease,
            server,
        }
    }
    pub(super) async fn shutdown(self) {
        self.node.shutdown().await.unwrap();
        self.server.abort();
        let _ = self.server.await;
    }
}

pub(super) fn sdk_without_retries(fixture: &Fixture) -> aws_sdk_dynamodb::Client {
    aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_initial_base_and_index_ranges_use_remote_owners() {
    let fixture = Fixture::new().await;
    let sdk = sdk_without_retries(&fixture);
    let remote = Remote::new(&fixture).await;
    beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1")
        .install_global_index_loop(
            &fixture.tasks,
            vec!["123456789012".into()],
            fixture.provisioner.clone(),
            fixture.directory.clone(),
        )
        .unwrap();
    // First wait for the local owner's existing Cells to appear in its signed
    // capacity. New range placement must come from those real measurements.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let node = fixture
                .directory
                .load(fixture.session, now_ms())
                .await
                .unwrap()
                .unwrap();
            if node
                .advertisement()
                .placement_capacity()
                .unwrap()
                .active_cells
                >= 4
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    create(&sdk, "ResidencyIndex", true).send().await.unwrap();
    let account = account_target("123456789012").unwrap();
    let record = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("ResidencyIndex".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let route = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(record.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let mut targets = route
        .partitions
        .iter()
        .map(|range| {
            beyonddb::data_target("123456789012", &record.id, &range.partition_id).unwrap()
        })
        .collect::<Vec<_>>();
    let index = &record.global_secondary_indexes[0];
    let page = fixture
        .client
        .query::<beyonddb::ReadGlobalIndexRoutePage>(
            &account,
            None,
            Json(beyonddb::RoutePageInput {
                table_id: index.id.clone(),
                start_hash: None,
                after_lower: None,
                expected_epoch: None,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    let beyonddb::RoutePageOutcome::Page { partitions, .. } = page else {
        panic!("missing GSI route")
    };
    targets.extend(partitions.iter().map(|range| {
        beyonddb::global_index_target("123456789012", &index.id, &range.partition_id).unwrap()
    }));
    let authority = CellAuthority::new(fixture.layout.clone());
    let mut remote_namespaces = std::collections::HashSet::new();
    for target in &targets {
        let control = authority.load(target.cell_id()).await.unwrap().unwrap();
        if control.value().owner.as_ref().unwrap().session == remote.session {
            remote_namespaces.insert(target.namespace());
        }
    }
    assert_eq!(
        remote_namespaces.len(),
        2,
        "both base and GSI ranges must bootstrap remotely"
    );
    sdk.put_item()
        .table_name("ResidencyIndex")
        .item("id", AwsAttributeValue::S("row".into()))
        .item("bucket", AwsAttributeValue::S("same".into()))
        .send()
        .await
        .unwrap();
    assert_index(&sdk).await;

    remote.node.shutdown().await.unwrap();
    remote.server.abort();
    let _ = remote.server.await;
    wait_for_expiry(&fixture, remote.session).await;
    let account_handle = fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap();
    fixture
        .provisioner
        .recover_registered_partitions("123456789012", account_handle, &fixture.directory)
        .await
        .unwrap();
    assert_index(&sdk).await;
    let item = sdk
        .get_item()
        .table_name("ResidencyIndex")
        .key("id", AwsAttributeValue::S("row".into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(
        item.item.unwrap()["bucket"],
        AwsAttributeValue::S("same".into())
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_initial_claim_recovery_preserves_unrouted_roots() {
    let fixture = Fixture::new().await;
    let sdk = sdk_without_retries(&fixture);
    let remote = Remote::new(&fixture).await;
    let account = account_target("123456789012").unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    // Root publication and range installation can precede the account route.
    // Preserve data across this boundary, even though route recovery cannot
    // discover the range yet.
    let published = unpublished(
        &fixture,
        "ResidencyPublishedClaim",
        remote.session,
        &remote.endpoint,
    )
    .await;
    let record = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("ResidencyPublishedClaim".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    fixture
        .provisioner
        .provision(&fixture.client, "123456789012", &record)
        .await
        .unwrap();
    let existing_key = (0..1_000)
        .map(|n| format!("existing-{n}"))
        .find(|id| {
            let item = Item::from([("id".into(), AttributeValue::S(id.clone()))]);
            beyonddb::data_key_hash(&record.id, &item, &record.key_schema).unwrap()[0] < 128
        })
        .unwrap();
    fixture
        .client
        .command::<beyonddb::PartitionPut>(
            &published,
            mutation(),
            Json(beyonddb::PartitionPutInput {
                table_id: record.id.clone(),
                epoch: 1,
                item: Item::from([("id".into(), AttributeValue::S(existing_key.clone()))]),
                condition: None,
            }),
        )
        .await
        .unwrap();
    assert!(
        fixture
            .client
            .query::<ReadTableRoute>(&account, None, Json(record.id.clone()))
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    let published_before = authority.load(published.cell_id()).await.unwrap().unwrap();
    assert!(published_before.value().root.is_some());
    assert_eq!(
        published_before.value().owner.as_ref().unwrap().session,
        remote.session
    );

    let dead = unpublished(
        &fixture,
        "ResidencyDeadClaim",
        remote.session,
        &remote.endpoint,
    )
    .await;
    let before = authority.load(dead.cell_id()).await.unwrap().unwrap();
    // Stop its listener while its lease is live: neither placement nor a
    // retried CreateTable may transfer this unpublished Cell to another node.
    remote.server.abort();
    let _ = remote.server.await;
    assert!(
        create(&sdk, "ResidencyDeadClaim", false)
            .send()
            .await
            .is_err()
    );
    assert_eq!(
        authority
            .load(dead.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value(),
        before.value()
    );
    remote.lease.cancel();
    wait_for_expiry(&fixture, remote.session).await;
    create(&sdk, "ResidencyDeadClaim", false)
        .send()
        .await
        .unwrap();
    let recovered = authority.load(dead.cell_id()).await.unwrap().unwrap();
    assert_eq!(recovered.value().incarnation, before.value().incarnation);
    assert_eq!(
        recovered.value().owner.as_ref().unwrap().session,
        fixture.session
    );
    assert!(recovered.value().root.is_some());
    assert!(
        fixture
            .directory
            .takeover_proof(remote.session, fixture.session, now_ms())
            .await
            .unwrap()
            .is_some()
    );
    let record = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("ResidencyDeadClaim".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let key = (0..1_000)
        .map(|n| format!("durable-{n}"))
        .find(|id| {
            let item = Item::from([("id".into(), AttributeValue::S(id.clone()))]);
            beyonddb::data_key_hash(&record.id, &item, &record.key_schema).unwrap()[0] < 128
        })
        .unwrap();
    sdk.put_item()
        .table_name("ResidencyDeadClaim")
        .item("id", AwsAttributeValue::S(key.clone()))
        .send()
        .await
        .unwrap();
    fixture
        .provisioner
        .admit_existing_partition("123456789012", &record.id, &[0; 16])
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    let item = sdk
        .get_item()
        .table_name("ResidencyDeadClaim")
        .key("id", AwsAttributeValue::S(key.clone()))
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(item.item.unwrap()["id"], AwsAttributeValue::S(key));
    create(&sdk, "ResidencyPublishedClaim", false)
        .send()
        .await
        .unwrap();
    let restored = authority.load(published.cell_id()).await.unwrap().unwrap();
    assert_eq!(
        restored.value().incarnation,
        published_before.value().incarnation
    );
    assert_eq!(
        restored.value().owner.as_ref().unwrap().session,
        fixture.session
    );
    let item = sdk
        .get_item()
        .table_name("ResidencyPublishedClaim")
        .key("id", AwsAttributeValue::S(existing_key.clone()))
        .consistent_read(true)
        .send()
        .await
        .unwrap();
    assert_eq!(item.item.unwrap()["id"], AwsAttributeValue::S(existing_key));
    assert!(matches!(
        remote.node.shutdown().await,
        Ok(()) | Err(crab_cell_runtime::Error::Fenced)
    ));
    fixture.shutdown().await;
}

async fn wait_for_expiry(fixture: &Fixture, session: SessionId) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while fixture.directory.is_live(session, now_ms()).await.unwrap() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
}

pub(super) fn create(
    sdk: &aws_sdk_dynamodb::Client,
    name: &str,
    index: bool,
) -> aws_sdk_dynamodb::operation::create_table::builders::CreateTableFluentBuilder {
    let mut request = sdk
        .create_table()
        .table_name(name)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(
            KeySchemaElement::builder()
                .attribute_name("id")
                .key_type(KeyType::Hash)
                .build()
                .unwrap(),
        )
        .attribute_definitions(
            AttributeDefinition::builder()
                .attribute_name("id")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .unwrap(),
        );
    if index {
        request = request
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name("bucket")
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .unwrap(),
            )
            .global_secondary_indexes(
                GlobalSecondaryIndex::builder()
                    .index_name("ByBucket")
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name("bucket")
                            .key_type(KeyType::Hash)
                            .build()
                            .unwrap(),
                    )
                    .projection(
                        Projection::builder()
                            .projection_type(ProjectionType::All)
                            .build(),
                    )
                    .build()
                    .unwrap(),
            );
    }
    request
}

pub(super) async fn table_id(fixture: &Fixture, name: &str) -> String {
    fixture
        .client
        .query::<DescribeTable>(
            &account_target("123456789012").unwrap(),
            None,
            Json(name.into()),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap()
        .id
}

async fn unpublished(
    fixture: &Fixture,
    name: &str,
    session: SessionId,
    endpoint: &str,
) -> CellTarget {
    use extenddb_core::types::{
        AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
    };
    fixture
        .client
        .command::<CreateTable>(
            &account_target("123456789012").unwrap(),
            mutation(),
            Json(TableSpec {
                placement: beyonddb::TablePlacement::Routed {
                    initial_partitions: 2,
                },
                table_name: name.into(),
                key_schema: vec![KeySchemaElement {
                    attribute_name: "id".into(),
                    key_type: KeyType::Hash,
                }],
                attribute_definitions: vec![AttributeDefinition {
                    attribute_name: "id".into(),
                    attribute_type: ScalarAttributeType::S,
                }],
                local_secondary_indexes: Vec::new(),
                global_secondary_indexes: Vec::new(),
                billing_mode: BillingMode::PayPerRequest,
                provisioned_throughput: None,
                deletion_protection_enabled: false,
                initial_tags: Vec::new(),
                resource_arn: Some(format!(
                    "arn:aws:dynamodb:us-east-1:123456789012:table/{name}"
                )),
            }),
        )
        .await
        .unwrap();
    let target =
        beyonddb::data_target("123456789012", &table_id(fixture, name).await, &[0; 16]).unwrap();
    let proof = CellCatalog::new(fixture.layout.clone(), target.tenant())
        .provision(
            CatalogEntry::new(
                &target,
                CatalogRole::Sql,
                fixture
                    .application
                    .registry()
                    .module_code("beyonddb-data")
                    .unwrap(),
                1,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    CellAuthority::new(fixture.layout.clone())
        .create_initial(
            &proof,
            IncarnationId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
            Owner {
                session,
                endpoint: endpoint.into(),
            },
        )
        .await
        .unwrap();
    target
}

async fn assert_index(sdk: &aws_sdk_dynamodb::Client) {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let result = sdk
                .query()
                .table_name("ResidencyIndex")
                .index_name("ByBucket")
                .key_condition_expression("#bucket = :b")
                .expression_attribute_names("#bucket", "bucket")
                .expression_attribute_values(":b", AwsAttributeValue::S("same".into()))
                .send()
                .await
                .unwrap();
            if result.items().len() == 1 {
                assert_eq!(result.items()[0]["id"], AwsAttributeValue::S("row".into()));
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
}

fn mutation() -> crab_cell_runtime::MutationIdentity {
    let now = now_ms();
    crab_cell_runtime::MutationIdentity {
        request_id: crab_cell_runtime::identity::RequestId::from_bytes(
            *uuid::Uuid::now_v7().as_bytes(),
        ),
        issued_at_ms: now,
        expires_at_ms: now + 60_000,
    }
}
