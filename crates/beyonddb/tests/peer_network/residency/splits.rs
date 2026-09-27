use super::*;
use beyonddb::{
    BeginSplit, BeginSplitOutcome, PublishedPartitionInput, ReadRoutePage, ReadSourceSplitPlan,
    ReadSplitPlan, RoutePageInput, RoutePageOutcome, SplitPlan,
};
use crab_cell_runtime::client::InvocationError;
use crab_cell_runtime::{MutationIdentity, identity::RequestId};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_independent_range_splits_preserve_progress_and_replay() {
    let fixture = Fixture::new().await;
    let (route, plans) = pending_splits(&fixture).await;
    let account = account_target("123456789012").unwrap();
    let table = route.partitions[0].table.clone();
    let mut competing = plans[0].clone();
    competing.children[0].partition_id = [120; 16];
    let conflict = fixture
        .client
        .command::<BeginSplit>(
            &account,
            MutationIdentity {
                request_id: RequestId::from_bytes([119; 16]),
                issued_at_ms: now_ms(),
                expires_at_ms: now_ms() + 60_000,
            },
            Json(competing),
        )
        .await;
    assert!(
        matches!(conflict, Err(InvocationError::Rejected(result)) if result.output.0 == BeginSplitOutcome::Conflict)
    );
    fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    for plan in &plans {
        let recovered = fixture
            .client
            .query::<ReadSourceSplitPlan>(
                &account,
                None,
                Json(PublishedPartitionInput {
                    table_id: table.id.clone(),
                    partition_id: plan.source.partition_id,
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        assert_eq!(recovered.as_ref(), Some(plan));
    }
    // Publish one range while the other plan is still pending. Replaying the
    // first concurrently with the second protects both sides of the cutover.
    fixture
        .provisioner
        .split_if_over_database_bytes(
            "123456789012",
            fixture.client.clone(),
            &table.id,
            plans[0].source.partition_id,
            u64::MAX,
        )
        .await
        .unwrap();
    let pending = fixture
        .client
        .query::<ReadSplitPlan>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(pending, Some(plans[1].clone()));
    let (replay, second) = tokio::join!(
        fixture
            .provisioner
            .resume_split("123456789012", fixture.client.clone(), &plans[0]),
        fixture.provisioner.split_partition(
            "123456789012",
            fixture.client.clone(),
            &table.id,
            plans[1].source.partition_id
        ),
    );
    replay.unwrap();
    assert_eq!(second.unwrap(), plans[1]);
    for plan in &plans {
        fixture
            .provisioner
            .resume_split("123456789012", fixture.client.clone(), plan)
            .await
            .unwrap();
    }
    let published = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(published.epoch, route.epoch + 2);
    assert_eq!(
        published.partitions,
        plans
            .iter()
            .flat_map(|plan| plan.children.clone())
            .collect::<Vec<_>>()
    );
    let stale_page = fixture
        .client
        .query::<ReadRoutePage>(
            &account,
            None,
            Json(RoutePageInput {
                table_id: table.id.clone(),
                start_hash: None,
                after_lower: None,
                expected_epoch: Some(route.epoch + 1),
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(stale_page, RoutePageOutcome::Changed);
    assert!(
        fixture
            .client
            .query::<ReadSplitPlan>(&account, None, Json(table.id.clone()))
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    // Release every new range and the metadata owner; SDK reads must restore
    // the published directory and copied values from object storage.
    for range in &published.partitions {
        fixture
            .provisioner
            .admit_existing_partition("123456789012", &table.id, &range.partition_id)
            .await
            .unwrap()
            .drain()
            .await
            .unwrap();
    }
    fixture
        .provisioner
        .admit_account("123456789012")
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    for (_, item) in &fixture.data {
        let read = sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(read.item.as_ref(), Some(item));
    }
    fixture.shutdown().await;
}

async fn pending_splits(fixture: &Fixture) -> (TableRoute, Vec<SplitPlan>) {
    let account = account_target("123456789012").unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account, None, Json("Residency".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let route = fixture
        .client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let plans: Vec<_> = route
        .partitions
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let lower = u128::from_be_bytes(source.lower.unwrap_or([0; 16]));
            let upper = u128::from_be_bytes(source.upper.unwrap_or([0xff; 16]));
            let boundary = (lower + (upper - lower) / 2).to_be_bytes();
            let mut left = source.clone();
            left.partition_id = [101 + index as u8 * 2; 16];
            left.upper = Some(boundary);
            left.epoch = route.epoch + 1;
            let mut right = source.clone();
            right.partition_id = [102 + index as u8 * 2; 16];
            right.lower = Some(boundary);
            right.epoch = route.epoch + 1;
            SplitPlan {
                source: source.clone(),
                children: [left, right],
                expected_epoch: route.epoch,
            }
        })
        .collect();
    for (index, plan) in plans.iter().enumerate() {
        fixture
            .client
            .command::<BeginSplit>(
                &account,
                MutationIdentity {
                    request_id: RequestId::from_bytes([111 + index as u8; 16]),
                    issued_at_ms: now_ms(),
                    expires_at_ms: now_ms() + 60_000,
                },
                Json(plan.clone()),
            )
            .await
            .expect("disjoint ranges must retain independent split plans");
    }
    (route, plans)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_capacity_sweep_advances_past_a_transaction_blocked_split() {
    use beyonddb::{
        GetItemInput, ParticipantTransactionState, PreparePartitionTransaction,
        PreparePartitionTransactionInput, PrepareTransactionOutcome, ReadPartitionTransaction,
        ReadTransactionInput, ResolvePartitionTransaction, ResolveTransactionInput,
        TransactionOperation, data_target,
    };
    use std::time::Duration;

    let fixture = Fixture::new().await;
    let (route, plans) = pending_splits(&fixture).await;
    let table = &route.partitions[0].table;
    let account = account_target("123456789012").unwrap();
    let source = data_target("123456789012", &table.id, &plans[0].source.partition_id).unwrap();
    let identity = |byte| MutationIdentity {
        request_id: RequestId::from_bytes([byte; 16]),
        issued_at_ms: now_ms(),
        expires_at_ms: now_ms() + 60_000,
    };
    let key = Item::from([(
        "id".into(),
        AttributeValue::S(fixture.data[0].1["id"].as_s().unwrap().clone()),
    )]);
    let prepared = crate::transaction_command!(
        fixture.client,
        PreparePartitionTransaction,
        &source,
        identity(201),
        Json(PreparePartitionTransactionInput {
            table_id: table.id.clone(),
            epoch: plans[0].source.epoch,
            transaction_id: [201; 16],
            coordinator_cell: *account.cell_id().as_bytes(),
            coordinator_key: vec![201; 16],
            operations: vec![TransactionOperation::Read(GetItemInput {
                table_name: table.table_name.clone(),
                table_id: table.id.clone(),
                key,
            })],
        }),
    )
    .await
    .unwrap();
    assert_eq!(prepared.output.0, PrepareTransactionOutcome::Prepared);
    // A pending transaction keeps the first source unsealable. The supervised
    // sweep must publish the other range without aborting that transaction.
    fixture
        .provisioner
        .install_account_capacity_loop(
            &fixture.tasks,
            "123456789012".into(),
            fixture.client.clone(),
            u64::MAX,
            Duration::from_millis(20),
        )
        .unwrap();
    let partial = wait_for_ranges(&fixture, &table.id, 3).await;
    assert_eq!(partial.partitions[0], plans[0].source);
    assert_eq!(&partial.partitions[1..], &plans[1].children);
    assert_eq!(
        fixture
            .client
            .query::<ReadSourceSplitPlan>(
                &account,
                None,
                Json(PublishedPartitionInput {
                    table_id: table.id.clone(),
                    partition_id: plans[0].source.partition_id,
                })
            )
            .await
            .unwrap()
            .output
            .0,
        Some(plans[0].clone())
    );
    assert_eq!(
        fixture
            .client
            .query::<ReadPartitionTransaction>(
                &source,
                None,
                Json(ReadTransactionInput {
                    transaction_id: [201; 16],
                    coordinator_cell: *account.cell_id().as_bytes(),
                })
            )
            .await
            .unwrap()
            .output
            .0,
        ParticipantTransactionState::Prepared
    );
    fixture
        .client
        .command::<ResolvePartitionTransaction>(
            &source,
            identity(202),
            Json(ResolveTransactionInput {
                transaction_id: [201; 16],
                coordinator_cell: *account.cell_id().as_bytes(),
                commit: false,
            }),
        )
        .await
        .unwrap();
    let complete = wait_for_ranges(&fixture, &table.id, 4).await;
    assert_eq!(complete.epoch, route.epoch + 2);
    assert!(fixture.node.is_ready());
    let sdk = aws_sdk_dynamodb::Client::from_conf(
        fixture
            .sdk
            .config()
            .to_builder()
            .retry_config(aws_sdk_dynamodb::config::retry::RetryConfig::disabled())
            .build(),
    );
    for (_, item) in &fixture.data {
        let read = sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(read.item.as_ref(), Some(item));
    }
    fixture.shutdown().await;
}

async fn wait_for_ranges(fixture: &Fixture, table_id: &str, count: usize) -> TableRoute {
    use std::time::Duration;
    let account = account_target("123456789012").unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(fixture.node.is_ready(), "capacity sweep stopped serving");
            let route = fixture
                .client
                .query::<ReadTableRoute>(&account, None, Json(table_id.to_owned()))
                .await
                .unwrap()
                .output
                .0
                .unwrap();
            if route.partitions.len() == count {
                break route;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("capacity sweep did not progress past the blocked source")
}
