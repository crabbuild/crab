mod elastic_cells {
    mod account_participant;
    mod coordinator_residency;
    mod coordinator_tokens;
    mod public_transactions;
    mod read_resolution;
    mod transaction_driver;
    mod transaction_reads;
    mod transaction_recovery;
    pub(crate) mod transaction_visibility;
}

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

use aws_credential_types::Credentials;
use aws_sdk_dynamodb::types::AttributeValue as AwsAttributeValue;
use beyonddb::{
    ActivateImportedPartition, ActivateImportedPartitionInput, ActivateImportedPartitionOutcome,
    ActivateTableRoute, ActivateTableRouteOutcome, BeginCrossCellTransaction,
    BeginCrossCellTransactionInput, BeginCrossCellTransactionOutcome, BeginSplit,
    BeginSplitOutcome, Beyonddb, BeyonddbPeerScope, CellAuthorizationStore, CellCredentialStore,
    CellInitialPartitionProvisioner, CellSplitController, CellStorage, CommitSplit,
    CommitSplitOutcome, CoordinatorDecision, CoordinatorParticipant, CoordinatorParticipantTarget,
    CoordinatorPhaseInput, CoordinatorPhaseOutcome, CreateTable, CreateTableOutcome,
    DecideCrossCellTransaction, DecideCrossCellTransactionInput, DecideCrossCellTransactionOutcome,
    DeleteItem, DeleteItemInput, DeleteTable, DeleteTableOutcome, DescribeTable, GetItem,
    GetItemInput, GetItemOutcome, ImportPartitionItem, ImportSummary, IndexedTransactionOperation,
    InitialPartitionProvisioner, InstallPartition, InstallPartitionOutcome, ItemMutationOutcome,
    Json, ListCoordinatorShards, ListCoordinatorShardsInput, NodeLeasePublisher,
    ParticipantTransactionState, PartitionDelete, PartitionDeleteInput, PartitionDeleteOutcome,
    PartitionExport, PartitionGet, PartitionGetInput, PartitionGetOutcome, PartitionImportInput,
    PartitionImportOutcome, PartitionInstall, PartitionLookupInput, PartitionLookupOutcome,
    PartitionPut, PartitionPutInput, PartitionPutOutcome, PartitionScan, PartitionScanInput,
    PartitionScanOutcome, PartitionSeal, PartitionSpec, PartitionState, PartitionTransactWrite,
    PartitionTransactWriteInput, PartitionTransactWriteOutcome, PartitionUpdate,
    PartitionUpdateInput, PartitionUpdateOutcome, PartitionUsage, PendingTransactionState,
    PreparePartitionTransaction, PreparePartitionTransactionInput, PrepareTransactionOutcome,
    PublishedNodeLease, PutItem, PutItemInput, ReadCoordinatorParticipant,
    ReadCoordinatorParticipantInput, ReadCrossCellTransaction, ReadCrossCellTransactionInput,
    ReadPartitionRoute, ReadPartitionState, ReadPartitionTransaction,
    ReadPendingCrossCellTransactions, ReadPendingCrossCellTransactionsInput, ReadRoutePage,
    ReadSplitPlan, ReadSplitRoute, ReadTableRoute, ReadTransactionInput, ReadTtlSchedule,
    ReadTtlSweep, ReadUnresolvedCoordinatorParticipants, RecordParticipantPrepare,
    RecordParticipantResolution, ResolvePartitionTransaction, ResolveTransactionInput,
    ResolveTransactionOutcome, RoutePageInput, RoutePageOutcome, SealPartition,
    SealPartitionOutcome, SplitPlan, SplitRouteState, TableRoute, TableSpec, TransactionOperation,
    TransactionToken, UpdateTtl, UpdateTtlInput, account_target, build_http_state,
    coordinator_target, credential_target, data_key_hash, data_target, initialize_account,
    initialize_coordinator, initialize_partition,
};
use crab_cell_app::CellApplication;
use crab_cell_host::CellNodeBuilder;
use crab_cell_peer_http::PeerTargetScope;
use crab_cell_runtime::cell::actor::{CellHandle, CellRuntime};
use crab_cell_runtime::cell::catalog::{CatalogEntry, CatalogRole, CellCatalog};
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::control::{Owner, authority::CellAuthority};
use crab_cell_runtime::identity::{
    ApplicationId, CellTarget, Digest, IncarnationId, NamespaceId, NodeId, RequestId, SessionId,
};
use crab_cell_runtime::ltx::{CellStorageLayout, DiskBudget, Host, Limits};
use crab_cell_runtime::node::{NodeAdvertisement, NodeCapacity, NodeDirectory, NodeFailureDomain};
use crab_cell_runtime::peer::{
    PeerAuthorizer, PeerCellResolver, PeerDispatcher, PeerPrincipal, PeerRoundTrip, PeerSigner,
    PeerVerifier, VerifiedPeerRequest,
};
use crab_cell_runtime::registry::{BuildDescriptor, Registry};
use crab_cell_runtime::{MutationIdentity, SqlWorkerPool};
use crab_ltx::{CellReplica, rusqlite};
use crab_storage::Store;
use ed25519_dalek::SigningKey;
use extenddb_auth::{AuthCacheRegistry, CredentialStore, StoredCredential};
use extenddb_core::expression::{
    CompareOp, Expr, ExpressionMaps, KeyCondition, PathElement, SortKeyCondition, UpdateAction,
};
use extenddb_core::limits::LimitsConfig;
use extenddb_core::types::{
    AttributeDefinition, AttributeValue, BillingMode, CreateTableInput, DescribeTableInput, Item,
    KeySchemaElement, KeyType, ReturnValuesOnConditionCheckFailure, ScalarAttributeType,
    TableStatus,
};
use extenddb_engine::OperationContext;
use extenddb_storage::authorization_store::AuthorizationStore;
use extenddb_storage::management_store::OpError;
use extenddb_storage::{
    BoxedFuture, DataEngine, IdempotencyKey, MetadataEngine, TableEngine, TransactGetOp,
    TransactWriteOp, error::StorageError,
};
use object_store::memory::InMemory;
use tokio_util::sync::CancellationToken;

const TEST_ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const TEST_SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TEST_ENCRYPTION_KEY: [u8; 32] = [0x36; 32];

async fn published_test_node_lease(
    layout: &CellStorageLayout,
    session: SessionId,
) -> PublishedNodeLease {
    let fleet = Digest::from_bytes([80; 32]);
    let image = Digest::from_bytes([81; 32]);
    let release = Digest::from_bytes([82; 32]);
    let directory = NodeDirectory::new(layout.clone(), fleet, image, release);
    NodeLeasePublisher::new(directory, move |now_ms, expires_at_ms| {
        NodeAdvertisement::sign(
            NodeId::from_bytes([83; 16]),
            session,
            "https://beyonddb-sort-query.internal:8081".into(),
            fleet,
            Digest::from_bytes([84; 32]),
            image,
            release,
            &SigningKey::from_bytes(&[85; 32]),
            1,
            now_ms,
            expires_at_ms,
            vec![Digest::from_bytes([86; 32])],
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
    .unwrap()
}

struct FailOnceProvisioner {
    inner: Arc<dyn InitialPartitionProvisioner>,
    failed: AtomicBool,
}

impl InitialPartitionProvisioner for FailOnceProvisioner {
    fn provision<'a>(
        &'a self,
        account_id: &'a str,
        table: &'a beyonddb::TableRecord,
    ) -> BoxedFuture<'a, Result<Vec<PartitionSpec>, StorageError>> {
        Box::pin(async move {
            let partitions = self.inner.provision(account_id, table).await?;
            if !self.failed.swap(true, Ordering::SeqCst) {
                return Err(StorageError::Transient(
                    "injected provision interruption".into(),
                ));
            }
            Ok(partitions)
        })
    }
}

fn identity(byte: u8) -> MutationIdentity {
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms + 60_000,
    }
}

struct Bootstrap<'a> {
    runtime: CellRuntime,
    registry: &'a Arc<Registry>,
    layout: &'a CellStorageLayout,
    session: SessionId,
}

impl Bootstrap<'_> {
    async fn cell(
        &self,
        target: &CellTarget,
        module: &'static str,
        byte: u8,
        file: &std::path::Path,
        initialize: for<'a> fn(&rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>,
    ) -> CellHandle {
        let proof = CellCatalog::new(self.layout.clone(), target.tenant())
            .provision(
                CatalogEntry::new(
                    target,
                    CatalogRole::Sql,
                    self.registry.module_code(module).unwrap(),
                    1,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let authority = CellAuthority::new(self.layout.clone());
        let incarnation = IncarnationId::from_bytes([byte; 16]);
        let control = authority
            .create_initial(
                &proof,
                incarnation,
                Owner {
                    session: self.session,
                    endpoint: "https://beyonddb-partition.internal:8081".into(),
                },
            )
            .await
            .unwrap();
        self.runtime
            .bootstrap(
                proof,
                CellReplica::new(
                    self.layout.clone(),
                    *target.cell_id().as_bytes(),
                    *incarnation.as_bytes(),
                    Limits::default(),
                )
                .unwrap(),
                authority,
                control,
                file.to_path_buf(),
                initialize,
            )
            .await
            .unwrap()
    }
}

struct LocalRuntimePeerResolver {
    runtime: CellRuntime,
    layout: CellStorageLayout,
}

impl PeerCellResolver for LocalRuntimePeerResolver {
    fn resolve(
        &self,
        target: CellTarget,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = crab_cell_runtime::Result<CellHandle>>
                + Send
                + 'static,
        >,
    > {
        let runtime = self.runtime.clone();
        let layout = self.layout.clone();
        Box::pin(async move {
            let proof = CellCatalog::new(layout.clone(), target.tenant())
                .lookup(target.cell_id())
                .await?
                .ok_or(crab_cell_runtime::Error::CellNotActive)?;
            let control = CellAuthority::new(layout)
                .load(target.cell_id())
                .await?
                .ok_or(crab_cell_runtime::Error::CellNotActive)?;
            runtime
                .local_handle(proof, &control)
                .await?
                .ok_or(crab_cell_runtime::Error::CellNotActive)
        })
    }
}

struct TestPeerAuthorizer;

impl PeerAuthorizer for TestPeerAuthorizer {
    fn authorize(&self, request: &VerifiedPeerRequest) -> crab_cell_runtime::Result<()> {
        if request.permits("beyonddb.cell.invoke") {
            Ok(())
        } else {
            Err(crab_cell_runtime::Error::PeerAuthorization(
                "peer action is missing",
            ))
        }
    }
}

struct LoopbackPeerRoundTrip {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
}

impl PeerRoundTrip for LoopbackPeerRoundTrip {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        let verifier = Arc::clone(&self.verifier);
        let dispatcher = Arc::clone(&self.dispatcher);
        Box::pin(async move {
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| crab_cell_runtime::Error::Peer("test clock"))?
                    .as_millis(),
            )
            .map_err(|_| crab_cell_runtime::Error::Peer("test clock overflow"))?;
            let verified = verifier.verify(&request, now_ms)?;
            if verified.target() != &target {
                return Err(crab_cell_runtime::Error::Peer("peer target changed"));
            }
            dispatcher.dispatch_bytes(&verified, now_ms).await
        })
    }
}

fn key_in_range(
    table_id: &str,
    schema: &[KeySchemaElement],
    lower_half: bool,
    start_index: usize,
) -> Item {
    let split = [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    for index in start_index..start_index + 1_000 {
        let key = Item::from([("id".into(), AttributeValue::S(format!("book-{index}")))]);
        if (data_key_hash(table_id, &key, schema).unwrap() < split) == lower_half {
            return key;
        }
    }
    panic!("failed to find a key in the requested range")
}

#[test]
fn peer_scope_allows_account_and_credentials_but_rejects_foreign_cells() {
    let scope = BeyonddbPeerScope;
    let account = account_target("123456789012").unwrap();
    let credential = credential_target(TEST_ACCESS_KEY).unwrap();
    let mut table_id = [0_u8; 32];
    table_id[..16].copy_from_slice(account.tenant().as_bytes());
    let data = data_target(
        "123456789012",
        blake3::Hash::from_bytes(table_id).to_hex().as_ref(),
        &[1; 16],
    )
    .unwrap();
    assert!(scope.check_target(&account).is_ok());
    assert!(scope.check_target(&credential).is_ok());
    assert!(scope.check_target(&data).is_ok());
    let coordinator = coordinator_target("123456789012", b"token").unwrap();
    assert!(scope.check_target(&coordinator).is_ok());
    let foreign_application = CellTarget::new(
        account.tenant(),
        ApplicationId::from_bytes([99; 16]),
        account.namespace(),
        account.partition(),
    )
    .unwrap();
    let foreign_namespace = CellTarget::new(
        account.tenant(),
        account.application(),
        NamespaceId::from_bytes([99; 16]),
        account.partition(),
    )
    .unwrap();
    assert!(scope.check_target(&foreign_application).is_err());
    assert!(scope.check_target(&foreign_namespace).is_err());
}

#[test]
fn sort_key_siblings_have_one_partition_owner() {
    let schema = vec![
        KeySchemaElement {
            attribute_name: "pk".into(),
            key_type: KeyType::Hash,
        },
        KeySchemaElement {
            attribute_name: "sk".into(),
            key_type: KeyType::Range,
        },
    ];
    let first = Item::from([
        ("pk".into(), AttributeValue::S("books".into())),
        ("sk".into(), AttributeValue::S("first".into())),
    ]);
    let second = Item::from([
        ("pk".into(), AttributeValue::S("books".into())),
        ("sk".into(), AttributeValue::S("second".into())),
    ]);
    assert_eq!(
        data_key_hash("table", &first, &schema).unwrap(),
        data_key_hash("table", &second, &schema).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_pages_cover_many_ranges_without_full_route_result() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "route-page-test".into(),
            cargo_lock_digest: Digest::from_bytes([7; 32]),
        })
        .unwrap(),
    );
    let account = account_target("123456789012").unwrap();
    let session = SessionId::from_bytes([75; 16]);
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("beyonddb-route-page-test"),
        *account.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 70).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let handle = Bootstrap {
        runtime: host.runtime(),
        registry: &registry,
        layout: &layout,
        session,
    }
    .cell(
        &account,
        "beyonddb-account",
        76,
        &directory.path().join("account.sqlite"),
        initialize_account,
    )
    .await;
    let client = host
        .application_handle::<Beyonddb>(
            CellClient::local(Arc::clone(&registry), handle.clone()),
            account.tenant(),
            account.application(),
        )
        .unwrap();
    let table = match client
        .command::<CreateTable>(
            &account,
            identity(77),
            Json(TableSpec {
                table_name: "ManyRanges".into(),
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
        .0
    {
        CreateTableOutcome::Created(table) => table,
        other => panic!("unexpected table creation: {other:?}"),
    };
    let width = u128::MAX / 65;
    let partitions = (0_u128..65)
        .map(|index| PartitionSpec {
            table: table.clone(),
            partition_id: index.to_be_bytes(),
            lower: (index > 0).then(|| (index * width).to_be_bytes()),
            upper: (index < 64).then(|| ((index + 1) * width).to_be_bytes()),
            epoch: 1,
        })
        .collect::<Vec<_>>();
    client
        .command::<ActivateTableRoute>(
            &account,
            identity(78),
            Json(TableRoute {
                table_id: table.id.clone(),
                epoch: 1,
                partitions: partitions.clone(),
            }),
        )
        .await
        .unwrap();
    let first = client
        .query::<ReadRoutePage>(
            &account,
            None,
            Json(RoutePageInput {
                table_id: table.id.clone(),
                start_hash: None,
                after_lower: None,
                expected_epoch: None,
            }),
        )
        .await
        .unwrap();
    let RoutePageOutcome::Page {
        epoch: 1,
        partitions: first,
        has_more: true,
    } = first.output.0
    else {
        panic!("expected first route page")
    };
    assert_eq!(first.len(), 64);
    let second = client
        .query::<ReadRoutePage>(
            &account,
            None,
            Json(RoutePageInput {
                table_id: table.id.clone(),
                start_hash: None,
                after_lower: Some(first[63].lower),
                expected_epoch: Some(1),
            }),
        )
        .await
        .unwrap();
    let RoutePageOutcome::Page {
        epoch: 1,
        partitions: second,
        has_more: false,
    } = second.output.0
    else {
        panic!("expected final route page")
    };
    assert_eq!(second[0].partition_id, partitions[64].partition_id);
    assert_eq!(first[63].upper, Some(second[0].lower));
    assert_eq!(
        client
            .query::<ReadRoutePage>(
                &account,
                None,
                Json(RoutePageInput {
                    table_id: table.id.clone(),
                    start_hash: None,
                    after_lower: Some(second[0].lower),
                    expected_epoch: None,
                }),
            )
            .await
            .unwrap()
            .output
            .0,
        RoutePageOutcome::Page {
            epoch: 1,
            partitions: vec![],
            has_more: false,
        }
    );
    assert_eq!(
        client
            .query::<ReadRoutePage>(
                &account,
                None,
                Json(RoutePageInput {
                    table_id: table.id.clone(),
                    start_hash: Some((64 * width + 1).to_be_bytes()),
                    after_lower: None,
                    expected_epoch: None,
                }),
            )
            .await
            .unwrap()
            .output
            .0,
        RoutePageOutcome::Page {
            epoch: 1,
            partitions: second,
            has_more: false,
        }
    );
    assert_eq!(
        client
            .query::<ReadRoutePage>(
                &account,
                None,
                Json(RoutePageInput {
                    table_id: table.id.clone(),
                    start_hash: None,
                    after_lower: Some(first[63].lower),
                    expected_epoch: Some(2),
                }),
            )
            .await
            .unwrap()
            .output
            .0,
        RoutePageOutcome::Changed
    );
    let bootstrap = Bootstrap {
        runtime: host.runtime(),
        registry: &registry,
        layout: &layout,
        session,
    };
    let mut handles = vec![handle];
    for (index, spec) in partitions.iter().enumerate() {
        let data = data_target("123456789012", &table.id, &spec.partition_id).unwrap();
        let byte = u8::try_from(index + 100).unwrap();
        let handle = bootstrap
            .cell(
                &data,
                "beyonddb-data",
                byte,
                &directory.path().join(format!("data-{index}.sqlite")),
                initialize_partition,
            )
            .await;
        CellClient::local(Arc::clone(&registry), handle.clone())
            .command::<InstallPartition>(
                &data,
                identity(byte),
                Json(PartitionInstall::Serving(spec.clone())),
            )
            .await
            .unwrap();
        handles.push(handle);
    }
    let storage = CellStorage::new(
        CellClient::local_many(registry, handles).unwrap(),
        "us-east-1",
    );
    let key_info = storage
        .table_key_info("123456789012", "ManyRanges")
        .await
        .unwrap();
    let item_in = |partition: &PartitionSpec| {
        (0..10_000)
            .map(|index| Item::from([("id".into(), AttributeValue::S(format!("key-{index}")))]))
            .find(|item| {
                let hash = data_key_hash(&table.id, item, &table.key_schema).unwrap();
                partition.lower.is_none_or(|lower| hash >= lower)
                    && partition.upper.is_none_or(|upper| hash < upper)
            })
            .unwrap()
    };
    let first_item = item_in(&partitions[0]);
    let last_item = item_in(&partitions[64]);
    for item in [&first_item, &last_item] {
        storage
            .put_item(
                &key_info,
                item.clone(),
                false,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap();
    }
    let (first_scan, continuation) = storage
        .scan(&key_info, Some(1), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(first_scan, vec![first_item.clone()]);
    let (last_scan, end) = storage
        .scan(&key_info, Some(1), continuation.as_ref(), None, None, None)
        .await
        .unwrap();
    assert_eq!(last_scan, vec![last_item.clone()]);
    assert_eq!(end, None);
    for key in [&first_item, &last_item] {
        let mut item = key.clone();
        item.insert("expires".into(), AttributeValue::N("1".into()));
        storage
            .put_item(
                &key_info,
                item,
                false,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap();
    }
    storage
        .update_ttl("123456789012", "ManyRanges", "expires", true)
        .await
        .unwrap();
    assert!(matches!(
        storage
            .create_ttl_index("123456789012", "ManyRanges", "expires")
            .await,
        Err(StorageError::Transient(_))
    ));
    assert_eq!(storage.sweep_account_ttl("123456789012").await.unwrap(), 1);
    assert_eq!(
        storage.get_item(&key_info, &first_item).await.unwrap(),
        None
    );
    assert!(
        storage
            .get_item(&key_info, &last_item)
            .await
            .unwrap()
            .is_some()
    );
    let sweep = client
        .query::<ReadTtlSweep>(&account, None, Json("ManyRanges".into()))
        .await
        .unwrap();
    assert_eq!(sweep.output.0.unwrap().after_lower, Some(first[63].lower));
    assert_eq!(storage.sweep_account_ttl("123456789012").await.unwrap(), 1);
    assert_eq!(storage.get_item(&key_info, &last_item).await.unwrap(), None);
    let sweep = client
        .query::<ReadTtlSweep>(&account, None, Json("ManyRanges".into()))
        .await
        .unwrap();
    assert_eq!(sweep.output.0.unwrap().after_lower, None);
    storage
        .update_ttl("123456789012", "ManyRanges", "expires", false)
        .await
        .unwrap();
    storage
        .drop_ttl_index("123456789012", "ManyRanges", "expires")
        .await
        .unwrap();
    let mut retained = first_item.clone();
    retained.insert("expires".into(), AttributeValue::N("1".into()));
    storage
        .put_item(
            &key_info,
            retained.clone(),
            false,
            None,
            &ExpressionMaps::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(storage.sweep_account_ttl("123456789012").await.unwrap(), 0);
    assert_eq!(
        storage.get_item(&key_info, &first_item).await.unwrap(),
        Some(retained)
    );
    for index in 0_u8..17 {
        let name = format!("A{index:02}");
        client
            .command::<CreateTable>(
                &account,
                identity(200 + index),
                Json(TableSpec {
                    table_name: name.clone(),
                    key_schema: table.key_schema.clone(),
                    attribute_definitions: table.attribute_definitions.clone(),
                    billing_mode: BillingMode::PayPerRequest,
                    provisioned_throughput: None,
                    deletion_protection_enabled: false,
                    initial_tags: Vec::new(),
                    resource_arn: None,
                }),
            )
            .await
            .unwrap();
        client
            .command::<UpdateTtl>(
                &account,
                identity(220 + index),
                Json(UpdateTtlInput {
                    table_name: name,
                    attribute_name: Some("expires".into()),
                }),
            )
            .await
            .unwrap();
    }
    storage.sweep_account_ttl("123456789012").await.unwrap();
    let schedule = client
        .query::<ReadTtlSchedule>(&account, None, Json(()))
        .await
        .unwrap();
    assert_eq!(schedule.output.0.as_deref(), Some("A15"));
    storage.sweep_account_ttl("123456789012").await.unwrap();
    let schedule = client
        .query::<ReadTtlSchedule>(&account, None, Json(()))
        .await
        .unwrap();
    assert_eq!(schedule.output.0, None);
    let large_table = match client
        .command::<CreateTable>(
            &account,
            identity(79),
            Json(TableSpec {
                table_name: "MoreRanges".into(),
                key_schema: table.key_schema.clone(),
                attribute_definitions: table.attribute_definitions.clone(),
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
        .0
    {
        CreateTableOutcome::Created(table) => table,
        other => panic!("unexpected table creation: {other:?}"),
    };
    let width = u128::MAX / 1_025;
    let large_route = TableRoute {
        table_id: large_table.id.clone(),
        epoch: 1,
        partitions: (0_u128..1_025)
            .map(|index| PartitionSpec {
                table: large_table.clone(),
                partition_id: index.to_be_bytes(),
                lower: (index > 0).then(|| (index * width).to_be_bytes()),
                upper: (index < 1_024).then(|| ((index + 1) * width).to_be_bytes()),
                epoch: 1,
            })
            .collect(),
    };
    client
        .command::<ActivateTableRoute>(&account, identity(80), Json(large_route.clone()))
        .await
        .unwrap();
    assert_eq!(
        client
            .query::<ReadTableRoute>(&account, None, Json(large_table.id.clone()))
            .await
            .unwrap()
            .output
            .0,
        Some(large_route.clone())
    );
    let source = large_route.partitions[512].clone();
    let lower = u128::from_be_bytes(source.lower.unwrap());
    let upper = u128::from_be_bytes(source.upper.unwrap());
    let boundary = (lower + (upper - lower) / 2).to_be_bytes();
    let mut left = source.clone();
    left.partition_id = [0xff; 16];
    left.upper = Some(boundary);
    left.epoch = 2;
    let mut right = source.clone();
    right.partition_id = [0xfe; 16];
    right.lower = Some(boundary);
    right.epoch = 2;
    let plan = SplitPlan {
        source,
        children: [left.clone(), right.clone()],
        expected_epoch: 1,
    };
    client
        .command::<BeginSplit>(&account, identity(81), Json(plan.clone()))
        .await
        .unwrap();
    assert_eq!(
        client
            .query::<ReadSplitRoute>(&account, None, Json(plan.clone()))
            .await
            .unwrap()
            .output
            .0,
        SplitRouteState::Before
    );
    assert_eq!(
        client
            .query::<ReadSplitPlan>(&account, None, Json(large_table.id.clone()))
            .await
            .unwrap()
            .output
            .0,
        Some(plan.clone())
    );
    client
        .command::<CommitSplit>(&account, identity(82), Json(plan.clone()))
        .await
        .unwrap();
    assert_eq!(
        client
            .query::<ReadSplitRoute>(&account, None, Json(plan))
            .await
            .unwrap()
            .output
            .0,
        SplitRouteState::After
    );
    let mut large_next_route = large_route;
    large_next_route.epoch = 2;
    large_next_route.partitions.splice(512..=512, [left, right]);
    assert_eq!(
        client
            .query::<ReadTableRoute>(&account, None, Json(large_table.id.clone()))
            .await
            .unwrap()
            .output
            .0,
        Some(large_next_route)
    );
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            Arc::clone(&application),
            layout.clone(),
            session,
            "https://beyonddb-partition.internal:8081".into(),
            directory.path().join("large-route-provisioner"),
        )
        .unwrap(),
    );
    let adapter = CellStorage::new(
        CellClient::local_runtime(application.registry(), host.runtime(), layout.clone()),
        "us-east-1",
    )
    .with_initial_partitions(provisioner.clone())
    .with_transaction_coordinators(provisioner);
    assert_eq!(
        adapter
            .table_key_info("123456789012", "MoreRanges")
            .await
            .unwrap()
            .table_id,
        large_table.id
    );
    assert_eq!(
        adapter
            .describe_table(
                "123456789012",
                DescribeTableInput {
                    table_name: "MoreRanges".into(),
                },
            )
            .await
            .unwrap()
            .table_status,
        TableStatus::Active
    );
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published_node_lease_renews_before_drain() {
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("beyonddb-lease-test"),
        [42; 16],
    );
    let session = SessionId::from_bytes([90; 16]);
    let directory = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([80; 32]),
        Digest::from_bytes([81; 32]),
        Digest::from_bytes([82; 32]),
    );
    let published = published_test_node_lease(&layout, session).await;
    let guard = published.guard();
    let cancellation = CancellationToken::new();
    let run_cancellation = cancellation.clone();
    let task = tokio::spawn(async move { published.run(&run_cancellation).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap();
            if directory
                .load(session, now_ms)
                .await
                .unwrap()
                .unwrap()
                .advertisement()
                .generation()
                > 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();
    task.await.unwrap().unwrap();
    guard.check().unwrap();
    guard.fence();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn numeric_sort_query_pages_in_key_order_through_one_data_cell() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "sort-query-test".into(),
            cargo_lock_digest: Digest::from_bytes([3; 32]),
        })
        .unwrap(),
    );
    let account = account_target("123456789012").unwrap();
    let session = SessionId::from_bytes([70; 16]);
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("beyonddb-sort-query-test"),
        *account.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 32).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build()
        .unwrap();
    let lease_cancellation = CancellationToken::new();
    let tasks = host
        .install_task_group(lease_cancellation.clone(), CancellationToken::new())
        .unwrap();
    let published_lease = published_test_node_lease(&layout, session).await;
    host.install_node_lease_for_startup(published_lease.guard())
        .unwrap();
    tasks
        .spawn(async move { published_lease.run(&lease_cancellation).await })
        .unwrap();
    host.start().unwrap();
    let registry = application.registry();
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            Arc::clone(&application),
            layout.clone(),
            session,
            "https://beyonddb-sort-query.internal:8081".into(),
            directory.path().join("data"),
        )
        .unwrap(),
    );
    let account_handle = Box::pin(provisioner.admit_account("123456789012"))
        .await
        .unwrap();
    let creator = CellStorage::new(
        CellClient::local(Arc::clone(&registry), account_handle.clone()),
        "us-east-1",
    )
    .with_initial_partitions(provisioner.clone())
    .with_transaction_coordinators(provisioner.clone());
    let created = creator
        .create_table(
            "123456789012",
            CreateTableInput {
                table_name: "Numbers".into(),
                key_schema: vec![
                    KeySchemaElement {
                        attribute_name: "pk".into(),
                        key_type: KeyType::Hash,
                    },
                    KeySchemaElement {
                        attribute_name: "sk".into(),
                        key_type: KeyType::Range,
                    },
                ],
                attribute_definitions: vec![
                    AttributeDefinition {
                        attribute_name: "pk".into(),
                        attribute_type: ScalarAttributeType::S,
                    },
                    AttributeDefinition {
                        attribute_name: "sk".into(),
                        attribute_type: ScalarAttributeType::N,
                    },
                ],
                billing_mode: Some(BillingMode::PayPerRequest),
                ..CreateTableInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(created.table_status, TableStatus::Active);
    let account_client = host
        .application_handle::<Beyonddb>(
            CellClient::local(Arc::clone(&registry), account_handle.clone()),
            account.tenant(),
            account.application(),
        )
        .unwrap();
    let route = account_client
        .query::<ReadTableRoute>(&account, None, Json(created.table_id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let data = data_target(
        "123456789012",
        &created.table_id,
        &route.partitions[0].partition_id,
    )
    .unwrap();
    let proof = CellCatalog::new(layout.clone(), data.tenant())
        .lookup(data.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let control = authority.load(data.cell_id()).await.unwrap().unwrap();
    let data_handle = host
        .runtime()
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap();
    let storage = CellStorage::new(
        CellClient::local_runtime(Arc::clone(&registry), host.runtime(), layout.clone()),
        "us-east-1",
    )
    .with_initial_partitions(provisioner.clone())
    .with_transaction_coordinators(provisioner.clone());
    let key_info = storage
        .table_key_info("123456789012", "Numbers")
        .await
        .unwrap();
    for number in ["10", "-10", "2", "1.5"] {
        let item = Item::from([
            ("pk".into(), AttributeValue::S("same".into())),
            ("sk".into(), AttributeValue::N(number.into())),
        ]);
        storage
            .put_item(
                &key_info,
                item,
                false,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap();
    }
    let usage = CellClient::local(Arc::clone(&registry), data_handle.clone())
        .query::<PartitionUsage>(&data, None, Json(()))
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(usage.item_count, 4);
    assert!(usage.item_bytes > 0);
    assert!(usage.database_bytes > usage.item_bytes);
    let equivalent_numeric_key: Item =
        serde_json::from_str(r#"{"pk":{"S":"same"},"sk":{"N":"2.0"}}"#).unwrap();
    assert_eq!(
        storage
            .get_item(&key_info, &equivalent_numeric_key)
            .await
            .unwrap()
            .unwrap()["sk"],
        AttributeValue::N("2".into())
    );
    let condition = KeyCondition {
        pk_path: vec![PathElement::Attribute("pk".into())],
        pk_value: Expr::Placeholder("p".into()),
        extra_pk_conditions: vec![],
        sk_condition: None,
        extra_sk_conditions: vec![],
    };
    let maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("p".into(), AttributeValue::S("same".into()))]),
    );
    let (first, cursor) = storage
        .query(&key_info, &condition, &maps, true, Some(2), None, None)
        .await
        .unwrap();
    assert_eq!(
        first
            .iter()
            .map(|item| item["sk"].clone())
            .collect::<Vec<_>>(),
        vec![
            AttributeValue::N("-10".into()),
            AttributeValue::N("1.5".into())
        ]
    );
    let (second, end) = storage
        .query(
            &key_info,
            &condition,
            &maps,
            true,
            Some(2),
            cursor.as_ref(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        second
            .iter()
            .map(|item| item["sk"].clone())
            .collect::<Vec<_>>(),
        vec![
            AttributeValue::N("2".into()),
            AttributeValue::N("10".into())
        ]
    );
    assert_eq!(end, None);
    let range = KeyCondition {
        sk_condition: Some(SortKeyCondition::Compare {
            path: vec![PathElement::Attribute("sk".into())],
            op: CompareOp::Gt,
            value: Expr::Placeholder("min".into()),
        }),
        ..condition
    };
    let range_maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([
            ("p".into(), AttributeValue::S("same".into())),
            ("min".into(), AttributeValue::N("0".into())),
        ]),
    );
    let (reverse, end) = storage
        .query(&key_info, &range, &range_maps, false, Some(10), None, None)
        .await
        .unwrap();
    assert_eq!(
        reverse
            .iter()
            .map(|item| item["sk"].clone())
            .collect::<Vec<_>>(),
        vec![
            AttributeValue::N("10".into()),
            AttributeValue::N("2".into()),
            AttributeValue::N("1.5".into()),
        ]
    );
    assert_eq!(end, None);
    let new_first = Item::from([
        ("pk".into(), AttributeValue::S("same".into())),
        ("sk".into(), AttributeValue::N("3".into())),
    ]);
    let new_second = Item::from([
        ("pk".into(), AttributeValue::S("same".into())),
        ("sk".into(), AttributeValue::N("4".into())),
    ]);
    let tx_maps = ExpressionMaps::default();
    storage
        .transact_write_items(
            &[
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &new_first,
                    condition: None,
                    maps: &tx_maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &new_second,
                    condition: None,
                    maps: &tx_maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
            ],
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        storage
            .transact_get_items(&[
                TransactGetOp {
                    key_info: &key_info,
                    key: &new_first,
                },
                TransactGetOp {
                    key_info: &key_info,
                    key: &new_second,
                },
            ])
            .await
            .unwrap(),
        vec![Some(new_first), Some(new_second)]
    );
    let (after_write, _) = storage
        .query(&key_info, &range, &range_maps, true, Some(10), None, None)
        .await
        .unwrap();
    assert_eq!(
        after_write
            .iter()
            .map(|item| item["sk"].clone())
            .collect::<Vec<_>>(),
        vec![
            AttributeValue::N("1.5".into()),
            AttributeValue::N("2".into()),
            AttributeValue::N("3".into()),
            AttributeValue::N("4".into()),
            AttributeValue::N("10".into()),
        ]
    );
    elastic_cells::transaction_visibility::assert_range_read_barriers(
        &storage,
        &CellClient::local(Arc::clone(&registry), data_handle.clone()),
        &data,
        route.partitions[0].epoch,
        &key_info,
    )
    .await;
    let usage_before_split = CellClient::local(Arc::clone(&registry), data_handle.clone())
        .query::<PartitionUsage>(&data, None, Json(()))
        .await
        .unwrap()
        .output
        .0;
    let split_threshold = usage_before_split.database_bytes - 1;
    assert!(split_threshold > usage_before_split.item_bytes);
    assert!(
        provisioner
            .reconcile_table_capacity(
                "123456789012",
                account_handle.clone(),
                &created.table_id,
                usage_before_split.database_bytes,
            )
            .await
            .unwrap()
            .is_none()
    );
    let mut token_items = [None, None];
    for index in 0..100 {
        let item = Item::from([
            (
                "pk".into(),
                AttributeValue::S(format!("split-token-{index}")),
            ),
            ("sk".into(), AttributeValue::N("0".into())),
        ]);
        let hash = data_key_hash(&created.table_id, &item, &key_info.base_key_schema).unwrap();
        token_items[usize::from(hash[0] >> 7)].get_or_insert(item);
        if token_items.iter().all(Option::is_some) {
            break;
        }
    }
    let [Some(left_token_item), Some(right_token_item)] = token_items else {
        panic!("both split ranges need a transaction key");
    };
    let token_writes = [
        TransactWriteOp::Put {
            key_info: &key_info,
            item: &left_token_item,
            condition: None,
            maps: &tx_maps,
            return_values_on_ccf: Default::default(),
            stream: None,
        },
        TransactWriteOp::Put {
            key_info: &key_info,
            item: &right_token_item,
            condition: None,
            maps: &tx_maps,
            return_values_on_ccf: Default::default(),
            stream: None,
        },
    ];
    storage
        .transact_write_items(
            &token_writes,
            Some(IdempotencyKey {
                account_id: "123456789012",
                token: "before-split",
                fingerprint: "two-ranges",
            }),
        )
        .await
        .unwrap();
    provisioner
        .install_account_capacity_loop(
            &tasks,
            "123456789012".into(),
            account_handle.clone(),
            split_threshold,
            Duration::from_secs(5),
        )
        .unwrap();
    let grown = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(host.is_ready(), "capacity loop stopped serving");
            if let Some(route) = account_client
                .query::<ReadTableRoute>(&account, None, Json(created.table_id.clone()))
                .await
                .unwrap()
                .output
                .0
                && route.epoch == 2
            {
                break route;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(grown.partitions.len(), 2);
    assert_eq!(grown.epoch, 2);
    let replay_after_split = storage
        .transact_write_items(
            &token_writes,
            Some(IdempotencyKey {
                account_id: "123456789012",
                token: "before-split",
                fingerprint: "two-ranges",
            }),
        )
        .await;
    assert!(matches!(
        replay_after_split,
        Err(StorageError::IdempotentReplay)
    ));
    let first_range = provisioner
        .reconcile_account_capacity("123456789012", account_handle.clone(), u64::MAX, None)
        .await
        .unwrap();
    assert!(first_range.split.is_none());
    assert_eq!(
        first_range.cursor.as_ref().unwrap().after_lower,
        Some([0; 16])
    );
    let second_range = provisioner
        .reconcile_account_capacity(
            "123456789012",
            account_handle.clone(),
            u64::MAX,
            first_range.cursor.as_ref(),
        )
        .await
        .unwrap();
    assert!(second_range.split.is_none());
    assert_eq!(second_range.cursor.as_ref().unwrap().after_lower, None);
    assert!(
        provisioner
            .reconcile_account_capacity(
                "123456789012",
                account_handle.clone(),
                u64::MAX,
                second_range.cursor.as_ref(),
            )
            .await
            .unwrap()
            .cursor
            .is_none()
    );
    assert_eq!(
        account_client
            .query::<ReadTableRoute>(&account, None, Json(created.table_id.clone()))
            .await
            .unwrap()
            .output
            .0,
        Some(grown.clone())
    );
    let completed = provisioner
        .split_if_over_database_bytes(
            "123456789012",
            account_handle.clone(),
            &created.table_id,
            route.partitions[0].partition_id,
            1,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completed.source, route.partitions[0]);
    assert_eq!(completed.children.as_slice(), grown.partitions.as_slice());
    let hash = data_key_hash(
        &created.table_id,
        &after_write[0],
        &key_info.base_key_schema,
    )
    .unwrap();
    let owner = grown
        .partitions
        .iter()
        .find(|partition| {
            partition.lower.is_none_or(|lower| hash >= lower)
                && partition.upper.is_none_or(|upper| hash < upper)
        })
        .unwrap();
    let split = provisioner
        .split_partition(
            "123456789012",
            account_handle.clone(),
            &created.table_id,
            owner.partition_id,
        )
        .await
        .unwrap();
    assert_eq!(split.source.partition_id, owner.partition_id);
    assert_eq!(split.expected_epoch, 2);
    let grown = account_client
        .query::<ReadTableRoute>(&account, None, Json(created.table_id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(grown.partitions.len(), 3);
    assert_eq!(grown.epoch, 3);
    let mut children = Vec::new();
    for partition in &grown.partitions {
        let target =
            data_target("123456789012", &created.table_id, &partition.partition_id).unwrap();
        let proof = CellCatalog::new(layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let control = CellAuthority::new(layout.clone())
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        children.push(
            host.runtime()
                .local_handle(proof, &control)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    let routed = Arc::new(
        CellStorage::new(
            CellClient::local_runtime(Arc::clone(&registry), host.runtime(), layout.clone()),
            "us-east-1",
        )
        .with_initial_partitions(provisioner.clone())
        .with_transaction_coordinators(provisioner.clone()),
    );
    assert_eq!(
        routed.get_item(&key_info, &left_token_item).await.unwrap(),
        Some(left_token_item)
    );
    assert_eq!(
        routed.get_item(&key_info, &right_token_item).await.unwrap(),
        Some(right_token_item)
    );
    let (after_split, _) = routed
        .query(&key_info, &range, &range_maps, true, Some(10), None, None)
        .await
        .unwrap();
    assert_eq!(after_split, after_write);
    let credential_target = credential_target(TEST_ACCESS_KEY).unwrap();
    let credential_handle = Box::pin(provisioner.admit_credential(TEST_ACCESS_KEY))
        .await
        .unwrap();
    let credential_store = CellCredentialStore::new(
        CellClient::local(Arc::clone(&registry), credential_handle.clone()),
        layout.clone(),
        TEST_ENCRYPTION_KEY,
    );
    credential_store
        .put_credential(
            TEST_ACCESS_KEY,
            StoredCredential {
                secret_key: TEST_SECRET_KEY.into(),
                account_id: "123456789012".into(),
                principal_name: "test-user".into(),
                session_name: None,
                is_session: false,
                session_token: None,
                is_active: true,
                expires_at: None,
            },
        )
        .await
        .unwrap();
    let authorization = Arc::new(CellAuthorizationStore::new(CellClient::local_runtime(
        Arc::clone(&registry),
        host.runtime(),
        layout.clone(),
    )));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let peer_session = SessionId::from_bytes([77; 16]);
    let peer_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 32).unwrap(),
        16 * 1024 * 1024,
        peer_session,
    )
    .unwrap();
    let signer = PeerSigner::new(
        peer_session,
        registry.release_digest(),
        SigningKey::from_bytes(&[78; 32]),
    );
    let verifier = Arc::new(PeerVerifier::new(
        peer_session,
        registry.release_digest(),
        signer.verifying_key(),
    ));
    let dispatcher = Arc::new(PeerDispatcher::new(
        Arc::clone(&registry),
        Arc::new(LocalRuntimePeerResolver {
            runtime: host.runtime(),
            layout: layout.clone(),
        }),
        Arc::new(TestPeerAuthorizer),
    ));
    let routed_client = CellClient::runtime_with_peer(
        Arc::clone(&registry),
        peer_runtime.clone(),
        layout.clone(),
        Arc::new(signer),
        PeerPrincipal {
            issuer: "beyonddb-test".into(),
            subject: "sdk-server".into(),
            actions: vec!["beyonddb.cell.invoke".into()],
        },
        Arc::new(LoopbackPeerRoundTrip {
            verifier,
            dispatcher,
        }),
    );
    let state = build_http_state(
        &host,
        routed_client,
        layout.clone(),
        provisioner.clone(),
        TEST_ENCRYPTION_KEY,
        "us-east-1",
        endpoint.clone(),
    )
    .unwrap();
    assert!(matches!(
        state
            .catalog_store
            .as_ref()
            .unwrap()
            .list_all_accounts()
            .await,
        Err(OpError::Internal(_))
    ));
    let server = tokio::spawn(extenddb_server::start_server(listener, state, None, None));
    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            TEST_ACCESS_KEY,
            TEST_SECRET_KEY,
            None,
            None,
            "beyonddb-test",
        ))
        .endpoint_url(endpoint.clone())
        .load()
        .await;
    let sdk = aws_sdk_dynamodb::Client::new(&config);
    let sdk_item = HashMap::from([
        ("pk".to_owned(), AwsAttributeValue::S("same".into())),
        ("sk".to_owned(), AwsAttributeValue::N("8".into())),
    ]);
    let denied_without_policy = sdk
        .put_item()
        .table_name("Numbers")
        .set_item(Some(sdk_item.clone()))
        .send()
        .await
        .unwrap_err();
    assert!(format!("{denied_without_policy:?}").contains("AccessDeniedException"));
    let policy_document = serde_json::json!({
        "Version": "2012-10-17",
        "Statement": [{
            "Effect": "Allow",
            "Action": "dynamodb:*",
            "Resource": [
                "arn:aws:dynamodb:us-east-1:123456789012:table/Numbers",
                "arn:aws:dynamodb:us-east-1:123456789012:table/NewTable"
            ]
        }]
    })
    .to_string();
    authorization
        .put_user_policy("123456789012", "test-user", "tables", &policy_document)
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        sdk.put_item()
            .table_name("Numbers")
            .set_item(Some(sdk_item.clone()))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    let sdk_read = tokio::time::timeout(
        Duration::from_secs(10),
        sdk.get_item()
            .table_name("Numbers")
            .set_key(Some(sdk_item.clone()))
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(sdk_read.item(), Some(&sdk_item));
    let same_hash = data_key_hash(
        &key_info.table_id,
        &Item::from([("pk".into(), AttributeValue::S("same".into()))]),
        &key_info.base_key_schema,
    )
    .unwrap();
    let same_partition = grown
        .partitions
        .iter()
        .find(|partition| {
            partition.lower.is_none_or(|lower| same_hash >= lower)
                && partition.upper.is_none_or(|upper| same_hash < upper)
        })
        .unwrap();
    elastic_cells::transaction_visibility::assert_sdk_read_barrier(
        &sdk,
        &CellClient::local_runtime(Arc::clone(&registry), host.runtime(), layout.clone()),
        same_partition,
        &key_info.account_id,
    )
    .await;
    let described = sdk
        .describe_table()
        .table_name("Numbers")
        .send()
        .await
        .unwrap();
    assert_eq!(
        described.table().and_then(|table| table.table_name()),
        Some("Numbers")
    );
    let new_table = sdk
        .create_table()
        .table_name("NewTable")
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
    assert_eq!(
        new_table
            .table_description()
            .and_then(|table| table.table_name()),
        Some("NewTable")
    );
    let new_table_id = new_table
        .table_description()
        .and_then(|table| table.table_id())
        .unwrap()
        .to_owned();
    let new_route = account_client
        .query::<ReadTableRoute>(&account, None, Json(new_table_id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let new_table_sweep = provisioner
        .reconcile_account_capacity("123456789012", account_handle.clone(), u64::MAX, None)
        .await
        .unwrap();
    assert_eq!(
        new_table_sweep.cursor.as_ref().unwrap().table_name,
        "NewTable"
    );
    let numbers_sweep = provisioner
        .reconcile_account_capacity(
            "123456789012",
            account_handle.clone(),
            u64::MAX,
            new_table_sweep.cursor.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(numbers_sweep.cursor.as_ref().unwrap().table_name, "Numbers");
    assert_eq!(
        numbers_sweep.cursor.as_ref().unwrap().after_lower,
        Some([0; 16])
    );
    let new_target = data_target(
        "123456789012",
        &new_table_id,
        &new_route.partitions[0].partition_id,
    )
    .unwrap();
    let new_item = HashMap::from([("id".to_owned(), AwsAttributeValue::S("first".into()))]);
    sdk.put_item()
        .table_name("NewTable")
        .set_item(Some(new_item.clone()))
        .send()
        .await
        .unwrap();
    let new_read = sdk
        .get_item()
        .table_name("NewTable")
        .set_key(Some(new_item.clone()))
        .send()
        .await
        .unwrap();
    assert_eq!(new_read.item(), Some(&new_item));
    let mut queried = Vec::new();
    let mut query_cursor = None;
    for _ in 0..10 {
        let page = sdk
            .query()
            .table_name("Numbers")
            .key_condition_expression("pk = :pk")
            .expression_attribute_values(":pk", AwsAttributeValue::S("same".into()))
            .limit(3)
            .set_exclusive_start_key(query_cursor)
            .send()
            .await
            .unwrap();
        queried.extend_from_slice(page.items());
        query_cursor = page.last_evaluated_key().cloned();
        if query_cursor.is_none() {
            break;
        }
    }
    assert!(query_cursor.is_none());
    assert_eq!(queried.len(), 7);
    assert_eq!(queried[0]["sk"], AwsAttributeValue::N("-10".into()));
    assert_eq!(queried[6]["sk"], AwsAttributeValue::N("10".into()));
    let mut scanned = Vec::new();
    let mut scan_cursor = None;
    for _ in 0..10 {
        let page = sdk
            .scan()
            .table_name("Numbers")
            .limit(3)
            .set_exclusive_start_key(scan_cursor)
            .send()
            .await
            .unwrap();
        scanned.extend_from_slice(page.items());
        scan_cursor = page.last_evaluated_key().cloned();
        if scan_cursor.is_none() {
            break;
        }
    }
    assert!(scan_cursor.is_none());
    assert_eq!(scanned.len(), 9);
    assert!(scanned.contains(&sdk_item));
    let mutation_key = HashMap::from([
        ("pk".to_owned(), AwsAttributeValue::S("same".into())),
        ("sk".to_owned(), AwsAttributeValue::N("9".into())),
    ]);
    sdk.put_item()
        .table_name("Numbers")
        .set_item(Some(mutation_key.clone()))
        .send()
        .await
        .unwrap();
    sdk.update_item()
        .table_name("Numbers")
        .set_key(Some(mutation_key.clone()))
        .update_expression("SET note = :note")
        .expression_attribute_values(":note", AwsAttributeValue::S("updated".into()))
        .condition_expression("attribute_exists(pk)")
        .send()
        .await
        .unwrap();
    let updated = sdk
        .get_item()
        .table_name("Numbers")
        .set_key(Some(mutation_key.clone()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        updated.item().and_then(|item| item.get("note")),
        Some(&AwsAttributeValue::S("updated".into()))
    );
    sdk.delete_item()
        .table_name("Numbers")
        .set_key(Some(mutation_key.clone()))
        .condition_expression("attribute_exists(pk)")
        .send()
        .await
        .unwrap();
    let deleted = sdk
        .get_item()
        .table_name("Numbers")
        .set_key(Some(mutation_key))
        .send()
        .await
        .unwrap();
    assert!(deleted.item().is_none());
    let denied_table = sdk
        .describe_table()
        .table_name("Forbidden")
        .send()
        .await
        .unwrap_err();
    assert!(format!("{denied_table:?}").contains("AccessDeniedException"));
    assert!(
        authorization
            .delete_user_policy("123456789012", "test-user", "tables")
            .await
            .unwrap()
    );
    let denied_after_removal = sdk
        .get_item()
        .table_name("Numbers")
        .set_key(Some(sdk_item.clone()))
        .send()
        .await
        .unwrap_err();
    assert!(format!("{denied_after_removal:?}").contains("AccessDeniedException"));
    authorization
        .put_user_policy("123456789012", "test-user", "tables", &policy_document)
        .await
        .unwrap();
    let wrong_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            TEST_ACCESS_KEY,
            "incorrect-test-secret",
            None,
            None,
            "beyonddb-negative-test",
        ))
        .endpoint_url(endpoint)
        .load()
        .await;
    let wrong_sdk = aws_sdk_dynamodb::Client::new(&wrong_config);
    let rejected = tokio::time::timeout(
        Duration::from_secs(10),
        wrong_sdk
            .get_item()
            .table_name("Numbers")
            .set_key(Some(sdk_item.clone()))
            .send(),
    )
    .await
    .unwrap();
    assert!(rejected.is_err());
    let management_store = CellCredentialStore::new(
        CellClient::local(Arc::clone(&registry), credential_handle.clone()),
        layout.clone(),
        TEST_ENCRYPTION_KEY,
    );
    assert!(
        management_store
            .revoke_credential(TEST_ACCESS_KEY)
            .await
            .unwrap()
    );
    assert!(
        management_store
            .revoke_credential(TEST_ACCESS_KEY)
            .await
            .unwrap()
    );
    let revoked = tokio::time::timeout(
        Duration::from_secs(10),
        sdk.get_item()
            .table_name("Numbers")
            .set_key(Some(sdk_item.clone()))
            .send(),
    )
    .await
    .unwrap();
    assert!(revoked.is_err());
    server.abort();
    peer_runtime.shutdown().await.unwrap();
    let context = OperationContext {
        storage: routed.clone(),
        limits: Arc::new(LimitsConfig::default()),
        region: Arc::from("us-east-1"),
        account_id: Arc::from("123456789012"),
        import_paths: Arc::from([]),
        export_paths: Arc::from([]),
        pre_fetched_key_info: None,
        auth_cache: AuthCacheRegistry::empty(),
        table_key_info_lookup: None,
    };
    let wire_item = serde_json::json!({"pk": {"S": "same"}, "sk": {"N": "7"}});
    let written = extenddb_engine::dispatch(
        "PutItem",
        serde_json::json!({"TableName": "Numbers", "Item": wire_item}),
        &context,
        "http://localhost",
    )
    .await
    .unwrap();
    assert!(written.body.get("Attributes").is_none());
    let wire_read = extenddb_engine::dispatch(
        "GetItem",
        serde_json::json!({"TableName": "Numbers", "Key": wire_item}),
        &context,
        "http://localhost",
    )
    .await
    .unwrap();
    assert_eq!(wire_read.body["Item"], wire_item);
    let owner = grown
        .partitions
        .iter()
        .find(|partition| {
            partition.lower.is_none_or(|lower| hash >= lower)
                && partition.upper.is_none_or(|upper| hash < upper)
        })
        .unwrap();
    let owner_target = data_target("123456789012", &created.table_id, &owner.partition_id).unwrap();
    let owner_epoch = owner.epoch;
    for child in children {
        child.drain().await.unwrap();
    }
    let new_proof = CellCatalog::new(layout.clone(), new_target.tenant())
        .lookup(new_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let new_control = CellAuthority::new(layout.clone())
        .load(new_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    host.runtime()
        .local_handle(new_proof, &new_control)
        .await
        .unwrap()
        .unwrap()
        .drain()
        .await
        .unwrap();
    credential_handle.drain().await.unwrap();
    let credential_directory = directory.path().join("data").join(
        blake3::Hash::from_bytes(*credential_target.cell_id().as_bytes())
            .to_hex()
            .as_str(),
    );
    let credential_files = std::fs::read_dir(credential_directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite")
        })
        .collect::<Vec<_>>();
    assert_eq!(credential_files.len(), 1);
    let stored_record: Vec<u8> = rusqlite::Connection::open_with_flags(
        &credential_files[0],
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row(
        "SELECT record FROM ddb_credentials WHERE access_key_id = ?1",
        [TEST_ACCESS_KEY],
        |row| row.get(0),
    )
    .unwrap();
    assert!(
        !stored_record
            .windows(TEST_SECRET_KEY.len())
            .any(|window| window == TEST_SECRET_KEY.as_bytes())
    );
    data_handle.drain().await.unwrap();
    account_handle.drain().await.unwrap();
    host.shutdown().await.unwrap();

    let next_session = SessionId::from_bytes([75; 16]);
    let restored_host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 32).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(next_session)
        .build_unleased_for_maintenance()
        .unwrap();
    let proof = CellCatalog::new(layout.clone(), owner_target.tenant())
        .lookup(owner_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let idle = authority
        .load(owner_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored = restored_host
        .runtime()
        .acquire_idle_restored(
            proof,
            CellReplica::new(
                layout.clone(),
                *owner_target.cell_id().as_bytes(),
                *idle.value().incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            authority,
            idle,
            directory.path().join("restored-numeric-owner.sqlite"),
            Owner {
                session: next_session,
                endpoint: "https://restored-numeric-owner.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let new_proof = CellCatalog::new(layout.clone(), new_target.tenant())
        .lookup(new_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let new_authority = CellAuthority::new(layout.clone());
    let new_idle = new_authority
        .load(new_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored_new = restored_host
        .runtime()
        .acquire_idle_restored(
            new_proof,
            CellReplica::new(
                layout.clone(),
                *new_target.cell_id().as_bytes(),
                *new_idle.value().incarnation.as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            new_authority,
            new_idle,
            directory.path().join("restored-new-table.sqlite"),
            Owner {
                session: next_session,
                endpoint: "https://restored-new-table.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_provisioner = CellInitialPartitionProvisioner::new(
        restored_host.runtime(),
        application.clone(),
        layout.clone(),
        next_session,
        "https://restored-beyonddb.internal:8081".into(),
        directory.path().join("restored-cells"),
    )
    .unwrap();
    let restored_account = Box::pin(restored_provisioner.admit_account("123456789012"))
        .await
        .unwrap();
    let restored_authorization = CellAuthorizationStore::new(CellClient::local_runtime(
        application.registry(),
        restored_host.runtime(),
        layout.clone(),
    ));
    assert_eq!(
        restored_authorization
            .fetch_user_policies("123456789012", "test-user")
            .await
            .unwrap(),
        vec![policy_document]
    );
    let new_client = CellClient::local_runtime(
        application.registry(),
        restored_host.runtime(),
        layout.clone(),
    );
    let new_key = Item::from([("id".into(), AttributeValue::S("first".into()))]);
    assert_eq!(
        new_client
            .query::<PartitionGet>(
                &new_target,
                None,
                Json(PartitionGetInput {
                    table_id: new_table_id,
                    epoch: new_route.partitions[0].epoch,
                    key: new_key.clone(),
                }),
            )
            .await
            .unwrap()
            .output
            .0,
        PartitionGetOutcome::Found(Some(new_key))
    );
    let restored_client = restored_host
        .application_handle::<Beyonddb>(
            CellClient::local(application.registry(), restored.clone()),
            owner_target.tenant(),
            owner_target.application(),
        )
        .unwrap();
    let restored_credential = Box::pin(restored_provisioner.admit_credential(TEST_ACCESS_KEY))
        .await
        .unwrap();
    let restored_store = CellCredentialStore::new(
        CellClient::local(application.registry(), restored_credential.clone()),
        layout.clone(),
        TEST_ENCRYPTION_KEY,
    );
    assert!(
        restored_store
            .lookup_credential(TEST_ACCESS_KEY)
            .await
            .unwrap()
            .is_some_and(
                |credential| credential.secret_key == TEST_SECRET_KEY && !credential.is_active
            )
    );
    let wrong_key_store = CellCredentialStore::new(
        CellClient::local(application.registry(), restored_credential.clone()),
        layout,
        [0; 32],
    );
    assert!(
        wrong_key_store
            .lookup_credential(TEST_ACCESS_KEY)
            .await
            .is_err()
    );
    let expected_item: Item = serde_json::from_value(wire_item).unwrap();
    let persisted = restored_client
        .query::<PartitionGet>(
            &owner_target,
            None,
            Json(PartitionGetInput {
                table_id: created.table_id.clone(),
                epoch: owner_epoch,
                key: expected_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        persisted.output.0,
        PartitionGetOutcome::Found(Some(expected_item))
    );
    let sdk_persisted = restored_client
        .query::<PartitionGet>(
            &owner_target,
            None,
            Json(PartitionGetInput {
                table_id: created.table_id,
                epoch: owner_epoch,
                key: Item::from([
                    ("pk".into(), AttributeValue::S("same".into())),
                    ("sk".into(), AttributeValue::N("8".into())),
                ]),
            }),
        )
        .await
        .unwrap();
    assert!(matches!(
        sdk_persisted.output.0,
        PartitionGetOutcome::Found(Some(_))
    ));
    restored.drain().await.unwrap();
    restored_account.drain().await.unwrap();
    restored_new.drain().await.unwrap();
    restored_credential.drain().await.unwrap();
    restored_host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_ranges_use_independent_cells_and_survive_owner_restart() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "partition-cell-test".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target("123456789012").unwrap();
    let session = SessionId::from_bytes([11; 16]);
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("beyonddb-partition-test"),
        *account.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 32).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let bootstrap = Bootstrap {
        runtime: host.runtime(),
        registry: &registry,
        layout: &layout,
        session,
    };
    let account_handle = bootstrap
        .cell(
            &account,
            "beyonddb-account",
            12,
            &directory.path().join("account.sqlite"),
            initialize_account,
        )
        .await;
    let account_client = host
        .application_handle::<Beyonddb>(
            CellClient::local(Arc::clone(&registry), account_handle.clone()),
            account.tenant(),
            account.application(),
        )
        .unwrap();
    let table = match account_client
        .command::<CreateTable>(
            &account,
            identity(13),
            Json(TableSpec {
                table_name: "Books".into(),
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
        .0
    {
        CreateTableOutcome::Created(table) => table,
        other => panic!("unexpected create outcome: {other:?}"),
    };
    let split = [0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let left_id = [1; 16];
    let right_id = [2; 16];
    let left_target = data_target("123456789012", &table.id, &left_id).unwrap();
    let right_target = data_target("123456789012", &table.id, &right_id).unwrap();
    assert_ne!(left_target.cell_id(), right_target.cell_id());
    let left_handle = bootstrap
        .cell(
            &left_target,
            "beyonddb-data",
            14,
            &directory.path().join("left.sqlite"),
            initialize_partition,
        )
        .await;
    let right_handle = bootstrap
        .cell(
            &right_target,
            "beyonddb-data",
            15,
            &directory.path().join("right.sqlite"),
            initialize_partition,
        )
        .await;
    let transaction_id = [110; 16];
    let coordinator_target = coordinator_target("123456789012", &transaction_id).unwrap();
    let coordinator_handle = bootstrap
        .cell(
            &coordinator_target,
            "beyonddb-coordinator",
            109,
            &directory.path().join("coordinator.sqlite"),
            initialize_coordinator,
        )
        .await;
    let coordinator_admission = CellInitialPartitionProvisioner::new(
        host.runtime(),
        Arc::clone(&application),
        layout.clone(),
        session,
        "https://beyonddb-partition.internal:8081".into(),
        directory.path().join("registered-coordinator"),
    )
    .unwrap();
    coordinator_admission
        .admit_coordinator("123456789012", &transaction_id)
        .await
        .unwrap();
    let cell_client =
        CellClient::local_runtime(Arc::clone(&registry), host.runtime(), layout.clone());
    let storage = CellStorage::new(cell_client.clone(), "us-east-1")
        .with_transaction_coordinators(Arc::new(coordinator_admission));
    let client = host
        .application_handle::<Beyonddb>(cell_client, account.tenant(), account.application())
        .unwrap();
    let shard = u32::from_be_bytes(coordinator_target.partition().try_into().unwrap());
    let registered = client
        .query::<ListCoordinatorShards>(
            &account,
            None,
            Json(ListCoordinatorShardsInput {
                account_id: "123456789012".into(),
                after: None,
                limit: 1,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(registered, vec![shard]);
    let partitions = [
        PartitionSpec {
            table: table.clone(),
            partition_id: left_id,
            lower: None,
            upper: Some(split),
            epoch: 1,
        },
        PartitionSpec {
            table: table.clone(),
            partition_id: right_id,
            lower: Some(split),
            upper: None,
            epoch: 2,
        },
    ];
    for (target, spec, byte) in [
        (&left_target, &partitions[0], 16),
        (&right_target, &partitions[1], 17),
    ] {
        let installed = client
            .command::<InstallPartition>(
                target,
                identity(byte),
                Json(PartitionInstall::Serving(spec.clone())),
            )
            .await
            .unwrap();
        assert_eq!(installed.output.0, InstallPartitionOutcome::Installed);
    }
    let route = TableRoute {
        table_id: table.id.clone(),
        epoch: 2,
        partitions: partitions.to_vec(),
    };
    assert_eq!(
        client
            .query::<ReadPartitionRoute>(
                &account,
                None,
                Json(PartitionLookupInput {
                    table_id: table.id.clone(),
                    hash: [0; 16],
                }),
            )
            .await
            .unwrap()
            .output
            .0,
        PartitionLookupOutcome::Unrouted
    );
    let old_account_key = key_in_range(&table.id, &table.key_schema, true, 500);
    client
        .command::<PutItem>(
            &account,
            identity(27),
            Json(PutItemInput {
                table_name: table.table_name.clone(),
                table_id: table.id.clone(),
                item: old_account_key.clone(),
                condition: None,
            }),
        )
        .await
        .unwrap();
    let unsafe_cutover = client
        .command::<ActivateTableRoute>(&account, identity(28), Json(route.clone()))
        .await;
    assert!(matches!(
        unsafe_cutover,
        Err(InvocationError::Rejected(result))
            if result.output.0 == ActivateTableRouteOutcome::TableNotEmpty
    ));
    client
        .command::<DeleteItem>(
            &account,
            identity(29),
            Json(DeleteItemInput {
                table_name: table.table_name.clone(),
                table_id: table.id.clone(),
                key: old_account_key,
                condition: None,
            }),
        )
        .await
        .unwrap();
    let mut gap = route.clone();
    gap.partitions[1].lower = Some([0x81, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let invalid = client
        .command::<ActivateTableRoute>(&account, identity(22), Json(gap))
        .await;
    assert!(matches!(
        invalid,
        Err(InvocationError::Rejected(result))
            if result.output.0 == ActivateTableRouteOutcome::InvalidRoute
    ));
    let active = client
        .command::<ActivateTableRoute>(&account, identity(23), Json(route.clone()))
        .await
        .unwrap();
    assert_eq!(active.output.0, ActivateTableRouteOutcome::Activated);
    let published = client
        .query::<ReadTableRoute>(&account, Some(active.receipt), Json(table.id.clone()))
        .await
        .unwrap();
    assert_eq!(published.output.0, Some(route.clone()));
    for (hash, partition_id, epoch) in [([0; 16], left_id, 1), (split, right_id, 2)] {
        assert_eq!(
            client
                .query::<ReadPartitionRoute>(
                    &account,
                    Some(active.receipt),
                    Json(PartitionLookupInput {
                        table_id: table.id.clone(),
                        hash,
                    }),
                )
                .await
                .unwrap()
                .output
                .0,
            PartitionLookupOutcome::Routed {
                partition_id,
                epoch,
            }
        );
    }
    let cross_left = key_in_range(&table.id, &table.key_schema, true, 600);
    let cross_right = key_in_range(&table.id, &table.key_schema, false, 600);
    let mut sides = [
        (
            left_target.clone(),
            1_u64,
            cross_left.clone(),
            0_u8,
            CoordinatorParticipantTarget::Data {
                table_id: table.id.clone(),
                partition_id: left_id,
                epoch: 1,
            },
        ),
        (
            right_target.clone(),
            2_u64,
            cross_right.clone(),
            1_u8,
            CoordinatorParticipantTarget::Data {
                table_id: table.id.clone(),
                partition_id: right_id,
                epoch: 2,
            },
        ),
    ];
    sides.sort_by_key(|side| *side.0.cell_id().as_bytes());
    let participants: Vec<_> = sides
        .iter()
        .map(|side| CoordinatorParticipant {
            target: side.4.clone(),
            operations: vec![IndexedTransactionOperation {
                index: side.3,
                operation: TransactionOperation::Put(PutItemInput {
                    table_name: table.table_name.clone(),
                    table_id: table.id.clone(),
                    item: side.2.clone(),
                    condition: None,
                }),
            }],
        })
        .collect();
    let mut reversed_participants = participants.clone();
    reversed_participants.reverse();
    let unordered_begin = client
        .command::<BeginCrossCellTransaction>(
            &coordinator_target,
            identity(125),
            Json(BeginCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                token: None,
                participants: reversed_participants,
            }),
        )
        .await;
    assert!(matches!(
        unordered_begin,
        Err(InvocationError::Rejected(result))
            if result.output.0 == BeginCrossCellTransactionOutcome::InvalidParticipants
    ));
    let begun_transaction = client
        .command::<BeginCrossCellTransaction>(
            &coordinator_target,
            identity(110),
            Json(BeginCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                token: None,
                participants: participants.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        begun_transaction.output.0,
        BeginCrossCellTransactionOutcome::Begun
    );
    let pending = client
        .query::<ReadPendingCrossCellTransactions>(
            &coordinator_target,
            None,
            Json(ReadPendingCrossCellTransactionsInput {
                after: None,
                limit: 1,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].transaction_id, transaction_id);
    assert_eq!(pending[0].state, PendingTransactionState::Begin);
    assert_eq!(pending[0].routing_key, transaction_id);
    assert!(
        client
            .query::<ReadPendingCrossCellTransactions>(
                &coordinator_target,
                None,
                Json(ReadPendingCrossCellTransactionsInput {
                    after: Some(pending[0].cursor.clone()),
                    limit: 1,
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .is_empty()
    );
    let early_decision = client
        .command::<DecideCrossCellTransaction>(
            &coordinator_target,
            identity(111),
            Json(DecideCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
                decision: CoordinatorDecision::Commit,
            }),
        )
        .await;
    assert!(matches!(
        early_decision,
        Err(InvocationError::Rejected(result))
            if result.output.0 == DecideCrossCellTransactionOutcome::NotPrepared
    ));
    for (position, side) in sides.iter().enumerate() {
        let prepared = client
            .command::<PreparePartitionTransaction>(
                &side.0,
                identity(112 + u8::try_from(position).unwrap()),
                Json(PreparePartitionTransactionInput {
                    table_id: table.id.clone(),
                    epoch: side.1,
                    transaction_id,
                    coordinator_cell: *coordinator_target.cell_id().as_bytes(),
                    coordinator_key: transaction_id.to_vec(),
                    operations: vec![participants[position].operations[0].operation.clone()],
                }),
            )
            .await
            .unwrap();
        assert_eq!(prepared.output.0, PrepareTransactionOutcome::Prepared);
        let recorded = client
            .command::<RecordParticipantPrepare>(
                &coordinator_target,
                identity(114 + u8::try_from(position).unwrap()),
                Json(CoordinatorPhaseInput {
                    account_id: "123456789012".into(),
                    transaction_id,
                    routing_key: transaction_id.to_vec(),
                    position: u8::try_from(position).unwrap(),
                    participant_cell: *side.0.cell_id().as_bytes(),
                    sequence: prepared.receipt.commit_sequence,
                }),
            )
            .await
            .unwrap();
        assert_eq!(recorded.output.0, CoordinatorPhaseOutcome::Recorded);
    }
    let decided = client
        .command::<DecideCrossCellTransaction>(
            &coordinator_target,
            identity(116),
            Json(DecideCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
                decision: CoordinatorDecision::Commit,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        decided.output.0,
        DecideCrossCellTransactionOutcome::Decided(CoordinatorDecision::Commit)
    );
    let opposing_decision = client
        .command::<DecideCrossCellTransaction>(
            &coordinator_target,
            identity(123),
            Json(DecideCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
                decision: CoordinatorDecision::Abort {
                    index: None,
                    reason: None,
                },
            }),
        )
        .await;
    assert!(matches!(
        opposing_decision,
        Err(InvocationError::Rejected(result))
            if result.output.0 == DecideCrossCellTransactionOutcome::DecisionConflict
    ));
    let repeated_begin = client
        .command::<BeginCrossCellTransaction>(
            &coordinator_target,
            identity(124),
            Json(BeginCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                token: None,
                participants: participants.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        repeated_begin.output.0,
        BeginCrossCellTransactionOutcome::Existing {
            transaction_id,
            decision: CoordinatorDecision::Commit,
        }
    );
    let pending_commit = client
        .query::<ReadPendingCrossCellTransactions>(
            &coordinator_target,
            None,
            Json(ReadPendingCrossCellTransactionsInput {
                after: None,
                limit: 1,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(pending_commit[0].state, PendingTransactionState::Commit);
    for (position, side) in sides.iter().enumerate().take(1) {
        let applied = client
            .command::<ResolvePartitionTransaction>(
                &side.0,
                identity(117 + u8::try_from(position).unwrap()),
                Json(ResolveTransactionInput {
                    transaction_id,
                    coordinator_cell: *coordinator_target.cell_id().as_bytes(),
                    commit: true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(applied.output.0, ResolveTransactionOutcome::Committed);
        let recorded = client
            .command::<RecordParticipantResolution>(
                &coordinator_target,
                identity(119 + u8::try_from(position).unwrap()),
                Json(CoordinatorPhaseInput {
                    account_id: "123456789012".into(),
                    transaction_id,
                    routing_key: transaction_id.to_vec(),
                    position: u8::try_from(position).unwrap(),
                    participant_cell: *side.0.cell_id().as_bytes(),
                    sequence: applied.receipt.commit_sequence,
                }),
            )
            .await
            .unwrap();
        assert_eq!(recorded.output.0, CoordinatorPhaseOutcome::Recorded);
    }
    // A committed transaction has only applied its first participant. The
    // direct query must still fence the second; the public read will resolve COMMIT.
    let visibility_key_info = storage
        .table_key_info("123456789012", "Books")
        .await
        .unwrap();
    assert_eq!(
        storage
            .get_item(&visibility_key_info, &sides[0].2)
            .await
            .unwrap(),
        Some(sides[0].2.clone())
    );
    let blocked = client
        .query::<PartitionGet>(
            &sides[1].0,
            None,
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: sides[1].1,
                key: sides[1].2.clone(),
            }),
        )
        .await
        .unwrap();
    assert!(matches!(blocked.output.0, PartitionGetOutcome::Conflict(_)));
    let unresolved = client
        .query::<ReadUnresolvedCoordinatorParticipants>(
            &coordinator_target,
            None,
            Json(ReadCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].position, 1);
    assert_eq!(
        storage
            .get_item(&visibility_key_info, &sides[1].2)
            .await
            .unwrap(),
        Some(sides[1].2.clone())
    );
    storage
        .finish_decided_cross_cell_transaction("123456789012", &transaction_id, transaction_id)
        .await
        .unwrap();
    storage
        .finish_decided_cross_cell_transaction("123456789012", &transaction_id, transaction_id)
        .await
        .unwrap();
    let coordinator_state = client
        .query::<ReadCrossCellTransaction>(
            &coordinator_target,
            None,
            Json(ReadCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
            }),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(coordinator_state.decision, CoordinatorDecision::Commit);
    assert_eq!(coordinator_state.prepared_count, 2);
    assert_eq!(coordinator_state.resolved_count, 2);
    assert!(
        client
            .query::<ReadPendingCrossCellTransactions>(
                &coordinator_target,
                None,
                Json(ReadPendingCrossCellTransactionsInput {
                    after: None,
                    limit: 1,
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .is_empty()
    );
    let cross_key_info = storage
        .table_key_info("123456789012", "Books")
        .await
        .unwrap();
    assert_eq!(
        storage
            .get_item(&cross_key_info, &cross_left)
            .await
            .unwrap(),
        Some(cross_left)
    );
    assert_eq!(
        storage
            .get_item(&cross_key_info, &cross_right)
            .await
            .unwrap(),
        Some(cross_right)
    );
    for (position, side) in sides.into_iter().enumerate() {
        client
            .command::<PartitionDelete>(
                &side.0,
                identity(121 + u8::try_from(position).unwrap()),
                Json(PartitionDeleteInput {
                    table_id: table.id.clone(),
                    epoch: side.1,
                    key: side.2,
                    condition: None,
                }),
            )
            .await
            .unwrap();
    }
    let boundary = [0x40, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let children = [
        PartitionSpec {
            table: table.clone(),
            partition_id: [3; 16],
            lower: None,
            upper: Some(boundary),
            epoch: 3,
        },
        PartitionSpec {
            table: table.clone(),
            partition_id: [4; 16],
            lower: Some(boundary),
            upper: Some(split),
            epoch: 3,
        },
    ];
    let plan = SplitPlan {
        source: partitions[0].clone(),
        children: children.clone(),
        expected_epoch: route.epoch,
    };
    let next_route = TableRoute {
        table_id: table.id.clone(),
        epoch: 3,
        partitions: vec![
            children[0].clone(),
            children[1].clone(),
            partitions[1].clone(),
        ],
    };
    let mut empty_child = plan.clone();
    empty_child.children[0].upper = Some([0; 16]);
    empty_child.children[1].lower = Some([0; 16]);
    let invalid = client
        .command::<BeginSplit>(&account, identity(40), Json(empty_child))
        .await;
    assert!(matches!(
        invalid,
        Err(InvocationError::Rejected(result))
            if result.output.0 == BeginSplitOutcome::InvalidPlan
    ));
    let begun = client
        .command::<BeginSplit>(&account, identity(41), Json(plan.clone()))
        .await
        .unwrap();
    assert_eq!(begun.output.0, BeginSplitOutcome::Planned);
    let replayed = client
        .command::<BeginSplit>(&account, identity(42), Json(plan.clone()))
        .await
        .unwrap();
    assert_eq!(replayed.output.0, BeginSplitOutcome::Planned);
    let mut conflict = plan.clone();
    let different_boundary = [0x50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    conflict.children[0].upper = Some(different_boundary);
    conflict.children[1].lower = Some(different_boundary);
    let competing = client
        .command::<BeginSplit>(&account, identity(43), Json(conflict))
        .await;
    assert!(matches!(
        competing,
        Err(InvocationError::Rejected(result))
            if result.output.0 == BeginSplitOutcome::Conflict
    ));
    let pending = client
        .query::<ReadSplitPlan>(&account, Some(begun.receipt), Json(table.id.clone()))
        .await
        .unwrap();
    assert_eq!(pending.output.0, Some(plan.clone()));
    let fenced_write = client
        .command::<PutItem>(
            &account,
            identity(30),
            Json(PutItemInput {
                table_name: table.table_name.clone(),
                table_id: table.id.clone(),
                item: key_in_range(&table.id, &table.key_schema, true, 700),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        fenced_write,
        Err(InvocationError::Rejected(result))
            if result.output.0 == ItemMutationOutcome::TableNotFound
    ));
    let left_key = key_in_range(&table.id, &table.key_schema, true, 0);
    let right_key = key_in_range(&table.id, &table.key_schema, false, 0);
    for (target, key, epoch, byte) in [
        (&left_target, &left_key, 1, 18),
        (&right_target, &right_key, 2, 19),
    ] {
        let written = client
            .command::<PartitionPut>(
                target,
                identity(byte),
                Json(PartitionPutInput {
                    table_id: table.id.clone(),
                    epoch,
                    item: key.clone(),
                    condition: None,
                }),
            )
            .await
            .unwrap();
        assert_eq!(written.output.0, PartitionPutOutcome::Applied(None));
        let read = client
            .query::<PartitionGet>(
                target,
                Some(written.receipt),
                Json(PartitionGetInput {
                    table_id: table.id.clone(),
                    epoch,
                    key: key.clone(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(read.output.0, PartitionGetOutcome::Found(Some(key.clone())));
        let page = client
            .query::<PartitionScan>(
                target,
                Some(written.receipt),
                Json(PartitionScanInput {
                    table_id: table.id.clone(),
                    epoch,
                    limit: Some(1),
                    exclusive_start_key: None,
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            page.output.0,
            PartitionScanOutcome::Page {
                items: vec![key.clone()],
                last_evaluated_key: None,
            }
        );
    }
    let second_left_key = key_in_range(&table.id, &table.key_schema, true, 100);
    client
        .command::<PartitionPut>(
            &left_target,
            identity(26),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 1,
                item: second_left_key.clone(),
                condition: None,
            }),
        )
        .await
        .unwrap();
    let first_page = client
        .query::<PartitionScan>(
            &left_target,
            None,
            Json(PartitionScanInput {
                table_id: table.id.clone(),
                epoch: 1,
                limit: Some(1),
                exclusive_start_key: None,
            }),
        )
        .await
        .unwrap();
    let PartitionScanOutcome::Page {
        mut items,
        last_evaluated_key: Some(cursor),
    } = first_page.output.0
    else {
        panic!("expected first partition page and continuation")
    };
    let second_page = client
        .query::<PartitionScan>(
            &left_target,
            None,
            Json(PartitionScanInput {
                table_id: table.id.clone(),
                epoch: 1,
                limit: Some(1),
                exclusive_start_key: Some(cursor),
            }),
        )
        .await
        .unwrap();
    let PartitionScanOutcome::Page {
        items: remaining,
        last_evaluated_key: None,
    } = second_page.output.0
    else {
        panic!("expected final partition page")
    };
    items.extend(remaining);
    assert_eq!(items.len(), 2);
    assert!(items.contains(&left_key));
    assert!(items.contains(&second_left_key));
    let wrong = client
        .command::<PartitionPut>(
            &right_target,
            identity(20),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 2,
                item: left_key.clone(),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        wrong,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionPutOutcome::WrongPartition
    ));
    let stale = client
        .query::<PartitionGet>(
            &left_target,
            None,
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 2,
                key: left_key.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(stale.output.0, PartitionGetOutcome::StaleRoute);
    let maps = ExpressionMaps::new(
        HashMap::new(),
        HashMap::from([("title".into(), AttributeValue::S("Updated".into()))]),
    );
    let actions = [UpdateAction::Set {
        path: vec![PathElement::Attribute("title".into())],
        value: Expr::Placeholder("title".into()),
    }];
    let key_info = storage
        .table_key_info("123456789012", "Books")
        .await
        .unwrap();
    let transaction = storage
        .transact_get_items(&[
            TransactGetOp {
                key_info: &key_info,
                key: &left_key,
            },
            TransactGetOp {
                key_info: &key_info,
                key: &second_left_key,
            },
        ])
        .await
        .unwrap();
    assert_eq!(
        transaction,
        vec![Some(left_key.clone()), Some(second_left_key.clone())]
    );
    let cross_cell = storage
        .transact_get_items(&[
            TransactGetOp {
                key_info: &key_info,
                key: &left_key,
            },
            TransactGetOp {
                key_info: &key_info,
                key: &right_key,
            },
        ])
        .await;
    assert_eq!(
        cross_cell.unwrap(),
        vec![Some(left_key.clone()), Some(right_key.clone())]
    );
    let tx_maps = ExpressionMaps::default();
    let rolled_back = key_in_range(&table.id, &table.key_schema, true, 300);
    let not_exists = Expr::Function {
        name: "attribute_not_exists".into(),
        args: vec![Expr::Path(vec![PathElement::Attribute("id".into())])],
    };
    let failed = storage
        .transact_write_items(
            &[
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &rolled_back,
                    condition: None,
                    maps: &tx_maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &left_key,
                    condition: Some(&not_exists),
                    maps: &tx_maps,
                    return_values_on_ccf: ReturnValuesOnConditionCheckFailure::AllOld,
                    stream: None,
                },
            ],
            None,
        )
        .await;
    assert!(matches!(
        failed,
        Err(StorageError::TransactionCanceled(reasons))
            if reasons[1].code == "ConditionalCheckFailed"
                && reasons[1].item == Some(left_key.clone())
    ));
    assert_eq!(
        storage.get_item(&key_info, &rolled_back).await.unwrap(),
        None
    );
    storage
        .transact_write_items(
            &[
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &left_key,
                    condition: None,
                    maps: &tx_maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &second_left_key,
                    condition: None,
                    maps: &tx_maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
            ],
            None,
        )
        .await
        .unwrap();
    let token = IdempotencyKey {
        account_id: "123456789012",
        token: "same-account-token",
        fingerprint: "left-write",
    };
    let left_write = [TransactWriteOp::Put {
        key_info: &key_info,
        item: &left_key,
        condition: None,
        maps: &tx_maps,
        return_values_on_ccf: Default::default(),
        stream: None,
    }];
    storage
        .transact_write_items(&left_write, Some(token))
        .await
        .unwrap();
    let replay = storage
        .transact_write_items(
            &left_write,
            Some(IdempotencyKey {
                account_id: "123456789012",
                token: "same-account-token",
                fingerprint: "left-write",
            }),
        )
        .await;
    assert!(matches!(replay, Err(StorageError::IdempotentReplay)));
    let right_new = key_in_range(&table.id, &table.key_schema, false, 600);
    let reused = storage
        .transact_write_items(
            &[TransactWriteOp::Put {
                key_info: &key_info,
                item: &right_new,
                condition: None,
                maps: &tx_maps,
                return_values_on_ccf: Default::default(),
                stream: None,
            }],
            Some(IdempotencyKey {
                account_id: "123456789012",
                token: "same-account-token",
                fingerprint: "right-write",
            }),
        )
        .await;
    assert!(matches!(reused, Err(StorageError::IdempotentMismatch)));
    assert_eq!(storage.get_item(&key_info, &right_new).await.unwrap(), None);
    let cross_write = storage
        .transact_write_items(
            &[
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &rolled_back,
                    condition: None,
                    maps: &tx_maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
                TransactWriteOp::Put {
                    key_info: &key_info,
                    item: &right_key,
                    condition: None,
                    maps: &tx_maps,
                    return_values_on_ccf: Default::default(),
                    stream: None,
                },
            ],
            None,
        )
        .await;
    cross_write.unwrap();
    assert_eq!(
        storage.get_item(&key_info, &rolled_back).await.unwrap(),
        Some(rolled_back.clone())
    );
    storage
        .delete_item(&key_info, &rolled_back, false, None, &tx_maps, None)
        .await
        .unwrap();
    let (first_range, continuation) = storage
        .scan(&key_info, Some(2), None, None, None, None)
        .await
        .unwrap();
    assert_eq!(first_range.len(), 2);
    assert!(first_range.contains(&left_key));
    assert!(first_range.contains(&second_left_key));
    let (second_range, end) = storage
        .scan(&key_info, Some(2), continuation.as_ref(), None, None, None)
        .await
        .unwrap();
    assert_eq!(second_range, vec![right_key.clone()]);
    assert_eq!(end, None);
    let routed_key = key_in_range(&table.id, &table.key_schema, false, 200);
    let previous = storage
        .put_item(
            &key_info,
            routed_key.clone(),
            true,
            None,
            &ExpressionMaps::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(previous, None);
    let not_exists = Expr::Function {
        name: "attribute_not_exists".into(),
        args: vec![Expr::Path(vec![PathElement::Attribute("id".into())])],
    };
    let conditional = storage
        .put_item(
            &key_info,
            routed_key.clone(),
            true,
            Some(&not_exists),
            &ExpressionMaps::default(),
            None,
        )
        .await;
    assert!(matches!(
        conditional,
        Err(StorageError::ConditionFailed(Some(old))) if old == routed_key
    ));
    let account_item = client
        .query::<GetItem>(
            &account,
            None,
            Json(GetItemInput {
                table_name: "Books".into(),
                table_id: table.id.clone(),
                key: routed_key.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(account_item.output.0, GetItemOutcome::TableNotFound);
    assert_eq!(
        storage.get_item(&key_info, &routed_key).await.unwrap(),
        Some(routed_key.clone())
    );
    let (previous, changed) = storage
        .update_item(
            &key_info,
            &routed_key,
            &actions,
            true,
            true,
            None,
            &maps,
            None,
        )
        .await
        .unwrap();
    let mut changed_item = routed_key.clone();
    changed_item.insert("title".into(), AttributeValue::S("Updated".into()));
    assert_eq!(
        (previous, changed),
        (Some(routed_key.clone()), Some(changed_item.clone()))
    );
    assert_eq!(
        storage
            .delete_item(
                &key_info,
                &routed_key,
                true,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap(),
        Some(changed_item)
    );
    assert_eq!(
        storage.get_item(&key_info, &routed_key).await.unwrap(),
        None
    );
    let updated = client
        .command::<PartitionUpdate>(
            &right_target,
            identity(24),
            Json(PartitionUpdateInput::from_expression(
                table.id.clone(),
                2,
                right_key.clone(),
                &actions,
                None,
                &maps,
            )),
        )
        .await
        .unwrap();
    let mut updated_item = right_key.clone();
    updated_item.insert("title".into(), AttributeValue::S("Updated".into()));
    assert_eq!(
        updated.output.0,
        PartitionUpdateOutcome::Applied {
            old: Some(right_key.clone()),
            new: updated_item.clone(),
        }
    );
    let deleted = client
        .command::<PartitionDelete>(
            &right_target,
            identity(25),
            Json(PartitionDeleteInput {
                table_id: table.id.clone(),
                epoch: 2,
                key: right_key.clone(),
                condition: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        deleted.output.0,
        PartitionDeleteOutcome::Applied(Some(updated_item))
    );
    let absent = client
        .query::<PartitionGet>(
            &right_target,
            Some(deleted.receipt),
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 2,
                key: right_key,
            }),
        )
        .await
        .unwrap();
    assert_eq!(absent.output.0, PartitionGetOutcome::Found(None));
    let export_input = || PartitionScanInput {
        table_id: table.id.clone(),
        epoch: 1,
        limit: Some(1),
        exclusive_start_key: None,
    };
    let premature_export = client
        .query::<PartitionExport>(&left_target, None, Json(export_input()))
        .await
        .unwrap();
    assert_eq!(premature_export.output.0, PartitionScanOutcome::NotSealed);
    let seal = PartitionSeal {
        table_id: table.id.clone(),
        source_partition_id: left_id,
        epoch: 1,
        next_epoch: next_route.epoch,
        source_lower: None,
        source_upper: Some(split),
        boundary,
        left_partition_id: next_route.partitions[0].partition_id,
        right_partition_id: next_route.partitions[1].partition_id,
    };
    let intent_item = key_in_range(&table.id, &table.key_schema, true, 800);
    let prepare_input = PreparePartitionTransactionInput {
        table_id: table.id.clone(),
        epoch: 1,
        transaction_id: [90; 16],
        coordinator_cell: *account.cell_id().as_bytes(),
        coordinator_key: [90; 16].to_vec(),
        operations: vec![TransactionOperation::Put(PutItemInput {
            table_name: table.table_name.clone(),
            table_id: table.id.clone(),
            item: intent_item.clone(),
            condition: None,
        })],
    };
    let prepared = client
        .command::<PreparePartitionTransaction>(
            &left_target,
            identity(90),
            Json(prepare_input.clone()),
        )
        .await
        .unwrap();
    assert_eq!(prepared.output.0, PrepareTransactionOutcome::Prepared);
    let prepared_state = client
        .query::<ReadPartitionTransaction>(
            &left_target,
            Some(prepared.receipt),
            Json(ReadTransactionInput {
                transaction_id: prepare_input.transaction_id,
                coordinator_cell: prepare_input.coordinator_cell,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        prepared_state.output.0,
        ParticipantTransactionState::Prepared
    );
    let hidden = client
        .query::<PartitionGet>(
            &left_target,
            Some(prepared.receipt),
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 1,
                key: intent_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(hidden.output.0, PartitionGetOutcome::Conflict(conflict) if conflict.transaction.transaction_id == prepare_input.transaction_id && conflict.coordinator_key == prepare_input.coordinator_key)
    );
    let conflicting_write = client
        .command::<PartitionPut>(
            &left_target,
            identity(91),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 1,
                item: intent_item.clone(),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        conflicting_write,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionPutOutcome::TransactionConflict
    ));
    let conflicting_transaction = client
        .command::<PartitionTransactWrite>(
            &left_target,
            identity(100),
            Json(PartitionTransactWriteInput {
                table_id: table.id.clone(),
                epoch: 1,
                operations: prepare_input.operations.clone(),
            }),
        )
        .await;
    assert!(matches!(
        conflicting_transaction,
        Err(InvocationError::Rejected(result))
            if result.output.0 == (PartitionTransactWriteOutcome::Rejected { index: 0, reason: beyonddb::TransactionFailure::Conflict })
    ));
    let premature_seal = client
        .command::<SealPartition>(&left_target, identity(92), Json(seal.clone()))
        .await;
    assert!(matches!(
        premature_seal,
        Err(InvocationError::Rejected(result))
            if result.output.0 == SealPartitionOutcome::InFlightTransaction
    ));
    let abort = client
        .command::<ResolvePartitionTransaction>(
            &left_target,
            identity(93),
            Json(ResolveTransactionInput {
                transaction_id: prepare_input.transaction_id,
                coordinator_cell: prepare_input.coordinator_cell,
                commit: false,
            }),
        )
        .await
        .unwrap();
    assert_eq!(abort.output.0, ResolveTransactionOutcome::Aborted);
    let retry_after_abort = client
        .command::<PreparePartitionTransaction>(&left_target, identity(94), Json(prepare_input))
        .await;
    assert!(matches!(
        retry_after_abort,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PrepareTransactionOutcome::Aborted
    ));
    let late_prepare = PreparePartitionTransactionInput {
        table_id: table.id.clone(),
        epoch: 1,
        transaction_id: [98; 16],
        coordinator_cell: *account.cell_id().as_bytes(),
        coordinator_key: [98; 16].to_vec(),
        operations: vec![TransactionOperation::Put(PutItemInput {
            table_name: table.table_name.clone(),
            table_id: table.id.clone(),
            item: intent_item.clone(),
            condition: None,
        })],
    };
    client
        .command::<ResolvePartitionTransaction>(
            &left_target,
            identity(98),
            Json(ResolveTransactionInput {
                transaction_id: late_prepare.transaction_id,
                coordinator_cell: late_prepare.coordinator_cell,
                commit: false,
            }),
        )
        .await
        .unwrap();
    let fenced_late_prepare = client
        .command::<PreparePartitionTransaction>(&left_target, identity(99), Json(late_prepare))
        .await;
    assert!(matches!(
        fenced_late_prepare,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PrepareTransactionOutcome::Aborted
    ));
    let committed_input = PreparePartitionTransactionInput {
        table_id: table.id.clone(),
        epoch: 1,
        transaction_id: [95; 16],
        coordinator_cell: *account.cell_id().as_bytes(),
        coordinator_key: [95; 16].to_vec(),
        operations: vec![TransactionOperation::Put(PutItemInput {
            table_name: table.table_name.clone(),
            table_id: table.id.clone(),
            item: intent_item.clone(),
            condition: None,
        })],
    };
    client
        .command::<PreparePartitionTransaction>(
            &left_target,
            identity(95),
            Json(committed_input.clone()),
        )
        .await
        .unwrap();
    let commit = client
        .command::<ResolvePartitionTransaction>(
            &left_target,
            identity(96),
            Json(ResolveTransactionInput {
                transaction_id: committed_input.transaction_id,
                coordinator_cell: committed_input.coordinator_cell,
                commit: true,
            }),
        )
        .await
        .unwrap();
    assert_eq!(commit.output.0, ResolveTransactionOutcome::Committed);
    let visible = client
        .query::<PartitionGet>(
            &left_target,
            Some(commit.receipt),
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 1,
                key: intent_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        visible.output.0,
        PartitionGetOutcome::Found(Some(intent_item.clone()))
    );
    client
        .command::<PartitionDelete>(
            &left_target,
            identity(97),
            Json(PartitionDeleteInput {
                table_id: table.id.clone(),
                epoch: 1,
                key: intent_item,
                condition: None,
            }),
        )
        .await
        .unwrap();
    let sealed = client
        .command::<SealPartition>(&left_target, identity(44), Json(seal.clone()))
        .await
        .unwrap();
    assert_eq!(sealed.output.0, SealPartitionOutcome::Sealed);
    let source_status = client
        .query::<ReadPartitionState>(&left_target, Some(sealed.receipt), Json(()))
        .await
        .unwrap();
    assert_eq!(
        source_status.output.0.unwrap().state,
        PartitionState::Sealed(seal.clone())
    );
    let replayed = client
        .command::<SealPartition>(&left_target, identity(45), Json(seal.clone()))
        .await
        .unwrap();
    assert_eq!(replayed.output.0, SealPartitionOutcome::Sealed);
    let mut different_seal = seal.clone();
    different_seal.boundary[0] = 0x50;
    let conflict = client
        .command::<SealPartition>(&left_target, identity(46), Json(different_seal))
        .await;
    assert!(matches!(
        conflict,
        Err(InvocationError::Rejected(result))
            if result.output.0 == SealPartitionOutcome::Conflict
    ));
    let fenced_write = client
        .command::<PartitionPut>(
            &left_target,
            identity(47),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 1,
                item: left_key.clone(),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        fenced_write,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionPutOutcome::Sealed
    ));
    let fenced_delete = client
        .command::<PartitionDelete>(
            &left_target,
            identity(48),
            Json(PartitionDeleteInput {
                table_id: table.id.clone(),
                epoch: 1,
                key: left_key.clone(),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        fenced_delete,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionDeleteOutcome::Sealed
    ));
    let fenced_update = client
        .command::<PartitionUpdate>(
            &left_target,
            identity(49),
            Json(PartitionUpdateInput::from_expression(
                table.id.clone(),
                1,
                left_key.clone(),
                &actions,
                None,
                &maps,
            )),
        )
        .await;
    assert!(matches!(
        fenced_update,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionUpdateOutcome::Sealed
    ));
    let fenced_read = client
        .query::<PartitionGet>(
            &left_target,
            Some(sealed.receipt),
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 1,
                key: left_key.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(fenced_read.output.0, PartitionGetOutcome::Sealed);
    let fenced_scan = client
        .query::<PartitionScan>(&left_target, None, Json(export_input()))
        .await
        .unwrap();
    assert_eq!(fenced_scan.output.0, PartitionScanOutcome::Sealed);
    assert!(matches!(
        storage.get_item(&key_info, &left_key).await,
        Err(StorageError::Transient(_))
    ));
    let mut exported = Vec::new();
    let mut cursor = None;
    loop {
        let mut input = export_input();
        input.exclusive_start_key = cursor;
        let response = client
            .query::<PartitionExport>(&left_target, None, Json(input))
            .await
            .unwrap();
        let PartitionScanOutcome::Page {
            items,
            last_evaluated_key,
        } = response.output.0
        else {
            panic!("expected a sealed export page")
        };
        exported.extend(items);
        let Some(next) = last_evaluated_key else {
            break;
        };
        cursor = Some(next);
    }
    assert_eq!(exported.len(), 2);
    assert!(exported.contains(&left_key));
    assert!(exported.contains(&second_left_key));
    let child_targets = [
        data_target("123456789012", &table.id, &[3; 16]).unwrap(),
        data_target("123456789012", &table.id, &[4; 16]).unwrap(),
    ];
    let child_handles = [
        bootstrap
            .cell(
                &child_targets[0],
                "beyonddb-data",
                50,
                &directory.path().join("child-left.sqlite"),
                initialize_partition,
            )
            .await,
        bootstrap
            .cell(
                &child_targets[1],
                "beyonddb-data",
                51,
                &directory.path().join("child-right.sqlite"),
                initialize_partition,
            )
            .await,
    ];
    let child_client = host
        .application_handle::<Beyonddb>(
            CellClient::local_many(Arc::clone(&registry), child_handles.iter().cloned()).unwrap(),
            account.tenant(),
            account.application(),
        )
        .unwrap();
    for (index, child) in child_targets.iter().enumerate() {
        let installed = child_client
            .command::<InstallPartition>(
                child,
                identity(52 + u8::try_from(index).unwrap()),
                Json(PartitionInstall::Importing {
                    spec: next_route.partitions[index].clone(),
                    source: seal.clone(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(installed.output.0, InstallPartitionOutcome::Installed);
    }
    let first_item = exported[0].clone();
    let first_hash = data_key_hash(&table.id, &first_item, &table.key_schema).unwrap();
    let first_child = usize::from(first_hash >= boundary);
    let premature_read = child_client
        .query::<PartitionGet>(
            &child_targets[first_child],
            None,
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 3,
                key: first_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(premature_read.output.0, PartitionGetOutcome::NotReady);
    let premature_write = child_client
        .command::<PartitionPut>(
            &child_targets[first_child],
            identity(65),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: first_item.clone(),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        premature_write,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionPutOutcome::NotReady
    ));
    let premature_scan = child_client
        .query::<PartitionScan>(
            &child_targets[first_child],
            None,
            Json(PartitionScanInput {
                table_id: table.id.clone(),
                epoch: 3,
                limit: Some(1),
                exclusive_start_key: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(premature_scan.output.0, PartitionScanOutcome::NotReady);
    let wrong_child = child_client
        .command::<ImportPartitionItem>(
            &child_targets[1 - first_child],
            identity(54),
            Json(PartitionImportInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: first_item.clone(),
            }),
        )
        .await;
    assert!(matches!(
        wrong_child,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionImportOutcome::WrongPartition
    ));
    let mut expected = [ImportSummary::default(), ImportSummary::default()];
    for item in &exported {
        let hash = data_key_hash(&table.id, item, &table.key_schema).unwrap();
        let index = usize::from(hash >= boundary);
        expected[index].include(item, &table.key_schema).unwrap();
    }
    let incomplete = child_client
        .command::<ActivateImportedPartition>(
            &child_targets[first_child],
            identity(55),
            Json(ActivateImportedPartitionInput {
                source: seal.clone(),
                expected: expected[first_child].clone(),
            }),
        )
        .await;
    assert!(matches!(
        incomplete,
        Err(InvocationError::Rejected(result))
            if result.output.0 == ActivateImportedPartitionOutcome::Incomplete
    ));
    let copied = child_client
        .command::<ImportPartitionItem>(
            &child_targets[first_child],
            identity(56),
            Json(PartitionImportInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: first_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(copied.output.0, PartitionImportOutcome::Imported);
    let duplicate = child_client
        .command::<ImportPartitionItem>(
            &child_targets[first_child],
            identity(58),
            Json(PartitionImportInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: first_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(duplicate.output.0, PartitionImportOutcome::Imported);
    let mut changed_item = first_item.clone();
    changed_item.insert("title".into(), AttributeValue::S("newer".into()));
    let conflict = child_client
        .command::<ImportPartitionItem>(
            &child_targets[first_child],
            identity(59),
            Json(PartitionImportInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: changed_item.clone(),
            }),
        )
        .await;
    assert!(matches!(
        conflict,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionImportOutcome::Conflict
    ));
    let early_write = child_client
        .command::<PartitionPut>(
            &child_targets[first_child],
            identity(67),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: changed_item.clone(),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        early_write,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionPutOutcome::NotReady
    ));
    let mut wrong_plan = plan.clone();
    wrong_plan.source.partition_id = [9; 16];
    let wrong_commit = client
        .command::<CommitSplit>(&account, identity(64), Json(wrong_plan))
        .await;
    assert!(matches!(
        wrong_commit,
        Err(InvocationError::Rejected(result))
            if result.output.0 == CommitSplitOutcome::PlanMismatch
    ));
    let controller_client = CellClient::local_many(
        Arc::clone(&registry),
        [
            account_handle.clone(),
            left_handle.clone(),
            right_handle.clone(),
        ]
        .into_iter()
        .chain(child_handles.iter().cloned()),
    )
    .unwrap();
    let split_provisioner = CellInitialPartitionProvisioner::new(
        host.runtime(),
        Arc::clone(&application),
        layout.clone(),
        session,
        "https://beyonddb-partition.internal:8081".into(),
        directory.path().join("split-host"),
    )
    .unwrap();
    split_provisioner
        .resume_split("123456789012", account_handle.clone(), &plan)
        .await
        .unwrap();
    CellSplitController::new(controller_client.clone())
        .resume("123456789012", &plan)
        .await
        .unwrap();
    let replayed = client
        .command::<CommitSplit>(&account, identity(66), Json(plan.clone()))
        .await
        .unwrap();
    assert_eq!(replayed.output.0, CommitSplitOutcome::Committed);
    let current_route = client
        .query::<ReadTableRoute>(&account, Some(replayed.receipt), Json(table.id.clone()))
        .await
        .unwrap();
    assert_eq!(current_route.output.0, Some(next_route.clone()));
    for (hash, partition) in [
        ([0; 16], &next_route.partitions[0]),
        (boundary, &next_route.partitions[1]),
        (split, &next_route.partitions[2]),
    ] {
        assert_eq!(
            client
                .query::<ReadPartitionRoute>(
                    &account,
                    Some(replayed.receipt),
                    Json(PartitionLookupInput {
                        table_id: table.id.clone(),
                        hash,
                    }),
                )
                .await
                .unwrap()
                .output
                .0,
            PartitionLookupOutcome::Routed {
                partition_id: partition.partition_id,
                epoch: partition.epoch,
            }
        );
    }
    let opened = child_client
        .query::<ReadPartitionState>(&child_targets[first_child], None, Json(()))
        .await
        .unwrap();
    assert_eq!(
        opened.output.0.unwrap().state,
        PartitionState::Opened {
            source: seal.clone(),
            summary: expected[first_child].clone(),
        }
    );
    let published_storage = CellStorage::new(
        CellClient::local_many(
            Arc::clone(&registry),
            [account_handle.clone(), right_handle.clone()]
                .into_iter()
                .chain(child_handles.iter().cloned()),
        )
        .unwrap(),
        "us-east-1",
    );
    assert_eq!(
        published_storage
            .get_item(&key_info, &left_key)
            .await
            .unwrap(),
        Some(left_key.clone())
    );
    assert_eq!(
        published_storage
            .get_item(&key_info, &second_left_key)
            .await
            .unwrap(),
        Some(second_left_key.clone())
    );
    let right_after_split = key_in_range(&table.id, &table.key_schema, false, 300);
    assert_eq!(
        published_storage
            .put_item(
                &key_info,
                right_after_split.clone(),
                true,
                None,
                &ExpressionMaps::default(),
                None,
            )
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        published_storage
            .get_item(&key_info, &right_after_split)
            .await
            .unwrap(),
        Some(right_after_split)
    );
    let newer = child_client
        .command::<PartitionPut>(
            &child_targets[first_child],
            identity(62),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: changed_item.clone(),
                condition: None,
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        newer.output.0,
        PartitionPutOutcome::Applied(Some(first_item.clone()))
    );
    let delayed_copy = child_client
        .command::<ImportPartitionItem>(
            &child_targets[first_child],
            identity(63),
            Json(PartitionImportInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: first_item.clone(),
            }),
        )
        .await;
    assert!(matches!(
        delayed_copy,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionImportOutcome::NotImporting
    ));
    let visible = child_client
        .query::<PartitionGet>(
            &child_targets[first_child],
            None,
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 3,
                key: first_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        visible.output.0,
        PartitionGetOutcome::Found(Some(changed_item.clone()))
    );
    let recovery_prepare = child_client
        .command::<PreparePartitionTransaction>(
            &child_targets[first_child],
            identity(101),
            Json(PreparePartitionTransactionInput {
                table_id: table.id.clone(),
                epoch: 3,
                transaction_id: [101; 16],
                coordinator_cell: *account.cell_id().as_bytes(),
                coordinator_key: [101; 16].to_vec(),
                operations: vec![TransactionOperation::Put(PutItemInput {
                    table_name: table.table_name.clone(),
                    table_id: table.id.clone(),
                    item: changed_item.clone(),
                    condition: None,
                })],
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        recovery_prepare.output.0,
        PrepareTransactionOutcome::Prepared
    );
    let mut recovery_participant = participants
        .iter()
        .find(|participant| {
            matches!(
                &participant.target,
                CoordinatorParticipantTarget::Data { partition_id, .. } if *partition_id == left_id
            )
        })
        .unwrap()
        .clone();
    recovery_participant.operations[0].index = 0;
    let pending_id = (0..=u16::MAX)
        .map(|suffix| {
            let mut id = transaction_id;
            id[14..].copy_from_slice(&suffix.to_be_bytes());
            id
        })
        .find(|id| {
            *id != transaction_id
                && beyonddb::coordinator_target("123456789012", id).unwrap() == coordinator_target
        })
        .unwrap();
    client
        .command::<BeginCrossCellTransaction>(
            &coordinator_target,
            identity(126),
            Json(BeginCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id: pending_id,
                token: None,
                participants: vec![recovery_participant.clone()],
            }),
        )
        .await
        .unwrap();
    let token_value = (0..=u16::MAX)
        .map(|suffix| format!("recovery-{suffix}"))
        .find(|token| {
            beyonddb::coordinator_target("123456789012", token.as_bytes()).unwrap()
                == coordinator_target
        })
        .unwrap();
    let token_id = [127; 16];
    client
        .command::<BeginCrossCellTransaction>(
            &coordinator_target,
            identity(127),
            Json(BeginCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id: token_id,
                token: Some(TransactionToken {
                    account_id: "123456789012".into(),
                    token: token_value.clone(),
                    fingerprint: "recovery-request".into(),
                }),
                participants: vec![recovery_participant.clone()],
            }),
        )
        .await
        .unwrap();
    let abort_id = (0..=u16::MAX)
        .map(|suffix| {
            let mut id = transaction_id;
            id[14..].copy_from_slice(&suffix.to_be_bytes());
            id
        })
        .find(|id| {
            *id != transaction_id
                && *id != pending_id
                && beyonddb::coordinator_target("123456789012", id).unwrap() == coordinator_target
        })
        .unwrap();
    client
        .command::<BeginCrossCellTransaction>(
            &coordinator_target,
            identity(128),
            Json(BeginCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id: abort_id,
                token: None,
                participants: vec![recovery_participant],
            }),
        )
        .await
        .unwrap();
    for handle in child_handles {
        handle.drain().await.unwrap();
    }
    coordinator_handle.drain().await.unwrap();
    left_handle.drain().await.unwrap();
    right_handle.drain().await.unwrap();
    account_handle.drain().await.unwrap();
    host.shutdown().await.unwrap();

    let next_session = SessionId::from_bytes([21; 16]);
    let restored_host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 32).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(next_session)
        .build_unleased_for_maintenance()
        .unwrap();
    let restored_provisioner = CellInitialPartitionProvisioner::new(
        restored_host.runtime(),
        Arc::clone(&application),
        layout.clone(),
        next_session,
        "https://restored-coordinator.internal:8081".into(),
        directory.path().join("restored-coordinator"),
    )
    .unwrap();
    let account_proof = CellCatalog::new(layout.clone(), account.tenant())
        .lookup(account.cell_id())
        .await
        .unwrap()
        .unwrap();
    let account_authority = CellAuthority::new(layout.clone());
    let account_idle = account_authority
        .load(account.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored_account = restored_host
        .runtime()
        .acquire_idle_restored(
            account_proof,
            CellReplica::new(
                layout.clone(),
                *account.cell_id().as_bytes(),
                *IncarnationId::from_bytes([12; 16]).as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            account_authority,
            account_idle,
            directory.path().join("restored-account.sqlite"),
            Owner {
                session: next_session,
                endpoint: "https://restored-account.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let recovery_nodes = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([201; 32]),
        Digest::from_bytes([202; 32]),
        application.registry().release_digest(),
    );
    // Inspect unfinished records before invoking the full startup resolver below.
    restored_provisioner
        .recover_owned_coordinator("123456789012", &transaction_id, &recovery_nodes)
        .await
        .unwrap();
    let registered = CellClient::local(application.registry(), restored_account.clone())
        .query::<ListCoordinatorShards>(
            &account,
            None,
            Json(ListCoordinatorShardsInput {
                account_id: "123456789012".into(),
                after: None,
                limit: 100,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert!(registered.contains(&u32::from_be_bytes(
        coordinator_target.partition().try_into().unwrap()
    )));
    let coordinator_proof = CellCatalog::new(layout.clone(), account.tenant())
        .lookup(coordinator_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let coordinator_control = CellAuthority::new(layout.clone())
        .load(coordinator_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored_coordinator = restored_host
        .runtime()
        .local_handle(coordinator_proof, &coordinator_control)
        .await
        .unwrap()
        .unwrap();
    assert!(
        next_route
            .partitions
            .iter()
            .all(|partition| partition.partition_id != left_id)
    );
    restored_provisioner
        .recover_transaction_participants(
            &coordinator_target,
            &CellClient::local(application.registry(), restored_coordinator.clone()),
            &recovery_nodes,
        )
        .await
        .unwrap();
    let source_proof = CellCatalog::new(layout.clone(), left_target.tenant())
        .lookup(left_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let source_control = CellAuthority::new(layout.clone())
        .load(left_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored_data = restored_host
        .runtime()
        .local_handle(source_proof, &source_control)
        .await
        .unwrap()
        .unwrap();
    let restored_child_target = &child_targets[first_child];
    let child_proof = CellCatalog::new(layout.clone(), restored_child_target.tenant())
        .lookup(restored_child_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let child_authority = CellAuthority::new(layout.clone());
    let child_idle = child_authority
        .load(restored_child_target.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored_child = restored_host
        .runtime()
        .acquire_idle_restored(
            child_proof,
            CellReplica::new(
                layout,
                *restored_child_target.cell_id().as_bytes(),
                *IncarnationId::from_bytes([50 + u8::try_from(first_child).unwrap(); 16])
                    .as_bytes(),
                Limits::default(),
            )
            .unwrap(),
            child_authority,
            child_idle,
            directory.path().join("restored-child.sqlite"),
            Owner {
                session: next_session,
                endpoint: "https://restored-child.internal:8081".into(),
            },
        )
        .await
        .unwrap();
    let restored_cell_client = CellClient::local_many(
        application.registry(),
        [
            restored_data,
            restored_account,
            restored_child,
            restored_coordinator,
        ],
    )
    .unwrap();
    let restored_storage = CellStorage::new(restored_cell_client.clone(), "us-east-1");
    let restored_client = restored_host
        .application_handle::<Beyonddb>(
            restored_cell_client,
            left_target.tenant(),
            left_target.application(),
        )
        .unwrap();
    let restored_decision = restored_client
        .query::<ReadCrossCellTransaction>(
            &coordinator_target,
            None,
            Json(ReadCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
            }),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(restored_decision.decision, CoordinatorDecision::Commit);
    assert_eq!(restored_decision.resolved_count, 2);
    let restored_pending = restored_client
        .query::<ReadPendingCrossCellTransactions>(
            &coordinator_target,
            None,
            Json(ReadPendingCrossCellTransactionsInput {
                after: None,
                limit: 1,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(restored_pending.len(), 1);
    let next_pending = restored_client
        .query::<ReadPendingCrossCellTransactions>(
            &coordinator_target,
            None,
            Json(ReadPendingCrossCellTransactionsInput {
                after: Some(restored_pending[0].cursor.clone()),
                limit: 1,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(next_pending.len(), 1);
    let third_pending = restored_client
        .query::<ReadPendingCrossCellTransactions>(
            &coordinator_target,
            None,
            Json(ReadPendingCrossCellTransactionsInput {
                after: Some(next_pending[0].cursor.clone()),
                limit: 1,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(third_pending.len(), 1);
    let mut discovered = [
        restored_pending[0].clone(),
        next_pending[0].clone(),
        third_pending[0].clone(),
    ];
    discovered.sort_by_key(|entry| entry.transaction_id);
    assert_eq!(discovered[0].transaction_id, pending_id);
    assert_eq!(discovered[0].state, PendingTransactionState::Begin);
    assert_eq!(discovered[1].transaction_id, abort_id);
    assert_eq!(discovered[2].transaction_id, token_id);
    assert_eq!(discovered[2].routing_key, token_value.as_bytes());
    let restored_token = TransactionToken {
        account_id: "123456789012".into(),
        token: token_value.clone(),
        fingerprint: "recovery-request".into(),
    };
    assert_eq!(
        restored_client
            .query::<beyonddb::ReadCoordinatorToken>(
                &coordinator_target,
                None,
                Json(restored_token.clone()),
            )
            .await
            .unwrap()
            .output
            .0,
        beyonddb::ReadCoordinatorTokenOutcome::Found {
            transaction_id: token_id,
            decision: CoordinatorDecision::Begin
        }
    );
    assert!(matches!(
        restored_storage
            .finish_decided_cross_cell_transaction("123456789012", &pending_id, pending_id)
            .await,
        Err(StorageError::Transient(_))
    ));
    assert!(
        restored_client
            .query::<ReadPendingCrossCellTransactions>(
                &coordinator_target,
                None,
                Json(ReadPendingCrossCellTransactionsInput {
                    after: Some(third_pending[0].cursor.clone()),
                    limit: 1,
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .is_empty()
    );
    restored_client
        .command::<DecideCrossCellTransaction>(
            &coordinator_target,
            identity(129),
            Json(DecideCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id: abort_id,
                routing_key: abort_id.to_vec(),
                decision: CoordinatorDecision::Abort {
                    index: None,
                    reason: None,
                },
            }),
        )
        .await
        .unwrap();
    restored_storage
        .finish_decided_cross_cell_transaction("123456789012", &abort_id, abort_id)
        .await
        .unwrap();
    let aborted = restored_client
        .query::<ReadPartitionTransaction>(
            &left_target,
            None,
            Json(ReadTransactionInput {
                transaction_id: abort_id,
                coordinator_cell: *coordinator_target.cell_id().as_bytes(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(aborted.output.0, ParticipantTransactionState::Aborted);
    let abort_status = restored_client
        .query::<ReadCrossCellTransaction>(
            &coordinator_target,
            None,
            Json(ReadCrossCellTransactionInput {
                account_id: "123456789012".into(),
                transaction_id: abort_id,
                routing_key: abort_id.to_vec(),
            }),
        )
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(abort_status.resolved_count, 1);
    restored_storage
        .recover_fenced_coordinator(&coordinator_target)
        .await
        .unwrap();
    assert!(
        restored_client
            .query::<ReadPendingCrossCellTransactions>(
                &coordinator_target,
                None,
                Json(ReadPendingCrossCellTransactionsInput {
                    after: None,
                    limit: 1,
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .is_empty()
    );
    assert_eq!(
        restored_client
            .query::<beyonddb::ReadCoordinatorToken>(
                &coordinator_target,
                None,
                Json(restored_token),
            )
            .await
            .unwrap()
            .output
            .0,
        beyonddb::ReadCoordinatorTokenOutcome::Missing
    );
    let restored_participant = restored_client
        .query::<ReadCoordinatorParticipant>(
            &coordinator_target,
            None,
            Json(ReadCoordinatorParticipantInput {
                account_id: "123456789012".into(),
                transaction_id,
                routing_key: transaction_id.to_vec(),
                position: 0,
            }),
        )
        .await
        .unwrap();
    assert_eq!(restored_participant.output.0, Some(participants[0].clone()));
    let recovered_intent = restored_client
        .query::<ReadPartitionTransaction>(
            restored_child_target,
            None,
            Json(ReadTransactionInput {
                transaction_id: [101; 16],
                coordinator_cell: *account.cell_id().as_bytes(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered_intent.output.0,
        ParticipantTransactionState::Prepared
    );
    let recovered_read = restored_client
        .query::<PartitionGet>(
            restored_child_target,
            None,
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 3,
                key: extenddb_core::types::extract_key(&changed_item, &table.key_schema),
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(recovered_read.output.0, PartitionGetOutcome::Conflict(conflict) if conflict.transaction.transaction_id == [101; 16] && conflict.coordinator_key == vec![101; 16])
    );
    let recovered_scan = restored_client
        .query::<PartitionScan>(
            restored_child_target,
            None,
            Json(PartitionScanInput {
                table_id: table.id.clone(),
                epoch: 3,
                limit: None,
                exclusive_start_key: None,
            }),
        )
        .await
        .unwrap();
    assert!(
        matches!(recovered_scan.output.0, PartitionScanOutcome::Conflict(conflict) if conflict.transaction.transaction_id == [101; 16] && conflict.coordinator_key == vec![101; 16])
    );
    let recovered_conflict = restored_client
        .command::<PartitionPut>(
            restored_child_target,
            identity(102),
            Json(PartitionPutInput {
                table_id: table.id.clone(),
                epoch: 3,
                item: changed_item.clone(),
                condition: None,
            }),
        )
        .await;
    assert!(matches!(
        recovered_conflict,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionPutOutcome::TransactionConflict
    ));
    let recovered_abort = restored_client
        .command::<ResolvePartitionTransaction>(
            restored_child_target,
            identity(103),
            Json(ResolveTransactionInput {
                transaction_id: [101; 16],
                coordinator_cell: *account.cell_id().as_bytes(),
                commit: false,
            }),
        )
        .await
        .unwrap();
    assert_eq!(recovered_abort.output.0, ResolveTransactionOutcome::Aborted);
    let restored_transaction = restored_client
        .query::<ReadPartitionTransaction>(
            &left_target,
            None,
            Json(ReadTransactionInput {
                transaction_id: [95; 16],
                coordinator_cell: *account.cell_id().as_bytes(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        restored_transaction.output.0,
        ParticipantTransactionState::Committed
    );
    let persisted = restored_client
        .query::<PartitionGet>(
            &left_target,
            None,
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 1,
                key: left_key.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(persisted.output.0, PartitionGetOutcome::Sealed);
    let exported_after_restart = restored_client
        .query::<PartitionExport>(
            &left_target,
            None,
            Json(PartitionScanInput {
                table_id: table.id.clone(),
                epoch: 1,
                limit: Some(10),
                exclusive_start_key: None,
            }),
        )
        .await
        .unwrap();
    let PartitionScanOutcome::Page {
        items: recovered_items,
        last_evaluated_key: None,
    } = exported_after_restart.output.0
    else {
        panic!("expected sealed source export after restart")
    };
    assert_eq!(recovered_items.len(), 2);
    assert!(recovered_items.contains(&left_key));
    assert!(recovered_items.contains(&second_left_key));
    let persisted_route = restored_client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap();
    assert_eq!(persisted_route.output.0, Some(next_route.clone()));
    assert_eq!(
        restored_client
            .query::<ReadPartitionRoute>(
                &account,
                None,
                Json(PartitionLookupInput {
                    table_id: table.id.clone(),
                    hash: boundary,
                }),
            )
            .await
            .unwrap()
            .output
            .0,
        PartitionLookupOutcome::Routed {
            partition_id: next_route.partitions[1].partition_id,
            epoch: next_route.partitions[1].epoch,
        }
    );
    let persisted_plan = restored_client
        .query::<ReadSplitPlan>(&account, None, Json(table.id.clone()))
        .await
        .unwrap();
    assert_eq!(persisted_plan.output.0, None);
    let restored_status = restored_client
        .query::<ReadPartitionState>(restored_child_target, None, Json(()))
        .await
        .unwrap();
    assert_eq!(
        restored_status.output.0.unwrap().state,
        PartitionState::Opened {
            source: seal.clone(),
            summary: expected[first_child].clone(),
        }
    );
    let recovered_child = restored_client
        .query::<PartitionGet>(
            restored_child_target,
            None,
            Json(PartitionGetInput {
                table_id: table.id.clone(),
                epoch: 3,
                key: first_item.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered_child.output.0,
        PartitionGetOutcome::Found(Some(changed_item))
    );
    let stale_import = restored_client
        .command::<ImportPartitionItem>(
            restored_child_target,
            identity(64),
            Json(PartitionImportInput {
                table_id: table.id,
                epoch: 3,
                item: first_item,
            }),
        )
        .await;
    assert!(matches!(
        stale_import,
        Err(InvocationError::Rejected(result))
            if result.output.0 == PartitionImportOutcome::NotImporting
    ));
    assert!(matches!(
        restored_client
            .command::<DeleteTable>(&account, identity(67), Json(table.table_name.clone()))
            .await
            .unwrap()
            .output
            .0,
        DeleteTableOutcome::Deleted(_)
    ));
    assert_eq!(
        restored_client
            .query::<ReadPartitionRoute>(
                &account,
                None,
                Json(PartitionLookupInput {
                    table_id: next_route.table_id,
                    hash: boundary,
                }),
            )
            .await
            .unwrap()
            .output
            .0,
        PartitionLookupOutcome::Unrouted
    );
    let second_key = (0..u16::MAX)
        .map(|suffix| format!("second-shard-{suffix}"))
        .find(|key| {
            beyonddb::coordinator_target("123456789012", key.as_bytes()).unwrap()
                != coordinator_target
        })
        .unwrap();
    let second_target =
        beyonddb::coordinator_target("123456789012", second_key.as_bytes()).unwrap();
    restored_provisioner
        .admit_coordinator("123456789012", second_key.as_bytes())
        .await
        .unwrap();
    let mut actual_shards = Vec::new();
    let mut after = None;
    loop {
        let page = restored_client
            .query::<ListCoordinatorShards>(
                &account,
                None,
                Json(ListCoordinatorShardsInput {
                    account_id: "123456789012".into(),
                    after,
                    limit: 1,
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        if page.is_empty() {
            break;
        }
        after = page.last().copied();
        actual_shards.extend(page);
    }
    let mut expected_shards = registered;
    expected_shards.push(u32::from_be_bytes(
        second_target.partition().try_into().unwrap(),
    ));
    expected_shards.sort_unstable();
    expected_shards.dedup();
    assert_eq!(actual_shards, expected_shards);
    restored_host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_table_retries_two_data_cells_before_reporting_active() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "initial-provision-test".into(),
            cargo_lock_digest: Digest::from_bytes([2; 32]),
        })
        .unwrap(),
    );
    let account = account_target("123456789012").unwrap();
    let session = SessionId::from_bytes([31; 16]);
    let directory = tempfile::TempDir::new().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("beyonddb-initial-provision-test"),
        *account.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let bootstrap = Bootstrap {
        runtime: host.runtime(),
        registry: &registry,
        layout: &layout,
        session,
    };
    let account_handle = bootstrap
        .cell(
            &account,
            "beyonddb-account",
            32,
            &directory.path().join("account.sqlite"),
            initialize_account,
        )
        .await;
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            Arc::clone(&application),
            layout.clone(),
            session,
            "https://beyonddb-provision.internal:8081".into(),
            directory.path().join("data"),
        )
        .unwrap()
        .with_initial_partition_count(2)
        .unwrap(),
    );
    let creator = CellStorage::new(
        CellClient::local(Arc::clone(&registry), account_handle.clone()),
        "us-east-1",
    )
    .with_initial_partitions(Arc::new(FailOnceProvisioner {
        inner: provisioner.clone(),
        failed: AtomicBool::new(false),
    }));
    let create_input = || CreateTableInput {
        table_name: "Books".into(),
        key_schema: vec![KeySchemaElement {
            attribute_name: "id".into(),
            key_type: KeyType::Hash,
        }],
        attribute_definitions: vec![AttributeDefinition {
            attribute_name: "id".into(),
            attribute_type: ScalarAttributeType::S,
        }],
        billing_mode: Some(BillingMode::PayPerRequest),
        ..CreateTableInput::default()
    };
    let interrupted = creator.create_table("123456789012", create_input()).await;
    assert!(matches!(interrupted, Err(StorageError::Transient(_))));
    let creating = creator
        .describe_table(
            "123456789012",
            DescribeTableInput {
                table_name: "Books".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(creating.table_status, TableStatus::Creating);
    assert!(matches!(
        creator.table_key_info("123456789012", "Books").await,
        Err(StorageError::TableNotActive(_))
    ));
    let created = creator
        .create_table("123456789012", create_input())
        .await
        .unwrap();
    assert_eq!(created.table_status, TableStatus::Active);
    let account_client = host
        .application_handle::<Beyonddb>(
            CellClient::local(Arc::clone(&registry), account_handle.clone()),
            account.tenant(),
            account.application(),
        )
        .unwrap();
    let record = account_client
        .query::<DescribeTable>(&account, None, Json("Books".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let route = account_client
        .query::<ReadTableRoute>(&account, None, Json(record.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(route.partitions.len(), 2);
    assert_eq!(route.partitions[0].upper, route.partitions[1].lower);
    let data = data_target(
        "123456789012",
        &created.table_id,
        &route.partitions[0].partition_id,
    )
    .unwrap();
    let second_data = data_target(
        "123456789012",
        &created.table_id,
        &route.partitions[1].partition_id,
    )
    .unwrap();
    assert_ne!(data.cell_id(), second_data.cell_id());
    let proof = CellCatalog::new(layout.clone(), data.tenant())
        .lookup(data.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let control = authority.load(data.cell_id()).await.unwrap().unwrap();
    let data_handle = host
        .runtime()
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap();
    let second_proof = CellCatalog::new(layout.clone(), second_data.tenant())
        .lookup(second_data.cell_id())
        .await
        .unwrap()
        .unwrap();
    let second_control = authority
        .load(second_data.cell_id())
        .await
        .unwrap()
        .unwrap();
    let second_handle = host
        .runtime()
        .local_handle(second_proof, &second_control)
        .await
        .unwrap()
        .unwrap();
    let storage = CellStorage::new(
        CellClient::local_many(
            Arc::clone(&registry),
            [
                account_handle.clone(),
                data_handle.clone(),
                second_handle.clone(),
            ],
        )
        .unwrap(),
        "us-east-1",
    )
    .with_initial_partitions(provisioner.clone())
    .with_transaction_coordinators(provisioner);
    let key_info = storage
        .table_key_info("123456789012", "Books")
        .await
        .unwrap();
    let first_item = key_in_range(&record.id, &record.key_schema, true, 0);
    let second_item = key_in_range(&record.id, &record.key_schema, false, 0);
    for item in [&first_item, &second_item] {
        assert_eq!(
            storage
                .put_item(
                    &key_info,
                    item.clone(),
                    true,
                    None,
                    &ExpressionMaps::default(),
                    None,
                )
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            storage.get_item(&key_info, item).await.unwrap(),
            Some(item.clone())
        );
    }
    let payload = "x".repeat(300_000);
    for index in 0..120 {
        for lower_half in [true, false] {
            let mut item = key_in_range(
                &record.id,
                &record.key_schema,
                lower_half,
                1_000 + index * 1_000,
            );
            item.insert("payload".into(), AttributeValue::S(payload.clone()));
            storage
                .put_item(
                    &key_info,
                    item,
                    false,
                    None,
                    &ExpressionMaps::default(),
                    None,
                )
                .await
                .unwrap();
        }
    }
    let usage_client = CellClient::local_many(
        Arc::clone(&registry),
        [data_handle.clone(), second_handle.clone()],
    )
    .unwrap();
    let first_bytes = usage_client
        .query::<PartitionUsage>(&data, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .item_bytes;
    let second_bytes = usage_client
        .query::<PartitionUsage>(&second_data, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .item_bytes;
    assert!(first_bytes + second_bytes > 64 * 1024 * 1024);
    data_handle.drain().await.unwrap();
    second_handle.drain().await.unwrap();
    account_handle.drain().await.unwrap();
    host.shutdown().await.unwrap();

    let next_session = SessionId::from_bytes([33; 16]);
    let restored_host = CellNodeBuilder::new(Arc::clone(&application))
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(next_session)
        .build_unleased_for_maintenance()
        .unwrap();
    let restored_provisioner = CellInitialPartitionProvisioner::new(
        restored_host.runtime(),
        Arc::clone(&application),
        layout.clone(),
        next_session,
        "https://beyonddb-restored.internal:8081".into(),
        directory.path().join("restored-data"),
    )
    .unwrap()
    .with_initial_partition_count(2)
    .unwrap();
    let restored_specs = restored_provisioner
        .provision("123456789012", &record)
        .await
        .unwrap();
    assert_eq!(restored_specs, route.partitions);
    let proof = CellCatalog::new(layout.clone(), data.tenant())
        .lookup(data.cell_id())
        .await
        .unwrap()
        .unwrap();
    let authority = CellAuthority::new(layout.clone());
    let control = authority.load(data.cell_id()).await.unwrap().unwrap();
    let restored = restored_host
        .runtime()
        .local_handle(proof, &control)
        .await
        .unwrap()
        .unwrap();
    let second_proof = CellCatalog::new(layout.clone(), second_data.tenant())
        .lookup(second_data.cell_id())
        .await
        .unwrap()
        .unwrap();
    let second_control = authority
        .load(second_data.cell_id())
        .await
        .unwrap()
        .unwrap();
    let restored_second = restored_host
        .runtime()
        .local_handle(second_proof, &second_control)
        .await
        .unwrap()
        .unwrap();
    let client = restored_host
        .application_handle::<Beyonddb>(
            CellClient::local_many(application.registry(), [restored, restored_second]).unwrap(),
            data.tenant(),
            data.application(),
        )
        .unwrap();
    let recovered_bytes = client
        .query::<PartitionUsage>(&data, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .item_bytes
        + client
            .query::<PartitionUsage>(&second_data, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .item_bytes;
    assert_eq!(recovered_bytes, first_bytes + second_bytes);
    for (target, item) in [(&data, first_item), (&second_data, second_item)] {
        let persisted = client
            .query::<PartitionGet>(
                target,
                None,
                Json(PartitionGetInput {
                    table_id: record.id.clone(),
                    epoch: 1,
                    key: item.clone(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(persisted.output.0, PartitionGetOutcome::Found(Some(item)));
    }
    restored_host.shutdown().await.unwrap();
}
