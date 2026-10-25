use std::sync::Arc;

use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires isolated RustFS and the compiled SDK HTTP test binary"]
async fn native_http_sdk_rustfs() {
    let sdk = std::env::var_os("CRAB_SDK_HTTP_TEST_BIN").expect("compiled SDK HTTP tests");
    // Hosted Git LFS installations provide global drivers. The approved local
    // Crab driver must win without allowing those shadowed commands to execute.
    let global_config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(global_config.path(), "[filter \"lfs\"]\nprocess = git-lfs filter-process\nclean = git-lfs clean -- %f\nsmudge = git-lfs smudge -- %f\nrequired = true\n").unwrap();
    let server = super::rustfs_server().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let app = super::router(Arc::clone(&server));
    let http = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap();
    });
    let url = format!("http://{address}/git/team/repo.git");
    // A separate SDK test process keeps server implementation crates out of its
    // dependency graph while exercising the real native HTTP/store boundary.
    let output = tokio::process::Command::new(sdk)
        .args([
            "native_crab::round_trip",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env("CRAB_SDK_TEST_HTTP_URL", url)
        .env("GIT_CONFIG_GLOBAL", global_config.path())
        .kill_on_drop(true)
        .output()
        .await;
    stop.cancel();
    let http = http.await;
    let repository = server
        .repositories
        .get(&("team".to_owned(), "repo".to_owned()))
        .unwrap();
    let view = repository.open_view().await;
    let legacy = repository
        .store
        .head(&repository.layout.manifest_path())
        .await;
    crate::test_support::server::close(&server).await;
    http.unwrap();
    let output = output.expect("run SDK HTTP test binary");
    assert!(
        output.status.success(),
        "native HTTP SDK failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let target = stdout
        .lines()
        .find_map(|line| {
            line.split_once("native-http-sdk-commit=")
                .map(|(_, oid)| oid)
        })
        .expect("SDK must verify and report its committed native HTTP result");
    assert!(stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    let view = view.unwrap();
    assert!(matches!(
        legacy,
        Err(crab_storage::StorageError::NotFound { .. })
    ));
    assert_eq!(
        view.refs().get("refs/heads/main").map(String::as_str),
        Some(target)
    );
}
