use std::time::Duration;

use super::*;
use crab_cell_runtime::{
    cell::catalog::CellCatalog,
    control::{ControlState, Owner, Transition},
    identity::CellTarget,
};

const ACCOUNT: &str = "123456789012";
const ACCESS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";

async fn interrupt_acquisition(fixture: &Fixture, handle: CellHandle) {
    let cell = handle.cell_id();
    handle.drain().await.unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let observed = authority.load(cell).await.unwrap().unwrap();
    let claimed = observed
        .value()
        .takeover(Owner {
            session: fixture.session,
            endpoint: fixture.endpoint.clone(),
        })
        .unwrap();
    authority
        .transition(&observed, claimed, Transition::Takeover)
        .await
        .unwrap();
}

async fn assert_locally_serving(fixture: &Fixture, target: &CellTarget) {
    let control = CellAuthority::new(fixture.layout.clone())
        .load(target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(control.value().state, ControlState::Serving);
    let proof = CellCatalog::new(fixture.layout.clone(), target.tenant())
        .lookup(target.cell_id())
        .await
        .unwrap()
        .unwrap();
    assert!(
        fixture
            .node
            .runtime()
            .local_handle(proof, &control)
            .await
            .unwrap()
            .is_some(),
        "recovery must finish activation before reporting success"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_recovery_resumes_claimed_roots_before_sdk_requests() {
    let fixture = Fixture::new().await;
    let provisioner = &fixture.provisioner;
    let coordinator = provisioner
        .admit_coordinator(ACCOUNT, b"claimed-root")
        .await
        .unwrap();
    let credential = provisioner.admit_credential(ACCESS_KEY).await.unwrap();
    let account = provisioner.admit_account(ACCOUNT).await.unwrap();
    for handle in [account, credential, coordinator] {
        interrupt_acquisition(&fixture, handle).await;
    }
    provisioner
        .recover_owned_account(ACCOUNT, &fixture.directory)
        .await
        .unwrap();
    provisioner
        .recover_owned_credential(ACCESS_KEY, &fixture.directory)
        .await
        .unwrap();
    provisioner
        .recover_owned_coordinator(ACCOUNT, b"claimed-root", &fixture.directory)
        .await
        .unwrap();
    for target in [
        account_target(ACCOUNT).unwrap(),
        beyonddb::credential_target(ACCESS_KEY).unwrap(),
        beyonddb::coordinator_target(ACCOUNT, b"claimed-root").unwrap(),
    ] {
        assert_locally_serving(&fixture, &target).await;
    }
    let item = &fixture.data[0].1;
    assert_eq!(
        fixture
            .sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap()
            .item
            .as_ref(),
        Some(item)
    );
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovered_recovery_resumes_claimed_ranges_before_sdk_requests() {
    let fixture = Fixture::with_partition_count(1).await;
    super::reclamation::create(&fixture.sdk, "ResidencyClaimedIndex", true).await;
    let account_target = account_target(ACCOUNT).unwrap();
    let table = fixture
        .client
        .query::<DescribeTable>(&account_target, None, Json("ResidencyClaimedIndex".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let source = beyonddb::data_target(ACCOUNT, &table.id, &[0; 16]).unwrap();
    let item = SdkItem::from([("id".into(), AwsAttributeValue::S("indexed".into()))]);
    fixture
        .sdk
        .put_item()
        .table_name("ResidencyClaimedIndex")
        .set_item(Some(item.clone()))
        .send()
        .await
        .unwrap();
    beyonddb::CellStorage::new(fixture.client.clone(), "us-east-1")
        .project_index_changes(ACCOUNT, &source, &table.id)
        .await
        .unwrap();
    fixture
        .provisioner
        .admit_coordinator(ACCOUNT, b"claimed-discovery")
        .await
        .unwrap();
    let mut targets = Vec::new();
    for entry in fixture
        .node
        .runtime()
        .active_catalog_entries()
        .await
        .unwrap()
    {
        if ![
            source.namespace(),
            beyonddb::global_index_target(ACCOUNT, &table.global_secondary_indexes[0].id, &[0; 16])
                .unwrap()
                .namespace(),
            beyonddb::coordinator_target(ACCOUNT, b"claimed-discovery")
                .unwrap()
                .namespace(),
        ]
        .contains(&entry.namespace())
        {
            continue;
        }
        let target = CellTarget::new(
            account_target.tenant(),
            account_target.application(),
            entry.namespace(),
            entry.partition(),
        )
        .unwrap();
        let proof = CellCatalog::new(fixture.layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let control = CellAuthority::new(fixture.layout.clone())
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let handle = fixture
            .node
            .runtime()
            .local_handle(proof, &control)
            .await
            .unwrap()
            .unwrap();
        interrupt_acquisition(&fixture, handle).await;
        targets.push(target);
    }
    assert_eq!(targets.len(), 4);
    let account = fixture.provisioner.admit_account(ACCOUNT).await.unwrap();
    fixture
        .provisioner
        .recover_registered_partitions(ACCOUNT, account, &fixture.client, &fixture.directory)
        .await
        .unwrap();
    // A local-only client cannot rescue a skipped activation through SDK routing.
    let local = CellClient::local_runtime(
        fixture.application.registry(),
        fixture.node.runtime(),
        fixture.layout.clone(),
    );
    let storage = beyonddb::CellStorage::new(local.clone(), "us-east-1");
    fixture
        .provisioner
        .recover_registered_coordinators(ACCOUNT, &local, &storage, &fixture.directory)
        .await
        .unwrap();
    for target in &targets {
        assert_locally_serving(&fixture, target).await;
    }
    assert_eq!(
        fixture
            .sdk
            .scan()
            .table_name("ResidencyClaimedIndex")
            .index_name("ById")
            .send()
            .await
            .unwrap()
            .items,
        Some(vec![item])
    );
    for (_, item) in &fixture.data {
        assert_eq!(
            fixture
                .sdk
                .get_item()
                .table_name("Residency")
                .key("id", item["id"].clone())
                .consistent_read(true)
                .send()
                .await
                .unwrap()
                .item
                .as_ref(),
            Some(item)
        );
    }
    fixture.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_restoration_releases_a_settled_range_without_losing_items() {
    let fixture = Fixture::new().await;
    let mut blockers = Vec::new();
    while fixture.node.runtime().stats().active_cells()
        < fixture.node.runtime().stats().active_cell_capacity()
    {
        blockers.push(
            fixture
                .provisioner
                .admit_credential(&format!("AKIAMETADATACAPACITY{}", blockers.len()))
                .await
                .unwrap(),
        );
    }
    fixture
        .provisioner
        .admit_account(ACCOUNT)
        .await
        .unwrap()
        .drain()
        .await
        .unwrap();
    blockers.push(
        fixture
            .provisioner
            .admit_credential("AKIAMETADATAOCCUPIED")
            .await
            .unwrap(),
    );
    let ranges: Vec<_> = fixture
        .data
        .iter()
        .map(|(handle, _)| handle.cell_id())
        .collect();
    assert!(!ranges.is_empty());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture
                .node
                .runtime()
                .idle_transfer_candidates()
                .await
                .unwrap()
                .iter()
                .any(|(cell, _, _, _)| ranges.contains(cell))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    fixture.provisioner.admit_account(ACCOUNT).await.unwrap();
    let authority = CellAuthority::new(fixture.layout.clone());
    let mut released = 0;
    for cell in ranges {
        let control = authority.load(cell).await.unwrap().unwrap();
        if control.value().owner.is_none() {
            assert!(control.value().root.is_some());
            released += 1;
        }
    }
    assert_eq!(released, 1);
    for blocker in blockers {
        blocker.drain().await.unwrap();
    }
    for (_, item) in &fixture.data {
        let restored = fixture
            .sdk
            .get_item()
            .table_name("Residency")
            .key("id", item["id"].clone())
            .consistent_read(true)
            .send()
            .await
            .unwrap();
        assert_eq!(restored.item.as_ref(), Some(item));
    }
    fixture.shutdown().await;
}
