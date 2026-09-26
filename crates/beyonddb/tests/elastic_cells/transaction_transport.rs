use crate::*;
use beyonddb::{TransactionPayloadRef, UploadTransactionPayload};

fn mutation() -> MutationIdentity {
    MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        ..identity(246)
    }
}

fn historical_upload(tx: &rusqlite::Transaction<'_>) -> crab_cell_runtime::Result<()> {
    initialize_coordinator(tx)?;
    tx.execute("INSERT INTO ddb_transaction_uploads (digest, expires_at_ms, upload_id, chunk, bytes, payload) VALUES (?1, 1, zeroblob(16), 0, 7, ?2)",
        rusqlite::params![[0_u8;32].as_slice(), b"expired".as_slice()])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_seals_only_complete_immutable_inputs_after_owner_replacement() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "transaction-transport".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account_id = "123456789012";
    let transaction_id = [246; 16];
    let coordinator = coordinator_target(account_id, &transaction_id).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("transaction-transport"),
        *coordinator.application().as_bytes(),
    );
    let host = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(SessionId::from_bytes([246; 16]))
        .build_unleased_for_maintenance()
        .unwrap();
    let registry = application.registry();
    let original_file = directory.path().join("original.sqlite");
    Bootstrap {
        runtime: host.runtime(),
        registry: &registry,
        layout: &layout,
        session: SessionId::from_bytes([246; 16]),
    }
    .cell(
        &coordinator,
        "beyonddb-coordinator",
        246,
        &original_file,
        historical_upload,
    )
    .await;
    let client = CellClient::local_runtime(registry.clone(), host.runtime(), layout.clone());
    let input = BeginCrossCellTransactionInput {
        account_id: account_id.into(),
        transaction_id,
        token: None,
        participants: vec![CoordinatorParticipant {
            target: CoordinatorParticipantTarget::Account,
            operations: (0..4)
                .map(|index| IndexedTransactionOperation {
                    index,
                    operation: TransactionOperation::Put(PutItemInput {
                        table_name: "Uploads".into(),
                        table_id: "unused-at-admission".into(),
                        item: Item::from([
                            ("id".into(), AttributeValue::S(index.to_string())),
                            ("payload".into(), AttributeValue::S("\0".repeat(200 * 1024))),
                        ]),
                        condition: None,
                    }),
                })
                .collect(),
        }],
    };
    let bytes = serde_json::to_vec(&input).unwrap();
    assert!(bytes.len() > 4 * 1024 * 1024 + 64 * 1024);
    let reference = TransactionPayloadRef::new(&bytes, mutation().expires_at_ms).unwrap();
    let chunks = reference.chunks(&bytes).collect::<Vec<_>>();
    let read = ReadCrossCellTransactionInput {
        account_id: account_id.into(),
        transaction_id,
        routing_key: transaction_id.to_vec(),
    };
    for _ in 0..2 {
        client
            .command::<UploadTransactionPayload<BeginCrossCellTransaction>>(
                &coordinator,
                mutation(),
                chunks[0].clone(),
            )
            .await
            .unwrap();
    }
    let mut changed = chunks[0].clone();
    changed.payload[0] ^= 1;
    assert!(
        client
            .command::<UploadTransactionPayload<BeginCrossCellTransaction>>(
                &coordinator,
                mutation(),
                changed
            )
            .await
            .is_err()
    );
    assert!(
        client
            .command::<BeginCrossCellTransaction>(&coordinator, mutation(), Json(reference.clone()))
            .await
            .is_err()
    );
    assert!(
        client
            .query::<ReadCrossCellTransaction>(&coordinator, None, Json(read.clone()))
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    // Neither an already-expired reference nor a complete but forged digest
    // can turn an upload into a durable transaction.
    let mut expired = chunks[0].clone();
    expired.reference.expires_at_ms = 1;
    assert!(
        client
            .command::<UploadTransactionPayload<BeginCrossCellTransaction>>(
                &coordinator,
                mutation(),
                expired
            )
            .await
            .is_err()
    );
    let mut forged = TransactionPayloadRef::new(b"{}", mutation().expires_at_ms).unwrap();
    forged.digest[0] ^= 1;
    client
        .command::<UploadTransactionPayload<BeginCrossCellTransaction>>(
            &coordinator,
            mutation(),
            forged.chunks(b"{}").next().unwrap(),
        )
        .await
        .unwrap();
    assert!(
        client
            .command::<BeginCrossCellTransaction>(&coordinator, mutation(), Json(forged))
            .await
            .is_err()
    );
    host.shutdown().await.unwrap();
    let database = rusqlite::Connection::open_with_flags(
        &original_file,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        database
            .query_row(
                "SELECT COUNT(*) FROM ddb_transaction_uploads WHERE expires_at_ms = 1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    drop(database);

    let session = SessionId::from_bytes([247; 16]);
    let replacement = CellNodeBuilder::new(application.clone())
        .with_runtime(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024)
        .with_replica_host(Host::default().with_local_disk_budget(DiskBudget::new(1 << 30)))
        .with_session(session)
        .build_unleased_for_maintenance()
        .unwrap();
    let provisioner = CellInitialPartitionProvisioner::new(
        replacement.runtime(),
        application,
        layout.clone(),
        session,
        "https://replacement.internal".into(),
        directory.path().join("replacement"),
    )
    .unwrap();
    provisioner
        .admit_coordinator(account_id, &transaction_id)
        .await
        .unwrap();
    let client = CellClient::local_runtime(registry, replacement.runtime(), layout);
    assert!(
        client
            .query::<ReadCrossCellTransaction>(&coordinator, None, Json(read.clone()))
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    // Restore the published first chunk, accept its duplicate, and assemble
    // the remaining chunks out of order on a different owner.
    for chunk in chunks.into_iter().rev() {
        client
            .command::<UploadTransactionPayload<BeginCrossCellTransaction>>(
                &coordinator,
                mutation(),
                chunk,
            )
            .await
            .unwrap();
    }
    // Two drivers can have identical bytes and millisecond deadlines. One
    // driver's seal must not consume the other's independently staged input.
    let competing = TransactionPayloadRef::new(&bytes, reference.expires_at_ms).unwrap();
    for chunk in competing.chunks(&bytes) {
        client
            .command::<UploadTransactionPayload<BeginCrossCellTransaction>>(
                &coordinator,
                mutation(),
                chunk,
            )
            .await
            .unwrap();
    }
    let identity = mutation();
    let first = client
        .command::<BeginCrossCellTransaction>(&coordinator, identity, Json(reference.clone()))
        .await
        .unwrap();
    assert_eq!(first.output.0, BeginCrossCellTransactionOutcome::Begun);
    let replay = client
        .command::<BeginCrossCellTransaction>(&coordinator, identity, Json(reference.clone()))
        .await
        .unwrap();
    assert_eq!(first.receipt, replay.receipt);
    assert_eq!(
        client
            .command::<BeginCrossCellTransaction>(&coordinator, mutation(), Json(competing))
            .await
            .unwrap()
            .output
            .0,
        BeginCrossCellTransactionOutcome::Existing {
            transaction_id,
            decision: CoordinatorDecision::Begin,
        }
    );
    // Consumption removed the temporary input; the transaction's recovery
    // payload remains authoritative and is independently readable in pieces.
    assert!(
        client
            .command::<BeginCrossCellTransaction>(&coordinator, mutation(), Json(reference))
            .await
            .is_err()
    );
    let mut recovered = Vec::new();
    for chunk in 0.. {
        let result = client
            .query::<ReadCoordinatorParticipant>(
                &coordinator,
                None,
                Json(ReadCoordinatorParticipantInput {
                    account_id: account_id.into(),
                    transaction_id,
                    routing_key: transaction_id.to_vec(),
                    position: 0,
                    chunk,
                }),
            )
            .await
            .unwrap()
            .output
            .unwrap();
        recovered.extend_from_slice(&result.payload);
        if chunk + 1 == result.chunks {
            break;
        }
    }
    assert_eq!(
        serde_json::from_slice::<Vec<IndexedTransactionOperation>>(&recovered).unwrap(),
        input.participants[0].operations
    );
    assert_eq!(
        client
            .query::<ReadCrossCellTransaction>(&coordinator, None, Json(read))
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .decision,
        CoordinatorDecision::Begin
    );
    replacement.shutdown().await.unwrap();
}
