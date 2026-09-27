use super::global_indexes::{ACCOUNT, mutation, owner};
use crate::*;
use beyonddb::{
    BeginDirectoryChange, DirectoryChange, DirectoryCopyReceipt, DirectoryInstall, DirectoryMode,
    DirectoryPage, DirectoryPageInput, DirectorySpec, DirectorySplitPublication,
    FinishDirectoryChange, FreezeDirectory, InstallDirectory, PublishDirectoryChange,
    PublishDirectorySplit, ReadDirectory, ReadDirectoryPage, directory_target,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directory_tree_fences_copy_cutover_and_stale_leaf_writers() {
    for installed_children in 0..=2 {
        interrupted_split(installed_children).await;
    }
}

async fn interrupted_split(installed_children: usize) {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "directory-tree".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("directory-tree"),
        *account.application().as_bytes(),
    );
    let (host, provisioner, client, _) = owner(
        &application,
        &layout,
        SessionId::from_bytes([86; 16]),
        &directory.path().join("first"),
    );
    let mut table = [0; 32];
    table[..16].copy_from_slice(account.tenant().as_bytes());
    let root = DirectorySpec {
        table_id: blake3::Hash::from_bytes(table).to_hex().to_string(),
        node_id: [0; 16],
        lower: [0; 16],
        upper: None,
        depth: 0,
    };
    let target = directory_target(ACCOUNT, &root).unwrap();
    assert!(
        provisioner
            .admit_existing_directory(ACCOUNT, &root)
            .await
            .is_err()
    );
    provisioner.admit_directory(ACCOUNT, &root).await.unwrap();
    let width = u128::MAX / 1024;
    let ranges: Vec<_> = (0_u128..1024)
        .map(|position| beyonddb::RoutePagePartition {
            partition_id: position.to_be_bytes(),
            lower: (position * width).to_be_bytes(),
            upper: (position < 1023).then(|| ((position + 1) * width).to_be_bytes()),
            epoch: 1,
        })
        .collect();
    client
        .command::<InstallDirectory>(
            &target,
            mutation(),
            Json(DirectoryInstall {
                spec: root.clone(),
                ranges: ranges.clone(),
                source: None,
            }),
        )
        .await
        .unwrap();
    let page = client
        .query::<ReadDirectoryPage>(
            &target,
            None,
            Json(DirectoryPageInput {
                hash: [0; 16],
                expected_version: Some(1),
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert!(matches!(page,DirectoryPage::Leaf {ranges, ..} if ranges.len()==64));
    let plan = change(&ranges[0], 9000);
    assert!(
        matches!(
            client
                .command::<BeginDirectoryChange>(&target, mutation(), Json(plan.clone()))
                .await,
            Err(InvocationError::Rejected(_))
        ),
        "a full leaf must split before accepting another range reservation"
    );
    let split = client
        .command::<FreezeDirectory>(&target, mutation(), Json(1))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert!(matches!(
        client
            .command::<BeginDirectoryChange>(&target, mutation(), Json(plan.clone()))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    let mut copies = Vec::new();
    let mut receipts = Vec::new();
    for (index, spec) in split.children.iter().enumerate() {
        let child = directory_target(ACCOUNT, spec).unwrap();
        let rows = ranges[index * 512..(index + 1) * 512].to_vec();
        copies.push((child.clone(), rows.clone()));
        if index >= installed_children {
            continue;
        }
        provisioner.admit_directory(ACCOUNT, spec).await.unwrap();
        let mut corrupt = rows.clone();
        corrupt[4].partition_id = [200; 16];
        assert!(matches!(
            client
                .command::<InstallDirectory>(
                    &child,
                    mutation(),
                    Json(DirectoryInstall {
                        spec: spec.clone(),
                        ranges: corrupt,
                        source: Some(split.clone()),
                    })
                )
                .await,
            Err(InvocationError::Rejected(_))
        ));
        let installed = client
            .command::<InstallDirectory>(
                &child,
                mutation(),
                Json(DirectoryInstall {
                    spec: spec.clone(),
                    ranges: rows.clone(),
                    source: Some(split.clone()),
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            client
                .query::<ReadDirectoryPage>(
                    &child,
                    None,
                    Json(DirectoryPageInput {
                        hash: spec.lower,
                        expected_version: None
                    })
                )
                .await
                .unwrap()
                .output
                .0,
            DirectoryPage::Unavailable
        );
        receipts.push(DirectoryCopyReceipt {
            cell_id: *child.cell_id().as_bytes(),
            sequence: installed.receipt.commit_sequence,
            fingerprint: split.fingerprints[index],
        });
    }
    if installed_children == 2 {
        client
            .command::<PublishDirectorySplit>(
                &target,
                mutation(),
                Json(DirectorySplitPublication {
                    split: split.clone(),
                    receipts: receipts.try_into().unwrap(),
                }),
            )
            .await
            .unwrap();
    }
    host.shutdown().await.unwrap();
    // A new owner discovers the children from the durable parent after cutover;
    // copies remain closed until recovery completes their opening protocol.
    let (host, provisioner, client, _) = owner(
        &application,
        &layout,
        SessionId::from_bytes([87; 16]),
        &directory.path().join("restored"),
    );
    provisioner
        .admit_existing_directory(ACCOUNT, &root)
        .await
        .unwrap();
    let restored = client
        .query::<ReadDirectory>(&target, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(
        restored.mode,
        if installed_children == 2 {
            DirectoryMode::Branch(split.clone())
        } else {
            DirectoryMode::Frozen(split.clone())
        }
    );
    assert_eq!(
        provisioner
            .split_directory(&client, ACCOUNT, &root)
            .await
            .unwrap(),
        split
    );
    let state = client
        .query::<ReadDirectory>(&target, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(
        client
            .query::<ReadDirectoryPage>(
                &target,
                None,
                Json(DirectoryPageInput {
                    hash: [0; 16],
                    expected_version: Some(1)
                })
            )
            .await
            .unwrap()
            .output
            .0,
        DirectoryPage::Redirect(Box::new(split.clone()))
    );
    assert!(matches!(
        client
            .command::<PublishDirectoryChange>(&target, mutation(), Json(plan))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    // Different metadata leaves now accept independent changes; the parent
    // version is unchanged and no account metadata writer participates.
    let plans = [change(&copies[0].1[0], 9100), change(&copies[1].1[0], 9200)];
    let (left, right) = tokio::join!(
        client.command::<BeginDirectoryChange>(&copies[0].0, mutation(), Json(plans[0].clone())),
        client.command::<BeginDirectoryChange>(&copies[1].0, mutation(), Json(plans[1].clone()))
    );
    left.unwrap();
    right.unwrap();
    for (index, plan) in plans.iter().enumerate() {
        let child = &copies[index].0;
        assert!(
            matches!(
                client
                    .command::<FreezeDirectory>(child, mutation(), Json(split.version + 1))
                    .await,
                Err(InvocationError::Rejected(_))
            ),
            "pending data split must keep its recovery owner"
        );
        client
            .command::<PublishDirectoryChange>(child, mutation(), Json(plan.clone()))
            .await
            .unwrap();
        client
            .command::<FinishDirectoryChange>(child, mutation(), Json(plan.clone()))
            .await
            .unwrap();
        assert_eq!(
            client
                .query::<ReadDirectoryPage>(
                    child,
                    None,
                    Json(DirectoryPageInput {
                        hash: plan.source.lower,
                        expected_version: Some(split.version + 1)
                    })
                )
                .await
                .unwrap()
                .output
                .0,
            DirectoryPage::Changed
        );
        let page = client
            .query::<ReadDirectoryPage>(
                child,
                None,
                Json(DirectoryPageInput {
                    hash: plan.children[1].lower,
                    expected_version: Some(split.version + 2),
                }),
            )
            .await
            .unwrap()
            .output
            .0;
        assert!(matches!(page,DirectoryPage::Leaf {ranges, ..} if ranges[0]==plan.children[1]));
    }
    // A replayed install after writes must acknowledge the original copy without
    // rolling membership back. Its immutable birth digest authenticates replay.
    client
        .command::<InstallDirectory>(
            &copies[0].0,
            mutation(),
            Json(DirectoryInstall {
                spec: split.children[0].clone(),
                ranges: copies[0].1.clone(),
                source: Some(split.clone()),
            }),
        )
        .await
        .unwrap();
    let replayed = client
        .query::<ReadDirectory>(&copies[0].0, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert_eq!(replayed.version, split.version + 2);
    let grandchildren = provisioner
        .split_directory(&client, ACCOUNT, &split.children[0])
        .await
        .unwrap();
    assert_eq!(grandchildren.children[0].depth, 2);
    assert_eq!(
        provisioner
            .split_directory(&client, ACCOUNT, &split.children[0])
            .await
            .unwrap(),
        grandchildren
    );
    assert_eq!(
        client
            .query::<ReadDirectory>(&target, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .unwrap(),
        state
    );
    // Retirement can interrupt a pending data split and a frozen metadata copy.
    // Its terminal fence must prevent either stale publisher from proceeding.
    let pending = change(&plans[1].children[0], 9500);
    client
        .command::<BeginDirectoryChange>(&copies[1].0, mutation(), Json(pending.clone()))
        .await
        .unwrap();
    let grandchild = directory_target(ACCOUNT, &grandchildren.children[0]).unwrap();
    let frozen = client
        .command::<FreezeDirectory>(&grandchild, mutation(), Json(grandchildren.version + 1))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let mut unfinished_copies = Vec::new();
    for spec in &frozen.children {
        let mut rows = Vec::new();
        let mut hash = spec.lower;
        loop {
            let page = client
                .query::<ReadDirectoryPage>(
                    &grandchild,
                    None,
                    Json(DirectoryPageInput {
                        hash,
                        expected_version: Some(frozen.version),
                    }),
                )
                .await
                .unwrap()
                .output
                .0;
            let DirectoryPage::Leaf { ranges, .. } = page else {
                panic!("frozen copy lost its readable source");
            };
            rows.extend(
                ranges
                    .into_iter()
                    .take_while(|range| spec.upper.is_none_or(|upper| range.lower < upper)),
            );
            let next = rows.last().unwrap().upper;
            if next == spec.upper {
                break;
            }
            hash = next.unwrap();
        }
        unfinished_copies.push(DirectoryInstall {
            spec: spec.clone(),
            ranges: rows,
            source: Some(frozen.clone()),
        });
    }
    // Interrupt deletion with zero, one or both unpublished child copies durable.
    for copy in unfinished_copies.iter().take(installed_children) {
        provisioner
            .admit_directory(ACCOUNT, &copy.spec)
            .await
            .unwrap();
        let target = directory_target(ACCOUNT, &copy.spec).unwrap();
        client
            .command::<InstallDirectory>(&target, mutation(), Json(copy.clone()))
            .await
            .unwrap();
    }
    let mut other_generation = root.clone();
    other_generation.table_id.replace_range(63..64, "1");
    assert!(matches!(
        client
            .command::<beyonddb::RetireDirectory>(&target, mutation(), Json(other_generation))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    client
        .command::<beyonddb::RetireDirectory>(&target, mutation(), Json(root.clone()))
        .await
        .unwrap();
    assert_eq!(
        client
            .query::<ReadDirectoryPage>(
                &target,
                None,
                Json(DirectoryPageInput {
                    hash: [0; 16],
                    expected_version: None,
                })
            )
            .await
            .unwrap()
            .output
            .0,
        DirectoryPage::Unavailable
    );
    assert!(matches!(
        client
            .command::<beyonddb::RecordDirectoryRetirement>(
                &target,
                mutation(),
                Json(beyonddb::DirectoryRetirementReceipt {
                    parent: root.clone(),
                    child_id: [0xff; 16],
                    sequence: 1,
                })
            )
            .await,
        Err(InvocationError::Rejected(_))
    ));
    client
        .command::<beyonddb::RetireDirectory>(
            &copies[0].0,
            mutation(),
            Json(split.children[0].clone()),
        )
        .await
        .unwrap();
    client
        .command::<beyonddb::RetireDirectory>(
            &grandchild,
            mutation(),
            Json(grandchildren.children[0].clone()),
        )
        .await
        .unwrap();
    // Lose the grandchild's retirement receipt before its parent acknowledges it.
    host.shutdown().await.unwrap();
    let (unavailable, remote_provisioner, _, _) = owner(
        &application,
        &layout,
        SessionId::from_bytes([88; 16]),
        &directory.path().join("unavailable"),
    );
    remote_provisioner
        .admit_existing_directory(ACCOUNT, &split.children[1])
        .await
        .unwrap();
    let (host, provisioner, client, _) = owner(
        &application,
        &layout,
        SessionId::from_bytes([89; 16]),
        &directory.path().join("retiring"),
    );
    // The other child is still owned elsewhere. A failed restoration must leave
    // the root pending, even after all reachable descendants are retired.
    let mut saw_unavailable = false;
    for _ in 0..8 {
        match provisioner
            .retire_directory_step(&client, ACCOUNT, &root)
            .await
        {
            Ok(false) => {}
            Ok(true) => panic!("retirement skipped an unavailable published child"),
            Err(_) => {
                saw_unavailable = true;
                break;
            }
        }
    }
    assert!(saw_unavailable);
    let retiring = client
        .query::<ReadDirectory>(&target, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    assert!(matches!(
        retiring.mode,
        DirectoryMode::Retiring {
            acknowledged: 1,
            ..
        }
    ));
    unavailable.shutdown().await.unwrap();
    assert!(
        provisioner
            .retire_directory_step(&client, ACCOUNT, &root)
            .await
            .unwrap()
    );
    assert!(
        provisioner
            .retire_directory_step(&client, ACCOUNT, &root)
            .await
            .unwrap()
    );
    for spec in std::iter::once(&root)
        .chain(split.children.iter())
        .chain(grandchildren.children.iter())
        .chain(frozen.children.iter())
    {
        let target = directory_target(ACCOUNT, spec).unwrap();
        assert_eq!(
            client
                .query::<ReadDirectory>(&target, None, Json(()))
                .await
                .unwrap()
                .output
                .0
                .unwrap()
                .mode,
            DirectoryMode::Retired
        );
    }
    assert!(matches!(
        client
            .command::<InstallDirectory>(
                &target,
                mutation(),
                Json(DirectoryInstall {
                    spec: root.clone(),
                    ranges: ranges.clone(),
                    source: None,
                })
            )
            .await,
        Err(InvocationError::Rejected(_))
    ));
    assert!(matches!(
        client
            .command::<beyonddb::OpenDirectory>(&copies[1].0, mutation(), Json(split.clone()))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    assert!(matches!(
        client
            .command::<PublishDirectoryChange>(&copies[1].0, mutation(), Json(pending))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    assert!(
        client
            .query::<beyonddb::ReadDirectoryChanges>(&copies[1].0, None, Json(None))
            .await
            .unwrap()
            .output
            .0
            .is_empty()
    );
    assert!(matches!(
        client
            .command::<FreezeDirectory>(&grandchild, mutation(), Json(frozen.version))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    for copy in unfinished_copies {
        let target = directory_target(ACCOUNT, &copy.spec).unwrap();
        assert!(matches!(
            client
                .command::<InstallDirectory>(&target, mutation(), Json(copy))
                .await,
            Err(InvocationError::Rejected(_))
        ));
        assert!(matches!(
            client
                .command::<beyonddb::OpenDirectory>(&target, mutation(), Json(frozen.clone()))
                .await,
            Err(InvocationError::Rejected(_))
        ));
    }
    assert!(
        provisioner
            .split_directory(&client, ACCOUNT, &root)
            .await
            .is_err()
    );
    host.shutdown().await.unwrap();
}

fn change(source: &beyonddb::RoutePagePartition, id: u128) -> DirectoryChange {
    let lower = u128::from_be_bytes(source.lower);
    let upper = source.upper.map(u128::from_be_bytes).unwrap_or(u128::MAX);
    let boundary = (lower + (upper - lower) / 2).to_be_bytes();
    DirectoryChange {
        source: source.clone(),
        children: [
            beyonddb::RoutePagePartition {
                partition_id: id.to_be_bytes(),
                lower: source.lower,
                upper: Some(boundary),
                epoch: source.epoch + 1,
            },
            beyonddb::RoutePagePartition {
                partition_id: (id + 1).to_be_bytes(),
                lower: boundary,
                upper: source.upper,
                epoch: source.epoch + 1,
            },
        ],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_directories_release_residency_without_losing_retirement_fences() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "directory-residency".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("directory-residency"),
        *account.application().as_bytes(),
    );
    let (host, provisioner, client, _) = owner(
        &application,
        &layout,
        SessionId::from_bytes([85; 16]),
        directory.path(),
    );
    let ranges = vec![beyonddb::RoutePagePartition {
        partition_id: [0; 16],
        lower: [0; 16],
        upper: None,
        epoch: 1,
    }];
    let mut directories = Vec::new();
    for ordinal in 0..16u8 {
        let mut id = [ordinal; 32];
        id[..16].copy_from_slice(account.tenant().as_bytes());
        let spec = DirectorySpec::root(blake3::Hash::from_bytes(id).to_hex().to_string());
        let target = directory_target(ACCOUNT, &spec).unwrap();
        provisioner.admit_directory(ACCOUNT, &spec).await.unwrap();
        client
            .command::<InstallDirectory>(
                &target,
                mutation(),
                Json(DirectoryInstall {
                    spec: spec.clone(),
                    ranges: ranges.clone(),
                    source: None,
                }),
            )
            .await
            .unwrap();
        directories.push((spec, target));
    }
    assert_eq!(host.runtime().stats().active_cells(), 16);
    let authority = CellAuthority::new(layout);
    assert!(provisioner.admit_account(ACCOUNT).await.is_err());
    assert!(
        authority.load(account.cell_id()).await.unwrap().is_none(),
        "capacity rejection must precede a new ownership claim"
    );
    // Keep one live node at the same capacity boundary. Only an irreversible
    // terminal fence, never ordinary inactivity, authorizes reclamation.
    for (spec, target) in &directories[1..] {
        client
            .command::<beyonddb::RetireDirectory>(target, mutation(), Json(spec.clone()))
            .await
            .unwrap();
    }
    // Durable retirement precedes the runtime's settled inventory refresh.
    // Reclamation may release only candidates that have crossed both gates.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host
                .runtime()
                .idle_transfer_candidates()
                .await
                .unwrap()
                .iter()
                .any(|(cell, _, _, _)| *cell != directories[0].1.cell_id())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    provisioner.admit_account(ACCOUNT).await.unwrap();
    let mut released = Vec::new();
    for (spec, target) in &directories {
        if authority
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap()
            .value()
            .owner
            .is_none()
        {
            released.push(spec.clone());
        }
    }
    assert_eq!(released.len(), 1);
    assert_ne!(released[0], directories[0].0);
    let spec = &released[0];
    provisioner
        .admit_existing_directory(ACCOUNT, spec)
        .await
        .unwrap();
    let target = directory_target(ACCOUNT, spec).unwrap();
    assert_eq!(
        client
            .query::<ReadDirectory>(&target, None, Json(()))
            .await
            .unwrap()
            .output
            .0
            .unwrap()
            .mode,
        DirectoryMode::Retired
    );
    assert!(matches!(
        client
            .command::<InstallDirectory>(
                &target,
                mutation(),
                Json(DirectoryInstall {
                    spec: spec.clone(),
                    ranges,
                    source: None,
                })
            )
            .await,
        Err(InvocationError::Rejected(_))
    ));
    host.shutdown().await.unwrap();
}
