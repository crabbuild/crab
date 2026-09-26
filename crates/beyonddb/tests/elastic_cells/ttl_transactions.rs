use crate::*;
use beyonddb::{ExpiredPartitionInput, ExpiredPartitionOutcome, ReadExpiredPartition};
use extenddb_core::types::TableKeyInfo;

fn mutation() -> MutationIdentity {
    let mut input = identity(239);
    input.request_id = RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes());
    input
}

fn key(id: &str) -> Item {
    Item::from([("id".into(), AttributeValue::S(id.into()))])
}

async fn seed(storage: &CellStorage, info: &TableKeyInfo, id: &str, expires: u64) {
    let mut item = key(id);
    item.insert("expires".into(), AttributeValue::N(expires.to_string()));
    storage
        .put_item(info, item, false, None, &ExpressionMaps::default(), None)
        .await
        .unwrap();
}

async fn begin(
    client: &CellClient,
    coordinator: &CellTarget,
    info: &TableKeyInfo,
    spec: &PartitionSpec,
    transaction_id: [u8; 16],
    operation: TransactionOperation,
) -> PreparePartitionTransactionInput {
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        coordinator,
        mutation(),
        Json(BeginCrossCellTransactionInput {
            account_id: info.account_id.clone(),
            transaction_id,
            token: None,
            participants: vec![CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Data {
                    table_id: info.table_id.clone(),
                    partition_id: spec.partition_id,
                    epoch: spec.epoch,
                },
                operations: vec![IndexedTransactionOperation {
                    index: 0,
                    operation: operation.clone(),
                }],
            }],
        }),
    )
    .await
    .unwrap();
    PreparePartitionTransactionInput {
        table_id: info.table_id.clone(),
        epoch: spec.epoch,
        transaction_id,
        coordinator_cell: *coordinator.cell_id().as_bytes(),
        coordinator_key: transaction_id.to_vec(),
        operations: vec![operation],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ttl_skips_transaction_locks_and_revisits_after_abort() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "ttl-transaction-fairness".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let account = account_target(account_id).unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let session = SessionId::from_bytes([239; 16]);
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("ttl-transactions"),
        *account.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application,
            layout.clone(),
            session,
            "http://ttl-test.internal".into(),
            directory.path().into(),
        )
        .unwrap(),
    );
    provisioner.admit_account(account_id).await.unwrap();
    let client = CellClient::local_runtime(registry.clone(), host.runtime(), layout.clone());
    let storage =
        CellStorage::new(client.clone(), "us-east-1").with_initial_partitions(provisioner.clone());
    let mut infos = Vec::new();
    for name in ["TtlBlocked", "TtlHealthy"] {
        let input = serde_json::from_value(serde_json::json!({
            "TableName": name,
            "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
            "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
            "BillingMode": "PAY_PER_REQUEST"
        }))
        .unwrap();
        storage.create_table(account_id, input).await.unwrap();
        infos.push(storage.table_key_info(account_id, name).await.unwrap());
        storage
            .update_ttl(account_id, name, "expires", true)
            .await
            .unwrap();
        storage
            .create_ttl_index(account_id, name, "expires")
            .await
            .unwrap();
    }
    let info = &infos[0];
    let route = client
        .query::<ReadTableRoute>(&account, None, Json(info.table_id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let spec = &route.partitions[0];
    let data = data_target(account_id, &info.table_id, &spec.partition_id).unwrap();
    for (id, expires) in [("shared", 1), ("exclusive", 2), ("healthy", 3)] {
        seed(&storage, info, id, expires).await;
    }
    seed(&storage, &infos[1], "later-table", 1).await;
    // Use distinct IDs in one shard so unrelated admission pressure cannot
    // mask whether the transaction locks starve TTL candidates.
    let first_id = [239; 16];
    provisioner
        .admit_coordinator(account_id, &first_id)
        .await
        .unwrap();
    let coordinator = coordinator_target(account_id, &first_id).unwrap();
    let ids: Vec<_> = (1_u128..100_000)
        .map(u128::to_be_bytes)
        .filter(|id| coordinator_target(account_id, id).unwrap() == coordinator)
        .take(3)
        .collect();
    assert_eq!(ids.len(), 3);
    let mut prepares = Vec::new();
    for (id, name, shared) in [(ids[0], "shared", true), (ids[1], "exclusive", false)] {
        let operation = if shared {
            TransactionOperation::Read(GetItemInput {
                table_name: info.table_name.clone(),
                table_id: info.table_id.clone(),
                key: key(name),
            })
        } else {
            TransactionOperation::Put(PutItemInput {
                table_name: info.table_name.clone(),
                table_id: info.table_id.clone(),
                item: key(name),
                condition: None,
            })
        };
        let prepare = begin(&client, &coordinator, info, spec, id, operation).await;
        transaction_command!(
            client,
            PreparePartitionTransaction,
            &data,
            mutation(),
            Json(prepare.clone())
        )
        .await
        .unwrap();
        prepares.push(prepare);
    }
    // Both oldest expired rows are locked, in different modes. Selection must
    // still reach the third row, and the sweep must visit the following table.
    let candidates = client
        .query::<ReadExpiredPartition>(
            &data,
            None,
            Json(ExpiredPartitionInput {
                table_id: info.table_id.clone(),
                epoch: spec.epoch,
                attribute_name: "expires".into(),
                cutoff_epoch: 10,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    let ExpiredPartitionOutcome::Items(candidates) = candidates else {
        panic!("TTL was not ready")
    };
    assert_eq!(
        candidates
            .iter()
            .map(|item| item["id"].clone())
            .collect::<Vec<_>>(),
        vec![AttributeValue::S("healthy".into())]
    );
    assert_eq!(storage.sweep_account_ttl(account_id).await.unwrap(), 2);

    seed(&storage, info, "raced", 3).await;
    seed(&storage, info, "race-sibling", 4).await;
    seed(&storage, &infos[1], "later-table", 1).await;
    let prepare = begin(
        &client,
        &coordinator,
        info,
        spec,
        ids[2],
        TransactionOperation::Read(GetItemInput {
            table_name: info.table_name.clone(),
            table_id: info.table_id.clone(),
            key: key("raced"),
        }),
    )
    .await;
    prepares.push(prepare.clone());
    let caller_session = SessionId::from_bytes([238; 16]);
    let caller = CellRuntime::new(
        SqlWorkerPool::new(1, 2).unwrap(),
        16 * 1024 * 1024,
        caller_session,
    )
    .unwrap();
    let signer = PeerSigner::new(
        caller_session,
        registry.release_digest(),
        SigningKey::from_bytes(&[238; 32]),
    );
    let raced = Arc::new(AtomicBool::new(false));
    let transport = LockAfterCandidates {
        verifier: Arc::new(PeerVerifier::new(
            caller_session,
            registry.release_digest(),
            signer.verifying_key(),
        )),
        dispatcher: Arc::new(PeerDispatcher::new(
            registry.clone(),
            Arc::new(LocalRuntimePeerResolver {
                runtime: host.runtime(),
                layout: layout.clone(),
            }),
            Arc::new(TestPeerAuthorizer),
        )),
        client: client.clone(),
        target: data.clone(),
        prepare,
        raced: raced.clone(),
    };
    let routed = CellClient::runtime_with_peer(
        registry,
        caller.clone(),
        layout,
        Arc::new(signer),
        PeerPrincipal {
            issuer: "ttl-test".into(),
            subject: "ttl-worker".into(),
            actions: vec!["beyonddb.cell.invoke".into()],
        },
        Arc::new(transport),
    );
    assert_eq!(
        CellStorage::new(routed, "us-east-1")
            .sweep_account_ttl(account_id)
            .await
            .unwrap(),
        2
    );
    assert!(raced.load(Ordering::SeqCst));
    assert!(
        storage
            .get_item(info, &key("raced"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        storage
            .get_item(info, &key("race-sibling"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        storage
            .get_item(&infos[1], &key("later-table"))
            .await
            .unwrap()
            .is_none()
    );
    caller.shutdown().await.unwrap();

    for prepare in prepares {
        client
            .command::<DecideCrossCellTransaction>(
                &coordinator,
                mutation(),
                Json(DecideCrossCellTransactionInput {
                    account_id: account_id.into(),
                    transaction_id: prepare.transaction_id,
                    routing_key: prepare.coordinator_key.clone(),
                    decision: CoordinatorDecision::Abort {
                        index: None,
                        reason: None,
                    },
                }),
            )
            .await
            .unwrap();
        storage
            .finish_decided_cross_cell_transaction(
                account_id,
                &prepare.coordinator_key,
                prepare.transaction_id,
            )
            .await
            .unwrap();
    }
    // Deferred items remain indexed. Subsequent passes delete them after the
    // authoritative abort releases the locks; none was lost behind a cursor.
    let deleted = storage.sweep_account_ttl(account_id).await.unwrap()
        + storage.sweep_account_ttl(account_id).await.unwrap();
    assert_eq!(deleted, 3);
    for id in ["shared", "exclusive", "raced"] {
        assert!(storage.get_item(info, &key(id)).await.unwrap().is_none());
    }
    host.shutdown().await.unwrap();
}

struct LockAfterCandidates {
    verifier: Arc<PeerVerifier>,
    dispatcher: Arc<PeerDispatcher>,
    client: CellClient,
    target: CellTarget,
    prepare: PreparePartitionTransactionInput,
    raced: Arc<AtomicBool>,
}

impl PeerRoundTrip for LockAfterCandidates {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        let verifier = self.verifier.clone();
        let dispatcher = self.dispatcher.clone();
        let client = self.client.clone();
        let owner = self.target.clone();
        let prepare = self.prepare.clone();
        let raced = self.raced.clone();
        Box::pin(async move {
            use crab_cell_runtime::peer::wire::{peer_request, read_request};
            use crab_cell_runtime::registry::Query;
            let now_ms = mutation().issued_at_ms;
            let verified = verifier.verify(&request, now_ms)?;
            assert_eq!(verified.target(), &target);
            let candidates = target == owner
                && matches!(verified.operation(),
                Some(peer_request::Operation::Read(read)) if matches!(&read.operation,
                    Some(read_request::Operation::CellQuery(query)) if query.query_id == ReadExpiredPartition::ID));
            let response = dispatcher.dispatch_bytes(&verified, now_ms).await?;
            if candidates && !raced.swap(true, Ordering::SeqCst) {
                transaction_command!(
                    client,
                    PreparePartitionTransaction,
                    &owner,
                    mutation(),
                    Json(prepare)
                )
                .await
                .unwrap();
            }
            Ok(response)
        })
    }
}
