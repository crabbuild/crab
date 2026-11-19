use super::*;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires owned loopback S3, CLI run evidence, embedded UI and Playwright"]
async fn browser_publishes_real_run_reports_and_rechecks_revoked_access() {
    let endpoint = std::env::var("CRAB_SDK_TEST_S3_ENDPOINT").expect("owned fixture endpoint");
    let proxy = crate::test_support::http::storage_proxy::StorageProxy::start(&endpoint).await;
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").expect("owned fixture bucket");
    // AWS_ENDPOINT_URL_S3 takes precedence over a generic endpoint override.
    let store = object_store::aws::AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_allow_http(true)
        .with_config(
            object_store::aws::AmazonS3ConfigKey::S3Endpoint,
            &proxy.endpoint,
        )
        .build()
        .unwrap();
    let store = object_store::prefix::PrefixStore::new(
        store,
        format!("results-browser-{}", uuid::Uuid::now_v7()),
    );
    let store = proxy.observed_store(crab_storage::Store::new(Arc::new(store)));
    let h = Harness::new_with_store(false, false, store).await;
    let outcome = std::panic::AssertUnwindSafe(check_browser(&h, &proxy.control_origin))
        .catch_unwind()
        .await;
    proxy.resume();
    h.close().await;
    proxy.close().await;
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

async fn check_browser(h: &Harness, storage_control: &str) {
    h.initialize_directory().await;
    h.add_repository("other").await;
    let output =
        PathBuf::from(std::env::var_os("CRAB_RESULTS_EVIDENCE").expect("CLI evidence directory"));
    std::fs::write(
        output.join("browser-storage-control.json"),
        serde_json::to_vec(&json!({"origin":storage_control})).unwrap(),
    )
    .unwrap();
    let report: Report =
        serde_json::from_slice(&std::fs::read(output.join("report.json")).unwrap()).unwrap();
    let bodies: BTreeMap<String, Vec<u8>> =
        serde_json::from_slice(&std::fs::read(output.join("run-bodies.json")).unwrap()).unwrap();
    let scratch = tempfile::tempdir_in(std::env::var_os("CARGO_TARGET_DIR").unwrap()).unwrap();
    let local = LocalResults::new(scratch.path()).unwrap();
    for (hash, bytes) in bodies {
        assert_eq!(local.put_blob(&bytes).unwrap(), hash);
    }
    let repository = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let remote = RemoteResults::new(
        WorkflowStore::from_storage(repository.store.clone()),
        repository.layout.repo_prefix(),
        CancellationToken::new(),
    );
    for row in &report.comparison.rows {
        local.install(&row.run).unwrap();
        remote.push(&local, &row.run).await.unwrap();
    }
    local.save_report(&report).unwrap();
    let original = remote.push_report(&local, &report).await.unwrap();
    let provenance: Value =
        serde_json::from_slice(&std::fs::read(output.join("reproduction-report.json")).unwrap())
            .unwrap();
    let provenance_report: Report = serde_json::from_value(provenance["report"].clone()).unwrap();
    local.save_report(&provenance_report).unwrap();
    assert_eq!(
        remote
            .push_report(&local, &provenance_report)
            .await
            .unwrap(),
        provenance["id"]
    );
    let imported: Value =
        serde_json::from_slice(&std::fs::read(output.join("metadata-report.json")).unwrap())
            .unwrap();
    let imported_report: Report = serde_json::from_value(imported["report"].clone()).unwrap();
    local.save_report(&imported_report).unwrap();
    assert_eq!(
        remote.push_report(&local, &imported_report).await.unwrap(),
        imported["id"]
    );
    // A separate catalog exercises imported status without changing the two-run
    // publication fixture or granting cross-repository report access.
    let other = h
        .server
        .repositories
        .get(&("team".into(), "other".into()))
        .unwrap();
    let other_remote = RemoteResults::new(
        WorkflowStore::from_storage(other.store.clone()),
        other.layout.repo_prefix(),
        CancellationToken::new(),
    );
    let imported_run = &imported_report.comparison.rows[0].run;
    local.install(imported_run).unwrap();
    other_remote.push(&local, imported_run).await.unwrap();
    // Reports retain model identities, never their payloads. Losing an excluded
    // body must not prevent a new report or make the browser fetch that body.
    let excluded: std::collections::BTreeSet<_> = report
        .comparison
        .rows
        .iter()
        .flat_map(|row| &row.run.snapshot.attachments)
        .filter(|attachment| attachment.kind == AttachmentKind::Output)
        .filter_map(|attachment| attachment.hash.as_ref())
        .collect();
    assert_eq!(excluded.len(), 2);
    for hash in excluded {
        repository
            .store
            .delete(
                &repository
                    .layout
                    .repo_path(&format!("workflow/results/objects/{hash}")),
            )
            .await
            .unwrap();
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let ui = root.join("packages/ui");
    let config = json!({
        "testDir": ui.join("tests/browser"),
        "testMatch": "results-server.e2e.ts",
        "workers": 1, "timeout": 240000, "forbidOnly": true,
        "reporter": [["list"], ["json", {"outputFile": output.join("browser-http.json")}]],
        "outputDir": output.join("browser-http"),
        "use": {"baseURL": h.origin, "trace": "off", "screenshot": "only-on-failure"},
        "projects": [{"name": "results-http"}]
    });
    let configuration = scratch.path().join("playwright.config.cjs");
    std::fs::write(&configuration, format!("module.exports = {config};\n")).unwrap();
    let mut command = tokio::process::Command::new("node");
    command
        .arg(ui.join("node_modules/@playwright/test/cli.js"))
        .arg("test")
        .arg("--config")
        .arg(configuration)
        .arg("--project=results-http")
        .current_dir(ui)
        .kill_on_drop(true)
        .env_remove("DEBUG")
        .env_remove("PLAYWRIGHT_JSON_OUTPUT_FILE");
    for key in [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ] {
        command.env_remove(key);
    }
    let result = tokio::time::timeout(Duration::from_secs(300), command.status())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.success(),
        "The real-server Results browser journey failed; see the Playwright output"
    );
    let proof: Value =
        serde_json::from_slice(&std::fs::read(output.join("browser-http.json")).unwrap()).unwrap();
    assert_eq!(proof["stats"]["expected"], 1);
    assert_eq!(proof["stats"]["unexpected"], 0);
    assert_eq!(proof["stats"]["skipped"], 0);
    assert_eq!(remote.report(&original).await.unwrap(), report);
}
