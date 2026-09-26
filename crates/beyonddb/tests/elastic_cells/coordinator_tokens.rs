use crate::*;
use beyonddb::{ReadCoordinatorToken, ReadCoordinatorTokenOutcome};

const ACCOUNT: &str = "123456789012";
const TOKEN: &str = "retained-token";

fn request() -> BeginCrossCellTransactionInput {
    let account = account_target(ACCOUNT).unwrap();
    let mut table_bytes = [9; 32];
    table_bytes[..16].copy_from_slice(account.tenant().as_bytes());
    let table_id = blake3::Hash::from_bytes(table_bytes).to_hex().to_string();
    let mut participants: Vec<_> = [1_u8, 2]
        .into_iter()
        .map(|id| CoordinatorParticipant {
            target: CoordinatorParticipantTarget::Data {
                table_id: table_id.clone(),
                partition_id: [id; 16],
                epoch: 1,
            },
            operations: vec![IndexedTransactionOperation {
                index: id - 1,
                operation: TransactionOperation::Put(PutItemInput {
                    table_name: "Tokens".into(),
                    table_id: table_id.clone(),
                    item: Item::from([("id".into(), AttributeValue::S(id.to_string()))]),
                    condition: None,
                }),
            }],
        })
        .collect();
    participants.sort_by_key(|participant| {
        let CoordinatorParticipantTarget::Data { partition_id, .. } = &participant.target else {
            unreachable!()
        };
        *data_target(ACCOUNT, &table_id, partition_id)
            .unwrap()
            .cell_id()
            .as_bytes()
    });
    BeginCrossCellTransactionInput {
        account_id: ACCOUNT.into(),
        transaction_id: [10; 16],
        token: Some(TransactionToken {
            account_id: ACCOUNT.into(),
            token: TOKEN.into(),
            fingerprint: "original".into(),
        }),
        participants,
    }
}

// Historical snapshots use old logical times so expiry is exercised without
// sleeping ten minutes or changing a live Cell's SQLite state behind its actor.
fn seed(
    transaction: &rusqlite::Transaction<'_>,
    state: i64,
    unresolved: i64,
) -> crab_cell_runtime::Result<()> {
    initialize_coordinator(transaction)?;
    let input = request();
    let digest = blake3::hash(&serde_json::to_vec(&input.participants).unwrap());
    transaction.execute(
        "INSERT INTO ddb_coordinator_transactions \
         (transaction_id, account_id, token, fingerprint, request_digest, state, unresolved_count, created_at_ms, decided_at_ms, completed_at_ms) \
         VALUES (?1, ?2, ?3, 'original', ?4, ?5, ?6, 1, ?7, ?8)",
        rusqlite::params![input.transaction_id.as_slice(), ACCOUNT, TOKEN, digest.as_bytes().as_slice(), state, unresolved, (state != 0).then_some(2_i64), (unresolved == 0).then_some(3_i64)],
    )?;
    for (position, participant) in input.participants.iter().enumerate() {
        let CoordinatorParticipantTarget::Data {
            table_id,
            partition_id,
            ..
        } = &participant.target
        else {
            unreachable!()
        };
        let cell = data_target(ACCOUNT, table_id, partition_id)?;
        transaction.execute(
            "INSERT INTO ddb_coordinator_participants \
             (transaction_id, position, cell_id, target, operation_chunks, prepared_sequence, resolved_sequence) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6)",
            rusqlite::params![input.transaction_id.as_slice(), position as i64, cell.cell_id().as_bytes().as_slice(), serde_json::to_vec(&participant.target).unwrap(), (state != 0).then_some(1_i64), ((position as i64) < 2 - unresolved).then_some(2_i64)],
        )?;
        transaction.execute(
            "INSERT INTO ddb_transaction_payloads (transaction_id, position, chunk, payload) VALUES (?1, ?2, 0, ?3)",
            rusqlite::params![input.transaction_id.as_slice(), position as i64, serde_json::to_vec(&participant.operations).unwrap()],
        )?;
    }
    Ok(())
}

fn old_begin(tx: &rusqlite::Transaction<'_>) -> crab_cell_runtime::Result<()> {
    seed(tx, 0, 2)
}
fn old_unresolved_commit(tx: &rusqlite::Transaction<'_>) -> crab_cell_runtime::Result<()> {
    seed(tx, 1, 1)
}
fn old_completed_commit(tx: &rusqlite::Transaction<'_>) -> crab_cell_runtime::Result<()> {
    seed(tx, 1, 0)
}

type Initialize = for<'a> fn(&rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokens_pin_unfinished_work_and_replay_original_routes_until_completion_window_expires() {
    let cases: [(Initialize, CoordinatorDecision, bool); 3] = [
        (old_begin, CoordinatorDecision::Begin, false),
        (old_unresolved_commit, CoordinatorDecision::Commit, false),
        (old_completed_commit, CoordinatorDecision::Commit, true),
    ];
    for (initialize, original_decision, expired) in cases {
        let application = Arc::new(
            Beyonddb::compile(BuildDescriptor {
                source_revision: "coordinator-token-test".into(),
                cargo_lock_digest: Digest::from_bytes([1; 32]),
            })
            .unwrap(),
        );
        let coordinator = coordinator_target(ACCOUNT, TOKEN.as_bytes()).unwrap();
        let session = SessionId::from_bytes([210; 16]);
        let directory = tempfile::TempDir::new().unwrap();
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            object_store::path::Path::from("coordinator-tokens"),
            *coordinator.application().as_bytes(),
        );
        let host = CellNodeBuilder::new(application.clone())
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
        let handle = bootstrap
            .cell(
                &coordinator,
                "beyonddb-coordinator",
                210,
                &directory.path().join("coordinator.sqlite"),
                initialize,
            )
            .await;
        let client = CellClient::local(registry.clone(), handle);
        let original = request();
        let token = original.token.clone().unwrap();
        let read = |token| client.query::<ReadCoordinatorToken>(&coordinator, None, Json(token));
        let expected = if expired {
            ReadCoordinatorTokenOutcome::Missing
        } else {
            ReadCoordinatorTokenOutcome::Found {
                transaction_id: original.transaction_id,
                decision: original_decision.clone(),
            }
        };
        assert_eq!(read(token.clone()).await.unwrap().output.0, expected);
        let changed_token = TransactionToken {
            fingerprint: "changed".into(),
            ..token.clone()
        };
        assert_eq!(
            read(changed_token.clone()).await.unwrap().output.0,
            if expired {
                ReadCoordinatorTokenOutcome::Missing
            } else {
                ReadCoordinatorTokenOutcome::Mismatch
            }
        );
        let mut retry = original.clone();
        retry.transaction_id = [20; 16];
        for participant in &mut retry.participants {
            let CoordinatorParticipantTarget::Data { epoch, .. } = &mut participant.target else {
                unreachable!()
            };
            *epoch = 2;
        }
        if expired {
            retry.token.as_mut().unwrap().fingerprint = "replacement".into();
            let TransactionOperation::Put(input) =
                &mut retry.participants[0].operations[0].operation
            else {
                unreachable!()
            };
            input
                .item
                .insert("replacement".into(), AttributeValue::Bool(true));
        }
        let active_token = retry.token.clone().unwrap();
        let begun = client
            .command::<BeginCrossCellTransaction>(&coordinator, identity(211), Json(retry.clone()))
            .await
            .unwrap()
            .output
            .0;
        assert_eq!(
            begun,
            if expired {
                BeginCrossCellTransactionOutcome::Begun
            } else {
                BeginCrossCellTransactionOutcome::Existing {
                    transaction_id: original.transaction_id,
                    decision: original_decision.clone(),
                }
            }
        );
        let original_payload = client
            .query::<ReadCoordinatorParticipant>(
                &coordinator,
                None,
                Json(ReadCoordinatorParticipantInput {
                    account_id: ACCOUNT.into(),
                    transaction_id: original.transaction_id,
                    routing_key: TOKEN.as_bytes().to_vec(),
                    position: 0,
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert_eq!(original_payload, original.participants[0]);
        retry.token = Some(changed_token);
        assert!(
            matches!(client.command::<BeginCrossCellTransaction>(&coordinator, identity(212), Json(retry)).await, Err(InvocationError::Rejected(result)) if result.output.0 == BeginCrossCellTransactionOutcome::Mismatch)
        );
        if !expired {
            if original_decision == CoordinatorDecision::Begin {
                client
                    .command::<DecideCrossCellTransaction>(
                        &coordinator,
                        identity(213),
                        Json(DecideCrossCellTransactionInput {
                            account_id: ACCOUNT.into(),
                            transaction_id: original.transaction_id,
                            routing_key: TOKEN.as_bytes().to_vec(),
                            decision: CoordinatorDecision::Abort {
                                index: None,
                                reason: None,
                            },
                        }),
                    )
                    .await
                    .unwrap();
            }
            for (position, participant) in original.participants.iter().enumerate() {
                let CoordinatorParticipantTarget::Data {
                    table_id,
                    partition_id,
                    ..
                } = &participant.target
                else {
                    unreachable!()
                };
                let phase = CoordinatorPhaseInput {
                    account_id: ACCOUNT.into(),
                    transaction_id: original.transaction_id,
                    routing_key: TOKEN.as_bytes().to_vec(),
                    position: position as u8,
                    participant_cell: *data_target(ACCOUNT, table_id, partition_id)
                        .unwrap()
                        .cell_id()
                        .as_bytes(),
                    sequence: 3,
                };
                client
                    .command::<RecordParticipantResolution>(
                        &coordinator,
                        identity(214 + position as u8),
                        Json(phase.clone()),
                    )
                    .await
                    .unwrap();
                client
                    .command::<RecordParticipantResolution>(
                        &coordinator,
                        identity(216 + position as u8),
                        Json(phase),
                    )
                    .await
                    .unwrap();
            }
            // Successful completion starts replay; fully resolved aborts release
            // the token slot like ExtendDB's rolled-back SQLite transaction.
            let outcome = read(token).await.unwrap().output.0;
            if original_decision == CoordinatorDecision::Begin {
                assert_eq!(outcome, ReadCoordinatorTokenOutcome::Missing);
            } else {
                assert!(matches!(
                    outcome,
                    ReadCoordinatorTokenOutcome::Found {
                        transaction_id: [10, ..],
                        ..
                    }
                ));
            }
        } else {
            assert_eq!(
                read(active_token).await.unwrap().output.0,
                ReadCoordinatorTokenOutcome::Found {
                    transaction_id: [20; 16],
                    decision: CoordinatorDecision::Begin
                }
            );
        }
        host.shutdown().await.unwrap();
    }
}
