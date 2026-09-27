use super::global_indexes::{ACCOUNT, mutation, owner};
use crate::*;
use beyonddb::{
    BeginDirectoryTransfer, DirectoryInstall, DirectoryPartitionInput, DirectorySpec,
    DirectoryTransfer, FinishDirectoryTransfer, FreezeDirectory, InstallDirectory,
    PublishDirectoryTransfer, ReadDirectory, ReadDirectoryRange, ReadDirectoryTransfer,
    directory_target,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_transfer_retains_exact_contract_through_publication_and_owner_restart() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "base-directory-transfer".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let files = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("base-directory-transfer"),
        *account.application().as_bytes(),
    );
    let (mut host, provisioner, mut client, storage) = owner(
        &application,
        &layout,
        SessionId::from_bytes([91; 16]),
        &files.path().join("initial"),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    storage
        .create_table(
            ACCOUNT,
            serde_json::from_value(serde_json::json!({
                "TableName": "DirectoryTransfers", "BillingMode": "PAY_PER_REQUEST",
                "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
                "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}]
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let table = client
        .query::<DescribeTable>(&account, None, Json("DirectoryTransfers".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let route = client
        .query::<ReadTableRoute>(&account, None, Json(table.id.clone()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let spec = DirectorySpec::root(table.id.clone());
    let directory = directory_target(ACCOUNT, &spec).unwrap();
    provisioner.admit_directory(ACCOUNT, &spec).await.unwrap();
    let range = |part: &PartitionSpec| beyonddb::RoutePagePartition {
        partition_id: part.partition_id,
        lower: part.lower.unwrap_or([0; 16]),
        upper: part.upper,
        epoch: part.epoch,
    };
    client
        .command::<InstallDirectory>(
            &directory,
            mutation(),
            Json(DirectoryInstall {
                spec: spec.clone(),
                ranges: route.partitions.iter().map(range).collect(),
                source: None,
            }),
        )
        .await
        .unwrap();
    let source = route.partitions[0].clone();
    let boundary = (u128::from_be_bytes(source.upper.unwrap()) / 2).to_be_bytes();
    let mut children = [source.clone(), source.clone()];
    children[0].partition_id = [31; 16];
    children[0].upper = Some(boundary);
    children[1].partition_id = [32; 16];
    children[1].lower = Some(boundary);
    for child in &mut children {
        child.epoch = source.epoch + 1;
    }
    let plan = SplitPlan { source, children };
    let transfer = DirectoryTransfer::from(plan.clone());
    client
        .command::<BeginDirectoryTransfer>(&directory, mutation(), Json(transfer.clone()))
        .await
        .unwrap();
    // Identical compact ranges cannot replace the full immutable table contract.
    let mut changed = plan.clone();
    changed.source.table.table_name = "AnotherContract".into();
    for child in &mut changed.children {
        child.table = changed.source.table.clone();
    }
    assert!(matches!(
        client
            .command::<BeginDirectoryTransfer>(&directory, mutation(), Json(changed.into()))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    let mut wrong_generation = plan.clone();
    let mut other_id = *blake3::Hash::from_hex(&table.id).unwrap().as_bytes();
    other_id[31] ^= 1;
    wrong_generation.source.table.id = blake3::Hash::from_bytes(other_id).to_hex().to_string();
    for child in &mut wrong_generation.children {
        child.table = wrong_generation.source.table.clone();
    }
    assert!(matches!(
        client
            .command::<BeginDirectoryTransfer>(
                &directory,
                mutation(),
                Json(wrong_generation.into())
            )
            .await,
        Err(InvocationError::Rejected(_))
    ));
    for published in [false, true] {
        host.shutdown().await.unwrap();
        let (next_host, next_provisioner, next_client, _) = owner(
            &application,
            &layout,
            SessionId::from_bytes([92 + u8::from(published); 16]),
            &files.path().join(format!("restored-{published}")),
        );
        next_provisioner
            .admit_existing_directory(ACCOUNT, &spec)
            .await
            .unwrap();
        host = next_host;
        client = next_client;
        // Recovery must find the original contract through any participant,
        // including after its source has disappeared from published membership.
        for part in [&plan.source, &plan.children[0], &plan.children[1]] {
            let input = DirectoryPartitionInput {
                table_id: table.id.clone(),
                partition_id: part.partition_id,
            };
            let recovered = client
                .query::<ReadDirectoryTransfer>(&directory, None, Json(input.clone()))
                .await
                .unwrap()
                .output
                .0;
            assert_eq!(recovered, Some(transfer.clone()));
            let member = client
                .query::<ReadDirectoryRange>(&directory, None, Json(input))
                .await
                .unwrap()
                .output
                .0;
            let is_child = part.partition_id != plan.source.partition_id;
            assert_eq!(member, (published == is_child).then(|| range(part)));
        }
        let version = client
            .query::<ReadDirectory>(&directory, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .version;
        assert!(matches!(
            client
                .command::<FreezeDirectory>(&directory, mutation(), Json(version))
                .await,
            Err(InvocationError::Rejected(_))
        ));
        if published {
            client
                .command::<FinishDirectoryTransfer>(&directory, mutation(), Json(transfer.clone()))
                .await
                .unwrap();
        } else {
            client
                .command::<PublishDirectoryTransfer>(&directory, mutation(), Json(transfer.clone()))
                .await
                .unwrap();
        }
    }
    assert!(
        client
            .query::<ReadDirectoryTransfer>(
                &directory,
                None,
                Json(DirectoryPartitionInput {
                    table_id: table.id,
                    partition_id: plan.children[1].partition_id,
                })
            )
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    assert!(matches!(
        client
            .command::<PublishDirectoryTransfer>(&directory, mutation(), Json(transfer))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    assert!(
        client
            .command::<FreezeDirectory>(&directory, mutation(), Json(2))
            .await
            .unwrap()
            .output
            .0
            .is_some()
    );
    host.shutdown().await.unwrap();
}
