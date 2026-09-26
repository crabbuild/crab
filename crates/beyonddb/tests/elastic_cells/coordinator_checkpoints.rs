use super::coordinator_residency::write;
use crate::*;

const ACCOUNT: &str = "123456789012";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settled_recovery_survives_restart_but_later_begin_is_not_skipped() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "settled-recovery".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let directory = tempfile::tempdir().unwrap();
    let account = account_target(ACCOUNT).unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("settled-recovery"),
        *account.application().as_bytes(),
    );
    let nodes = NodeDirectory::new(
        layout.clone(),
        Digest::from_bytes([201; 32]),
        Digest::from_bytes([202; 32]),
        application.registry().release_digest(),
    );
    let session = SessionId::from_bytes([249; 16]);
    let (host, provisioner, client, storage) = owner(
        &application,
        &layout,
        session,
        &directory.path().join("first"),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    storage
        .create_table(
            ACCOUNT,
            serde_json::from_value(serde_json::json!({
                "TableName": "Residency",
                "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
                "BillingMode": "PAY_PER_REQUEST"
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let info = storage.table_key_info(ACCOUNT, "Residency").await.unwrap();
    let mut seen = std::collections::HashSet::new();
    let tokens: Vec<_> = (0..100)
        .map(|n| format!("checkpoint-{n}"))
        .filter(|token| {
            seen.insert(
                coordinator_target(ACCOUNT, token.as_bytes())
                    .unwrap()
                    .cell_id(),
            )
        })
        .take(3)
        .collect();
    assert_eq!(tokens.len(), 3);
    for (version, token) in tokens.iter().enumerate() {
        assert!(!write(&storage, &info, token, version).await);
    }
    // Observe settled roots through the public startup recovery path. One of
    // those observations will deliberately become stale before this owner exits.
    provisioner
        .recover_registered_coordinators(ACCOUNT, &client, &storage, &nodes)
        .await
        .unwrap();
    let stale = coordinator_target(ACCOUNT, tokens[0].as_bytes()).unwrap();
    let id = (1_u128..100_000)
        .map(u128::to_be_bytes)
        .find(|id| coordinator_target(ACCOUNT, id).unwrap() == stale)
        .unwrap();
    let key = Item::from([("id".into(), AttributeValue::S("shared".into()))]);
    let operation = TransactionOperation::Read(GetItemInput {
        table_name: info.table_name.clone(),
        table_id: info.table_id.clone(),
        key: key.clone(),
    });
    transaction_command!(
        client,
        BeginCrossCellTransaction,
        &stale,
        identity(250),
        Json(BeginCrossCellTransactionInput {
            account_id: ACCOUNT.into(),
            transaction_id: id,
            token: None,
            participants: vec![CoordinatorParticipant {
                target: CoordinatorParticipantTarget::Account,
                operations: vec![IndexedTransactionOperation {
                    index: 0,
                    operation: operation.clone()
                }],
            }],
        }),
    )
    .await
    .unwrap();
    transaction_command!(
        client,
        beyonddb::PrepareAccountTransaction,
        &account,
        identity(251),
        Json(beyonddb::PrepareAccountTransactionInput {
            transaction_id: id,
            coordinator_cell: *stale.cell_id().as_bytes(),
            coordinator_key: id.to_vec(),
            operations: vec![operation],
        }),
    )
    .await
    .unwrap();
    host.shutdown().await.unwrap();
    let authority = CellAuthority::new(layout.clone());
    let mut before = Vec::new();
    for token in &tokens {
        let target = coordinator_target(ACCOUNT, token.as_bytes()).unwrap();
        let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert!(observed.value().owner.is_none());
        before.push((target, observed.value().epoch));
    }
    let (host, provisioner, client, storage) = owner(
        &application,
        &layout,
        SessionId::from_bytes([250; 16]),
        &directory.path().join("restored"),
    );
    let account_handle = provisioner.admit_account(ACCOUNT).await.unwrap();
    provisioner
        .recover_registered_account(ACCOUNT, account_handle, &client, &storage, &nodes)
        .await
        .unwrap();
    for (target, epoch) in &before {
        let observed = authority.load(target.cell_id()).await.unwrap().unwrap();
        if *target == stale {
            assert!(
                observed.value().epoch > *epoch,
                "changed root must be recovered"
            );
        } else {
            assert_eq!(
                observed.value().epoch,
                *epoch,
                "settled history was needlessly restored"
            );
            assert!(observed.value().owner.is_none());
        }
    }
    // Inspect raw phase state before a public read could help recovery. Startup
    // must abort BEGIN and release the prepared shared lock, despite its old hint.
    let state = client
        .query::<beyonddb::ReadAccountTransaction>(
            &account,
            None,
            Json(ReadTransactionInput {
                transaction_id: id,
                coordinator_cell: *stale.cell_id().as_bytes(),
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(state, ParticipantTransactionState::Aborted);
    assert!(!write(&storage, &info, "after-checkpoint-recovery", 3).await);
    assert!(write(&storage, &info, &tokens[1], 1).await);
    let value = storage.get_item(&info, &key).await.unwrap().unwrap();
    assert_eq!(value.get("version"), Some(&AttributeValue::N("3".into())));
    host.shutdown().await.unwrap();
}

fn owner(
    application: &Arc<crab_cell_app::CompiledApplication>,
    layout: &CellStorageLayout,
    session: SessionId,
    directory: &std::path::Path,
) -> (
    crab_cell_host::CellNode,
    Arc<CellInitialPartitionProvisioner>,
    CellClient,
    CellStorage,
) {
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let provisioner = Arc::new(
        CellInitialPartitionProvisioner::new(
            host.runtime(),
            application.clone(),
            layout.clone(),
            session,
            "http://checkpoint.internal".into(),
            directory.into(),
        )
        .unwrap(),
    );
    let client = CellClient::local_runtime(application.registry(), host.runtime(), layout.clone());
    let storage = CellStorage::new(client.clone(), "us-east-1")
        .with_transaction_coordinators(provisioner.clone());
    (host, provisioner, client, storage)
}
