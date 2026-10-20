use super::*;

#[tokio::test]
async fn completed_maintenance_expires_and_observes_ref_only_changes() {
    let server = fixture_without_cells().await;
    let repo = repository(&server);
    let history = git_history(2);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let (root, old) = publish_next(
        &repo,
        root,
        None,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    let initial = crate::projection::current_source(&repo, &server, &server.cancellation)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if repo
                .schedule_maintenance(&server, MaintenanceTrigger::Poll(&initial))
                .await
                .unwrap()
                == MaintenanceSchedule::Completed
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let source = crate::projection::current_source(&repo, &server, &server.cancellation)
        .await
        .unwrap();
    assert_eq!(
        repo.schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
            .await
            .unwrap(),
        MaintenanceSchedule::Completed
    );
    repo.maintenance.lock().await.completed.as_mut().unwrap().1 -= crate::integrity::SCRUB_INTERVAL;
    assert_eq!(
        repo.schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
            .await
            .unwrap(),
        MaintenanceSchedule::Started
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        while !repo
            .maintenance
            .lock()
            .await
            .worker
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    repo.schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
        .await
        .unwrap();
    let etag = root.etag().clone();
    publish_next(&repo, root, Some(old), history.oids[1].clone(), None).await;
    assert_eq!(
        crab_write::capsule_protocol::open_root(&repo.layout)
            .await
            .unwrap()
            .etag(),
        &etag
    );
    let source = crate::projection::source_identity(
        &crate::projection::open_view(&repo.layout)
            .await
            .unwrap()
            .git_snapshot()
            .unwrap(),
    )
    .unwrap();
    let scheduled = repo
        .schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
        .await
        .unwrap();
    close(&server).await;
    assert_eq!(scheduled, MaintenanceSchedule::Started);
}

#[tokio::test]
async fn wide_raw_byte_tree_preserves_cell_attribution_across_batches() {
    let server = fixture().await;
    let repo = repository(&server);
    let (history, paths) = crate::test_git::wide_history(1025);
    let oid = history.oids[0].clone();
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(&repo, root, None, oid.clone(), Some(history.pack)).await;
    crate::maintenance::run(
        repo.layout.clone(),
        Arc::clone(&server.maintenance_admission),
        None,
        server.cancellation.clone(),
        crate::maintenance::Pass::BackgroundMaintenance,
        Some(crate::maintenance::ProjectionContext {
            repository_id: repo.id,
            client: server
                .repository_cells
                .as_ref()
                .map(|router| (**router).clone()),
            metrics: server.metrics.clone(),
            identity: repo.identity.clone(),
            runtime: Arc::clone(&server.runtime),
            options: server.options,
        }),
    )
    .await
    .unwrap()
    .unwrap();
    for action in ["tree", "tree-attribution"] {
        let mut actual = Vec::new();
        let mut cursor = String::new();
        loop {
            let response = router(Arc::clone(&server))
                .oneshot(
                    Request::builder()
                        .uri(format!(
                            "/api/repos/team/repo/{action}?rev={oid}&limit=200{cursor}"
                        ))
                        .header("host", "localhost:8788")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
                .await
                .unwrap();
            let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
            actual.extend(page["items"].as_array().unwrap().iter().map(|item| {
                (
                    item["path_hex"].as_str().unwrap().to_owned(),
                    item["last_commit"]["oid"].as_str().map(str::to_owned),
                )
            }));
            let Some(next) = page["next"].as_str() else {
                break;
            };
            cursor = format!("&cursor={next}");
        }
        let expected = paths
            .iter()
            .map(|path| {
                (
                    path.iter().map(|byte| format!("{byte:02x}")).collect(),
                    (action == "tree-attribution").then(|| oid.clone()),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{action} changed native Git byte paths");
    }
    close(&server).await;
}
