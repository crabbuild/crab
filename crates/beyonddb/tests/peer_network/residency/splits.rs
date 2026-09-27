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
