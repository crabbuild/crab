use crate::*;
use extenddb_core::types::TableKeyInfo;
use extenddb_storage::TransactWriteOp;

use super::transaction_driver::DropPhaseReplies;

pub(super) async fn assert_lost_replies_and_canceled_token_reuse(
    owner: &crab_cell_host::CellNode,
    registry: Arc<Registry>,
    layout: CellStorageLayout,
    provisioner: Arc<CellInitialPartitionProvisioner>,
    infos: [TableKeyInfo; 2],
) {
    let session = SessionId::from_bytes([221; 16]);
    let runtime =
        CellRuntime::new(SqlWorkerPool::new(1, 8).unwrap(), 16 * 1024 * 1024, session).unwrap();
    let signer = PeerSigner::new(
        session,
        registry.release_digest(),
        SigningKey::from_bytes(&[221; 32]),
    );
    let lost = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let transport = DropPhaseReplies {
        verifier: Arc::new(PeerVerifier::new(
            session,
            registry.release_digest(),
            signer.verifying_key(),
        )),
        dispatcher: Arc::new(PeerDispatcher::new(
            registry.clone(),
            Arc::new(LocalRuntimePeerResolver {
                runtime: owner.runtime(),
                layout: layout.clone(),
            }),
            Arc::new(TestPeerAuthorizer),
        )),
        lost: lost.clone(),
        enabled: 15,
    };
    let client = CellClient::runtime_with_peer(
        registry,
        runtime.clone(),
        layout,
        Arc::new(signer),
        PeerPrincipal {
            issuer: "admission-test".into(),
            subject: "admission".into(),
            actions: vec!["beyonddb.cell.invoke".into()],
        },
        Arc::new(transport),
    );
    let storage = CellStorage::new(client.clone(), "us-east-1")
        .with_transaction_coordinators(provisioner.clone());
    let maps = ExpressionMaps::default();
    let item = Item::from([("id".into(), AttributeValue::S("lost-admission".into()))]);
    let ops = infos
        .iter()
        .map(|info| TransactWriteOp::Put {
            key_info: info,
            item: &item,
            condition: None,
            maps: &maps,
            return_values_on_ccf: Default::default(),
            stream: None,
        })
        .collect::<Vec<_>>();
    let token = |fingerprint| IdempotencyKey {
        account_id: "123456789012",
        token: "lost-public-phases",
        fingerprint,
    };
    storage
        .transact_write_items(&ops, Some(token("both-items")))
        .await
        .unwrap();
    assert_eq!(
        lost.load(Ordering::SeqCst),
        15,
        "BEGIN, prepare, decision and resolution replies were dropped after dispatch"
    );
    for info in &infos {
        assert_eq!(
            storage.get_item(info, &item).await.unwrap(),
            Some(item.clone())
        );
    }
    assert!(matches!(
        storage
            .transact_write_items(&ops, Some(token("both-items")))
            .await,
        Err(StorageError::IdempotentReplay)
    ));
    assert!(matches!(
        storage
            .transact_write_items(&ops, Some(token("changed")))
            .await,
        Err(StorageError::IdempotentMismatch)
    ));

    let new_item = Item::from([("id".into(), AttributeValue::S("canceled-retry".into()))]);
    let not_exists = Expr::Function {
        name: "attribute_not_exists".into(),
        args: vec![Expr::Path(vec![PathElement::Attribute("id".into())])],
    };
    let retry_ops = [
        TransactWriteOp::Put {
            key_info: &infos[0],
            item: &new_item,
            condition: None,
            maps: &maps,
            return_values_on_ccf: Default::default(),
            stream: None,
        },
        TransactWriteOp::Put {
            key_info: &infos[1],
            item: &item,
            condition: Some(&not_exists),
            maps: &maps,
            return_values_on_ccf: ReturnValuesOnConditionCheckFailure::AllOld,
            stream: None,
        },
    ];
    let retry_token = || IdempotencyKey {
        account_id: "123456789012",
        token: "canceled-public-phases",
        fingerprint: "retry-conditions",
    };
    assert!(
        matches!(storage.transact_write_items(&retry_ops, Some(retry_token())).await, Err(StorageError::TransactionCanceled(reasons)) if reasons[1].code == "ConditionalCheckFailed" && reasons[1].item == Some(item.clone()))
    );
    assert_eq!(storage.get_item(&infos[0], &new_item).await.unwrap(), None);
    storage
        .delete_item(&infos[1], &item, false, None, &maps, None)
        .await
        .unwrap();
    storage
        .transact_write_items(&retry_ops, Some(retry_token()))
        .await
        .unwrap();
    assert_eq!(
        storage.get_item(&infos[0], &new_item).await.unwrap(),
        Some(new_item.clone())
    );
    assert_eq!(
        storage.get_item(&infos[1], &item).await.unwrap(),
        Some(item.clone())
    );
    let absent = Item::from([("id".into(), AttributeValue::S("absent-read".into()))]);
    lost.store(0, Ordering::SeqCst);
    let read = storage
        .transact_get_items(&[
            TransactGetOp {
                key_info: &infos[1],
                key: &item,
            },
            TransactGetOp {
                key_info: &infos[0],
                key: &new_item,
            },
            TransactGetOp {
                key_info: &infos[0],
                key: &absent,
            },
            TransactGetOp {
                key_info: &infos[1],
                key: &item,
            },
        ])
        .await
        .unwrap();
    assert_eq!(
        read,
        vec![Some(item.clone()), Some(new_item), None, Some(item.clone())]
    );
    assert_eq!(lost.load(Ordering::SeqCst), 15);
    super::transaction_reads::assert_shared_snapshots(
        &client,
        &storage,
        &provisioner,
        &infos,
        &item,
    )
    .await;
    runtime.shutdown().await.unwrap();
}
