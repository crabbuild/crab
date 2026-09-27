use super::global_indexes::{ACCOUNT, mutation, owner};
use crate::*;
use beyonddb::{ActivateGlobalIndexRoute, GlobalIndexRoute, RoutePagePartition};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activation_replay_compares_large_base_and_index_directories() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "directory-activation".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("directory-activation"),
        *account.application().as_bytes(),
    );
    let (host, provisioner, client, _) = owner(
        &application,
        &layout,
        SessionId::from_bytes([89; 16]),
        directory.path(),
    );
    let account_handle = provisioner.admit_account(ACCOUNT).await.unwrap();
    let spec = TableSpec {
        placement: beyonddb::TablePlacement::Routed {
            initial_partitions: 256,
        },
        local_secondary_indexes: Vec::new(),
        global_secondary_indexes: vec![
            serde_json::from_value(serde_json::json!({
                "IndexName": "ByGroup",
                "KeySchema": [{"AttributeName": "group", "KeyType": "HASH"}],
                "Projection": {"ProjectionType": "ALL"}
            }))
            .unwrap(),
        ],
        table_name: "ReplayDirectory".into(),
        key_schema: vec![KeySchemaElement {
            attribute_name: "id".into(),
            key_type: KeyType::Hash,
        }],
        attribute_definitions: ["id", "group"]
            .into_iter()
            .map(|name| AttributeDefinition {
                attribute_name: name.into(),
                attribute_type: ScalarAttributeType::S,
            })
            .collect(),
        billing_mode: BillingMode::PayPerRequest,
        provisioned_throughput: None,
        deletion_protection_enabled: false,
        initial_tags: Vec::new(),
        resource_arn: None,
    };
    let table = match client
        .command::<CreateTable>(&account, mutation(), Json(spec))
        .await
        .unwrap()
        .output
        .0
    {
        CreateTableOutcome::Created(table) => table,
        other => panic!("unexpected creation: {other:?}"),
    };
    let width = u128::MAX / 1_024;
    let route = GlobalIndexRoute {
        index: table.global_secondary_indexes[0].clone(),
        table,
        partitions: (0_u128..1_024)
            .map(|position| RoutePagePartition {
                partition_id: position.to_be_bytes(),
                lower: (position * width).to_be_bytes(),
                upper: (position < 1_023).then(|| ((position + 1) * width).to_be_bytes()),
                epoch: 1,
            })
            .collect(),
    };
    client
        .command::<ActivateGlobalIndexRoute>(&account, mutation(), Json(route.clone()))
        .await
        .unwrap();
    let base = TableRoute {
        table_id: route.table.id.clone(),
        epoch: 1,
        partitions: route
            .partitions
            .iter()
            .map(|range| PartitionSpec {
                table: route.table.clone(),
                partition_id: range.partition_id,
                lower: (range.lower != [0; 16]).then_some(range.lower),
                upper: range.upper,
                epoch: range.epoch,
            })
            .collect(),
    };
    client
        .command::<ActivateTableRoute>(&account, mutation(), Json(base.clone()))
        .await
        .unwrap();
    account_handle.drain().await.unwrap();
    provisioner.admit_account(ACCOUNT).await.unwrap();
    assert_eq!(
        client
            .command::<ActivateTableRoute>(&account, mutation(), Json(base.clone()))
            .await
            .unwrap()
            .output
            .0,
        ActivateTableRouteOutcome::Activated
    );
    assert!(
        client
            .command::<ActivateGlobalIndexRoute>(&account, mutation(), Json(route.clone()))
            .await
            .unwrap()
            .output
            .0
    );
    // Same-sized, still contiguous proposals must compare all pages, not just
    // the first page or directory size. IDs and range bounds both matter.
    for position in [0, 63, 64, 1_023] {
        let mut changed = route.clone();
        changed.partitions[position].partition_id = [0xf0; 16];
        let rejected = client
            .command::<ActivateGlobalIndexRoute>(&account, mutation(), Json(changed))
            .await;
        assert!(matches!(rejected, Err(InvocationError::Rejected(result)) if !result.output.0));
        let mut changed = base.clone();
        changed.partitions[position].partition_id = [0xf0; 16];
        let rejected = client
            .command::<ActivateTableRoute>(&account, mutation(), Json(changed))
            .await;
        assert!(
            matches!(rejected, Err(InvocationError::Rejected(result)) if result.output.0 == ActivateTableRouteOutcome::AlreadyActive)
        );
    }
    let mut shortened = route.clone();
    shortened.partitions.pop();
    shortened.partitions.last_mut().unwrap().upper = None;
    let rejected = client
        .command::<ActivateGlobalIndexRoute>(&account, mutation(), Json(shortened))
        .await;
    assert!(matches!(rejected, Err(InvocationError::Rejected(result)) if !result.output.0));
    let mut shortened = base.clone();
    shortened.partitions.pop();
    shortened.partitions.last_mut().unwrap().upper = None;
    let rejected = client
        .command::<ActivateTableRoute>(&account, mutation(), Json(shortened))
        .await;
    assert!(
        matches!(rejected, Err(InvocationError::Rejected(result)) if result.output.0 == ActivateTableRouteOutcome::AlreadyActive)
    );
    host.shutdown().await.unwrap();
}
