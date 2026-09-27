use super::global_indexes::{ACCOUNT, mutation, owner};
use crate::*;
use beyonddb::{
    ActivateGlobalIndexImport, ApplyGlobalIndexMutation, BeginDirectoryTransfer,
    DirectoryPartitionInput, ExportGlobalIndexEntries, GlobalIndexApplyOutcome, GlobalIndexEntry,
    GlobalIndexExport, GlobalIndexFingerprint, GlobalIndexImport, GlobalIndexImportComplete,
    GlobalIndexMutation, GlobalIndexQuery, GlobalIndexScan, GlobalIndexSplitPlan, GlobalIndexState,
    ImportGlobalIndexEntry, OpenGlobalIndexImport, PrepareGlobalIndexSplit, ProjectionVersion,
    PublishDirectoryTransfer, ReadDirectoryTransfer, ReadGlobalIndexPartition,
    ReadGlobalIndexState, global_index_target, initialize_global_index,
};

async fn fenced(
    client: &CellClient,
    target: &CellTarget,
    plan: &GlobalIndexSplitPlan,
    epoch: u64,
    key: &Item,
) {
    let scan = client
        .query::<GlobalIndexScan>(
            target,
            None,
            Json(PartitionScanInput {
                table_id: plan.source.index.id.clone(),
                epoch,
                index_name: None,
                limit: None,
                exclusive_start_key: None,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(scan, PartitionScanOutcome::StaleRoute);
    let query = client
        .query::<GlobalIndexQuery>(
            target,
            None,
            Json(beyonddb::PartitionQueryInput {
                table_id: plan.source.index.id.clone(),
                epoch,
                index_name: None,
                partition_key: Item::from([("group".into(), key["group"].clone())]),
                sort: None,
                extra_range_equals: vec![],
                forward: true,
                limit: 100,
                exclusive_start_key: None,
            }),
        )
        .await
        .unwrap()
        .output
        .0;
    assert_eq!(query, beyonddb::PartitionQueryOutcome::StaleRoute);
    assert!(
        matches!(client.command::<ApplyGlobalIndexMutation>(target, mutation(), Json(GlobalIndexMutation {
        index_id: plan.source.index.id.clone(), epoch, key: key.clone(),
        version: ProjectionVersion { source_epoch: 99, sequence: 99 }, item: None,
    })).await, Err(InvocationError::Rejected(result)) if result.output.0 == GlobalIndexApplyOutcome::StaleRoute)
    );
}

async fn restored(
    application: &Arc<crab_cell_app::CompiledApplication>,
    layout: &CellStorageLayout,
    directory: &std::path::Path,
    session: SessionId,
    targets: &[CellTarget],
) -> (
    crab_cell_host::CellNode,
    Arc<CellInitialPartitionProvisioner>,
    CellClient,
) {
    let (host, provisioner, client, _) = owner(application, layout, session, directory);
    for (i, target) in targets.iter().enumerate() {
        let cell_type = application
            .cell_types()
            .iter()
            .find(|cell_type| cell_type.namespace() == target.namespace())
            .unwrap();
        let proof = CellCatalog::new(layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let authority = CellAuthority::new(layout.clone());
        let idle = authority.load(target.cell_id()).await.unwrap().unwrap();
        host.runtime()
            .acquire_idle_restored(
                proof,
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *idle.value().incarnation.as_bytes(),
                    Limits {
                        max_database_bytes: cell_type.database_limit_bytes(),
                        max_capture_bytes: cell_type.capture_limit_bytes(),
                        ..Limits::default()
                    },
                )
                .unwrap(),
                authority,
                idle,
                directory.join(format!("restored-{i}.sqlite")),
                Owner {
                    session,
                    endpoint: "http://index-transfer.internal".into(),
                },
            )
            .await
            .unwrap();
    }
    (host, provisioner, client)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_transfer_retains_versions_tombstones_and_fences_replay_after_restart() {
    let application = Arc::new(
        Beyonddb::compile(BuildDescriptor {
            source_revision: "index-transfer".into(),
            cargo_lock_digest: Digest::from_bytes([1; 32]),
        })
        .unwrap(),
    );
    let account = account_target(ACCOUNT).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let layout = CellStorageLayout::new(
        Store::new(Arc::new(InMemory::new())),
        object_store::path::Path::from("index-transfer"),
        *account.application().as_bytes(),
    );
    let session = SessionId::from_bytes([95; 16]);
    let (mut host, provisioner, mut client, storage) = owner(
        &application,
        &layout,
        session,
        &directory.path().join("first"),
    );
    provisioner.admit_account(ACCOUNT).await.unwrap();
    storage.create_table(ACCOUNT, serde_json::from_value(serde_json::json!({
        "TableName":"SplitIndexes", "BillingMode":"PAY_PER_REQUEST",
        "KeySchema":[{"AttributeName":"id","KeyType":"HASH"}],
        "AttributeDefinitions":[{"AttributeName":"id","AttributeType":"S"},{"AttributeName":"group","AttributeType":"S"},{"AttributeName":"score","AttributeType":"N"}],
        "GlobalSecondaryIndexes":[{"IndexName":"ByGroup","KeySchema":[{"AttributeName":"group","KeyType":"HASH"},{"AttributeName":"score","KeyType":"RANGE"}],"Projection":{"ProjectionType":"ALL"}}]
    })).unwrap()).await.unwrap();
    let table = client
        .query::<DescribeTable>(&account, None, Json("SplitIndexes".into()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let index = &table.global_secondary_indexes[0];
    let index_directory =
        beyonddb::directory_target(ACCOUNT, &beyonddb::DirectorySpec::root(index.id.clone()))
            .unwrap();
    let RoutePageOutcome::Page {
        partitions, epoch, ..
    } = beyonddb::read_global_index_route_page(
        &client,
        "123456789012",
        RoutePageInput {
            table_id: index.id.clone(),
            start_hash: None,
            after_lower: None,
            expected_epoch: None,
        },
    )
    .await
    .unwrap()
    else {
        panic!("missing index route");
    };
    let source = global_index_target(ACCOUNT, &index.id, &partitions[0].partition_id).unwrap();
    let spec = client
        .query::<ReadGlobalIndexPartition>(&source, None, Json(()))
        .await
        .unwrap()
        .output
        .0
        .unwrap();
    let boundary = (u128::from_be_bytes(spec.upper.unwrap()) / 2).to_be_bytes();
    let mut children = [spec.clone(), spec.clone()];
    children[0].partition_id = [96; 16];
    children[0].upper = Some(boundary);
    children[0].epoch = epoch + 1;
    children[1].partition_id = [97; 16];
    children[1].lower = Some(boundary);
    children[1].epoch = epoch + 1;
    let plan = GlobalIndexSplitPlan {
        source: spec,
        children,
        expected_epoch: epoch,
    };
    assert!(
        client
            .command::<BeginDirectoryTransfer>(
                &index_directory,
                mutation(),
                Json(plan.clone().into())
            )
            .await
            .unwrap()
            .output
            .0
    );
    let targets = plan
        .children
        .each_ref()
        .map(|child| global_index_target(ACCOUNT, &index.id, &child.partition_id).unwrap());
    for (i, target) in targets.iter().enumerate() {
        Bootstrap {
            runtime: host.runtime(),
            registry: &application.registry(),
            layout: &layout,
            session,
        }
        .cell(
            target,
            "beyonddb-global-index",
            96 + i as u8,
            &directory.path().join(format!("child-{i}.sqlite")),
            initialize_global_index,
        )
        .await;
    }
    // The same index key is legal for many distinct base keys. Include both
    // split halves, normalized numeric keys, wide escaped keys, large images,
    // and deletion records; a batch of 65 full keys would exceed SQL limits.
    let mut groups = [None, None];
    for n in 0..1000 {
        let key = Item::from([(
            "group".into(),
            AttributeValue::S(format!("{}g{n}", "\u{2}".repeat(1800))),
        )]);
        let hash = data_key_hash(&index.id, &key, &index.specification.key_schema).unwrap();
        if hash < plan.source.upper.unwrap() {
            groups[usize::from(hash >= boundary)].get_or_insert(key["group"].clone());
        }
        if groups.iter().all(Option::is_some) {
            break;
        }
    }
    let groups = groups.map(Option::unwrap);
    let mut entries = Vec::new();
    for n in 0..70 {
        let key = Item::from([
            (
                "id".into(),
                AttributeValue::S(format!("{}item-{n:03}", "\u{1}".repeat(1800))),
            ),
            ("group".into(), groups[n % 2].clone()),
            (
                "score".into(),
                serde_json::from_value(serde_json::json!({"N": "2.000"})).unwrap(),
            ),
        ]);
        let mut item = key.clone();
        if n < 4 {
            item.insert(
                "payload".into(),
                AttributeValue::B(vec![n as u8; 350 * 1024]),
            );
        }
        let entry = GlobalIndexEntry {
            key,
            version: ProjectionVersion {
                source_epoch: 1,
                sequence: n as u64 + 1,
            },
            item: (n % 3 != 0).then_some(item),
        };
        assert_eq!(
            client
                .command::<ApplyGlobalIndexMutation>(
                    &source,
                    mutation(),
                    Json(GlobalIndexMutation {
                        index_id: index.id.clone(),
                        epoch,
                        key: entry.key.clone(),
                        version: entry.version,
                        item: entry.item.clone(),
                    })
                )
                .await
                .unwrap()
                .output
                .0,
            GlobalIndexApplyOutcome::Applied
        );
        entries.push(entry);
    }
    assert!(
        client
            .query::<ExportGlobalIndexEntries>(
                &source,
                None,
                Json(GlobalIndexExport {
                    plan: plan.clone(),
                    after: None
                })
            )
            .await
            .unwrap()
            .output
            .0
            .is_none()
    );
    for target in targets.iter().chain([&source]) {
        assert!(
            client
                .command::<PrepareGlobalIndexSplit>(target, mutation(), Json(plan.clone()))
                .await
                .unwrap()
                .output
                .0
        );
    }
    let mut competing = plan.clone();
    competing.children[0].partition_id = [91; 16];
    assert!(matches!(
        client
            .command::<PrepareGlobalIndexSplit>(&source, mutation(), Json(competing))
            .await,
        Err(InvocationError::Rejected(_))
    ));
    fenced(&client, &source, &plan, epoch, &entries[1].key).await;
    for target in &targets {
        fenced(&client, target, &plan, epoch + 1, &entries[1].key).await;
    }
    let mut copied = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = client
            .query::<ExportGlobalIndexEntries>(
                &source,
                None,
                Json(GlobalIndexExport {
                    plan: plan.clone(),
                    after: cursor,
                }),
            )
            .await
            .unwrap()
            .output
            .0
            .unwrap();
        assert!(!page.entries.is_empty() && page.entries.len() <= 64);
        pages += 1;
        copied.extend(page.entries);
        let Some(next) = page.next else {
            break;
        };
        cursor = Some(next);
    }
    assert!(pages >= 2);
    assert_eq!(copied.len(), entries.len());
    assert_eq!(
        copied.iter().filter(|entry| entry.item.is_none()).count(),
        entries.iter().filter(|entry| entry.item.is_none()).count()
    );
    let mut summaries = [
        GlobalIndexFingerprint::default(),
        GlobalIndexFingerprint::default(),
    ];
    for (position, entry) in copied.iter().enumerate() {
        let hash = data_key_hash(&index.id, &entry.key, &index.specification.key_schema).unwrap();
        let child = usize::from(hash >= boundary);
        summaries[child]
            .include(entry, &plan.children[child])
            .unwrap();
        for expected in [
            GlobalIndexApplyOutcome::Applied,
            GlobalIndexApplyOutcome::Replay,
        ] {
            assert_eq!(
                client
                    .command::<ImportGlobalIndexEntry>(
                        &targets[child],
                        mutation(),
                        Json(GlobalIndexImport {
                            plan: plan.clone(),
                            entry: entry.clone()
                        })
                    )
                    .await
                    .unwrap()
                    .output
                    .0,
                expected
            );
        }
        if position == 12 {
            host.shutdown().await.unwrap();
            (host, _, client) = restored(
                &application,
                &layout,
                &directory.path().join("partial"),
                SessionId::from_bytes([98; 16]),
                &[
                    targets[0].clone(),
                    targets[1].clone(),
                    source.clone(),
                    account.clone(),
                    index_directory.clone(),
                ],
            )
            .await;
            for (i, target) in targets.iter().enumerate() {
                assert_eq!(
                    client
                        .query::<ReadGlobalIndexState>(target, None, Json(()))
                        .await
                        .unwrap()
                        .output
                        .0,
                    Some(GlobalIndexState::Importing {
                        plan: plan.clone(),
                        summary: summaries[i].clone()
                    })
                );
                assert!(
                    client
                        .command::<PrepareGlobalIndexSplit>(target, mutation(), Json(plan.clone()))
                        .await
                        .unwrap()
                        .output
                        .0
                );
            }
            fenced(&client, &source, &plan, epoch, &entries[1].key).await;
        }
    }
    let mut conflict = copied[0].clone();
    conflict.version.sequence += 1000;
    let child = usize::from(
        data_key_hash(&index.id, &conflict.key, &index.specification.key_schema).unwrap()
            >= boundary,
    );
    assert!(
        matches!(client.command::<ImportGlobalIndexEntry>(&targets[child], mutation(), Json(GlobalIndexImport { plan: plan.clone(), entry: conflict })).await, Err(InvocationError::Rejected(result)) if result.output.0 == GlobalIndexApplyOutcome::VersionConflict)
    );
    for (i, target) in targets.iter().enumerate() {
        let complete = GlobalIndexImportComplete {
            plan: plan.clone(),
            expected: summaries[i].clone(),
        };
        assert!(matches!(
            client
                .command::<OpenGlobalIndexImport>(target, mutation(), Json(complete.clone()))
                .await,
            Err(InvocationError::Rejected(_))
        ));
        let mut wrong = complete.clone();
        wrong.expected.digest[0] ^= 1;
        assert!(matches!(
            client
                .command::<ActivateGlobalIndexImport>(target, mutation(), Json(wrong))
                .await,
            Err(InvocationError::Rejected(_))
        ));
        assert!(
            client
                .command::<ActivateGlobalIndexImport>(target, mutation(), Json(complete))
                .await
                .unwrap()
                .output
                .0
        );
        fenced(&client, target, &plan, epoch + 1, &entries[1].key).await;
    }
    assert!(
        client
            .command::<PublishDirectoryTransfer>(
                &index_directory,
                mutation(),
                Json(plan.clone().into())
            )
            .await
            .unwrap()
            .output
            .0
    );
    for spec in [&plan.source, &plan.children[0], &plan.children[1]] {
        assert_eq!(
            client
                .query::<ReadDirectoryTransfer>(
                    &index_directory,
                    None,
                    Json(DirectoryPartitionInput {
                        table_id: index.id.clone(),
                        partition_id: spec.partition_id
                    })
                )
                .await
                .unwrap()
                .output
                .0,
            Some(plan.clone().into())
        );
    }
    host.shutdown().await.unwrap();
    let (host, _, client) = restored(
        &application,
        &layout,
        &directory.path().join("activated"),
        SessionId::from_bytes([99; 16]),
        &[
            targets[0].clone(),
            targets[1].clone(),
            source.clone(),
            account.clone(),
            index_directory.clone(),
        ],
    )
    .await;
    fenced(&client, &source, &plan, epoch, &entries[1].key).await;
    for (i, target) in targets.iter().enumerate() {
        assert_eq!(
            client
                .query::<ReadGlobalIndexState>(target, None, Json(()))
                .await
                .unwrap()
                .output
                .0,
            Some(GlobalIndexState::Activated {
                plan: plan.clone(),
                summary: summaries[i].clone()
            })
        );
    }
    // Resume after directory publication but before either child opens. The
    // retained account plan and sealed source drive recovery without old owners.
    provisioner
        .resume_global_index_split(ACCOUNT, client.clone(), &plan)
        .await
        .unwrap();
    for spec in [&plan.source, &plan.children[0], &plan.children[1]] {
        assert!(
            client
                .query::<ReadDirectoryTransfer>(
                    &index_directory,
                    None,
                    Json(DirectoryPartitionInput {
                        table_id: index.id.clone(),
                        partition_id: spec.partition_id
                    })
                )
                .await
                .unwrap()
                .output
                .0
                .is_none()
        );
    }
    for entry in &copied {
        let child = usize::from(
            data_key_hash(&index.id, &entry.key, &index.specification.key_schema).unwrap()
                >= boundary,
        );
        let old = GlobalIndexMutation {
            index_id: index.id.clone(),
            epoch: epoch + 1,
            key: entry.key.clone(),
            version: ProjectionVersion {
                source_epoch: 0,
                sequence: 1,
            },
            item: Some(entry.key.clone()),
        };
        assert_eq!(
            client
                .command::<ApplyGlobalIndexMutation>(&targets[child], mutation(), Json(old))
                .await
                .unwrap()
                .output
                .0,
            GlobalIndexApplyOutcome::Superseded
        );
        assert!(
            matches!(client.command::<ImportGlobalIndexEntry>(&targets[child], mutation(), Json(GlobalIndexImport { plan: plan.clone(), entry: entry.clone() })).await, Err(InvocationError::Rejected(result)) if result.output.0 == GlobalIndexApplyOutcome::StaleRoute)
        );
    }
    let mut actual = Vec::new();
    for target in &targets {
        let mut after = None;
        loop {
            let PartitionScanOutcome::Page {
                items,
                last_evaluated_key,
            } = client
                .query::<GlobalIndexScan>(
                    target,
                    None,
                    Json(PartitionScanInput {
                        table_id: index.id.clone(),
                        epoch: epoch + 1,
                        index_name: None,
                        limit: Some(10),
                        exclusive_start_key: after,
                    }),
                )
                .await
                .unwrap()
                .output
                .0
            else {
                panic!("child not serving");
            };
            actual.extend(items);
            let Some(next) = last_evaluated_key else {
                break;
            };
            after = Some(next);
        }
    }
    actual.sort_by_key(|item| format!("{:?}", item["id"]));
    let mut expected: Vec<_> = entries.into_iter().filter_map(|entry| entry.item).collect();
    expected.sort_by_key(|item| format!("{:?}", item["id"]));
    assert_eq!(
        actual, expected,
        "no tombstones resurrected or duplicate index keys overwritten"
    );
    host.shutdown().await.unwrap();
}
