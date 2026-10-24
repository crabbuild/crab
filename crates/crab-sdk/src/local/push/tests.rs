use super::*;

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .args([
            "-c",
            "user.name=SDK local push",
            "-c",
            "user.email=sdk@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn local_push_tokens_require_the_native_push_url_binding_version() {
    let binding = LocalPushBinding {
        version: LOCAL_PUSH_TOKEN_VERSION,
        remote: "origin".to_owned(),
        updates: vec![LocalPushUpdate {
            source: Some("refs/heads/main".to_owned()),
            destination: "refs/heads/main".to_owned(),
            expected: None,
            target: Some("1".repeat(40)),
        }],
        dry_run: false,
        policy: WritePolicy::FastForward,
        trusted_execution: false,
        backend: LocalPushBackend::Git {
            repository: "/fixture".to_owned(),
            remote_url_hash: "a".repeat(64),
        },
    };
    let mut json = serde_json::to_value(binding).unwrap();
    let current = LocalPushRecoveryToken::from_json(&json.to_string()).is_ok();
    json["version"] = 3.into();
    assert!(
        current
            && matches!(LocalPushRecoveryToken::from_json(&json.to_string()), Err(error) if error.kind() == ErrorKind::UnsupportedCapability)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn git_transport_acknowledges_fresh_push_without_replaying_saved_tokens() {
    let fixture = tempfile::tempdir().unwrap();
    git(fixture.path(), &["init", "--bare", "remote.git"]);
    let checkout = fixture.path().join("checkout");
    std::fs::create_dir(&checkout).unwrap();
    git(&checkout, &["init", "--initial-branch=main"]);
    std::fs::write(checkout.join("file.txt"), b"transport push\n").unwrap();
    git(&checkout, &["add", "file.txt"]);
    git(&checkout, &["commit", "-m", "transport"]);
    let target = git(&checkout, &["rev-parse", "HEAD"]);
    let remote = fixture.path().join("remote.git");
    git(fixture.path(), &["init", "--bare", "fetch.git"]);
    let fetch_url = fixture.path().join("fetch.git");
    git(
        &checkout,
        &["remote", "add", "origin", fetch_url.to_str().unwrap()],
    );
    git(
        &checkout,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            remote.to_str().unwrap(),
        ],
    );
    git(&checkout, &["config", "branch.main.remote", "origin"]);
    git(
        &checkout,
        &["config", "branch.main.merge", "refs/heads/main"],
    );

    let git_path = std::path::PathBuf::from("/usr/bin/git");
    let tools = crate::LocalTools::new(&git_path, &git_path).unwrap();
    let client = crate::client::memory_local_client(tools);
    let root = checkout.canonicalize().unwrap();
    let repository = LocalRepository {
        client: client.clone(),
        root: root.clone(),
        git_dir: root.join(".git").canonicalize().unwrap(),
        common_dir: root.join(".git").canonicalize().unwrap(),
    };
    let prepared = repository
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap();
    let recovery =
        LocalPushRecoveryToken::from_json(&prepared.recovery_token().to_json().unwrap()).unwrap();
    let unresolved = client
        .resume_push(
            recovery.clone(),
            fixture.path().join("unused-scratch"),
            OperationOptions::default(),
        )
        .await
        .unwrap();
    assert!(matches!(unresolved, LocalPushOutcome::Indeterminate { .. }));
    assert!(
        git(
            fixture.path(),
            &["--git-dir", remote.to_str().unwrap(), "for-each-ref"]
        )
        .is_empty()
    );
    let mut forged = recovery.clone();
    forged.0.trusted_execution = true;
    let error = git::execute(&client, forged, OperationOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Authorization);

    let result = prepared.execute().await.unwrap();
    assert!(matches!(
        result,
        LocalPushOutcome::TransportCommitted { .. }
    ));
    assert_eq!(
        git(
            fixture.path(),
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main"
            ]
        ),
        target
    );
    assert!(matches!(
        client
            .reconcile_push(recovery, OperationOptions::default())
            .await
            .unwrap(),
        LocalPushOutcome::Indeterminate { .. }
    ));
    let unchanged = repository
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap();
    assert!(matches!(
        unchanged.execute().await.unwrap(),
        LocalPushOutcome::TransportCommitted { .. }
    ));
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_push_publishes_generated_pack_without_remote_helper() {
    let checkout = tempfile::tempdir().unwrap();
    git(checkout.path(), &["init", "--initial-branch=main"]);
    std::fs::write(checkout.path().join("file.txt"), b"direct push\n").unwrap();
    git(checkout.path(), &["add", "file.txt"]);
    git(checkout.path(), &["commit", "-m", "direct"]);
    let target = git(checkout.path(), &["rev-parse", "HEAD"]);
    let git_path = git(checkout.path(), &["--exec-path"])
        .strip_suffix("/libexec/git-core")
        .map_or_else(
            || std::path::PathBuf::from("/usr/bin/git"),
            |prefix| std::path::PathBuf::from(prefix).join("bin/git"),
        );
    let git_path = if git_path.is_file() {
        git_path.canonicalize().unwrap()
    } else {
        std::path::PathBuf::from("/usr/bin/git")
    };
    let tools = crate::LocalTools::new(&git_path, &git_path).unwrap();
    let client = crate::client::memory_local_client(tools);
    git(
        checkout.path(),
        &["remote", "add", "origin", "crab://bucket/repository"],
    );
    let locator = crate::RepositoryLocator::new("repository").unwrap();
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let root = checkout.path().canonicalize().unwrap();
    let repository = LocalRepository {
        client: client.clone(),
        root: root.clone(),
        git_dir: root.join(".git").canonicalize().unwrap(),
        common_dir: root.join(".git").canonicalize().unwrap(),
    };

    let prepared = repository
        .prepare_push(
            PushOptions::default()
                .with_destination("refs/heads/main")
                .unwrap(),
        )
        .await
        .unwrap_or_else(|error| {
            let mut message = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(error) = source {
                message.push_str(": ");
                message.push_str(&error.to_string());
                source = error.source();
            }
            panic!("{message}")
        });
    let result = prepared.execute().await.unwrap_or_else(|error| {
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(error) = source {
            message.push_str(": ");
            message.push_str(&error.to_string());
            source = error.source();
        }
        panic!("{message}")
    });

    assert!(matches!(result, LocalPushOutcome::Committed { .. }));
    let remote = client
        .open_remote_with_options(locator, OperationOptions::default())
        .await
        .unwrap();
    let refs = remote.refs().await.unwrap();
    assert_eq!(
        refs.entries()
            .iter()
            .find(|reference| reference.name() == "refs/heads/main")
            .unwrap()
            .target()
            .to_string(),
        target
    );
    let before = client
        .0
        .store
        .list_prefix(&object_store::path::Path::from("repository/"))
        .await
        .unwrap();
    let dry = repository
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(vec![
                    PushRefspec::update("refs/heads/main", "refs/tags/dry-run").unwrap(),
                ])
                .unwrap()
                .dry_run(true),
        )
        .await
        .unwrap();
    let dry_token =
        LocalPushRecoveryToken::from_json(&dry.recovery_token().to_json().unwrap()).unwrap();
    assert!(matches!(
        dry.execute().await.unwrap(),
        LocalPushOutcome::DryRun { .. }
    ));
    assert!(matches!(
        client
            .resume_push(
                dry_token.clone(),
                checkout.path().to_owned(),
                OperationOptions::default()
            )
            .await
            .unwrap(),
        LocalPushOutcome::DryRun { .. }
    ));
    assert!(matches!(
        client
            .reconcile_push(dry_token, OperationOptions::default())
            .await
            .unwrap(),
        LocalPushOutcome::DryRun { .. }
    ));
    let options = PushOptions::default()
        .with_destination("refs/heads/main")
        .unwrap();
    let unchanged = repository.prepare_push(options).await.unwrap();
    let token =
        LocalPushRecoveryToken::from_json(&unchanged.recovery_token().to_json().unwrap()).unwrap();
    assert!(matches!(
        unchanged.execute().await.unwrap(),
        LocalPushOutcome::TransportCommitted { .. }
    ));
    assert!(matches!(
        client
            .resume_push(
                token.clone(),
                checkout.path().to_owned(),
                OperationOptions::default()
            )
            .await
            .unwrap(),
        LocalPushOutcome::TransportCommitted { .. }
    ));
    assert!(matches!(
        client
            .reconcile_push(token.clone(), OperationOptions::default())
            .await
            .unwrap(),
        LocalPushOutcome::TransportCommitted { .. }
    ));
    let after = client
        .0
        .store
        .list_prefix(&object_store::path::Path::from("repository/"))
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "unchanged pushes must not write remote state"
    );
    std::fs::write(checkout.path().join("file.txt"), b"later local edit\n").unwrap();
    git(checkout.path(), &["add", "file.txt"]);
    git(checkout.path(), &["commit", "-m", "later"]);
    let stale = client
        .reconcile_push(token.clone(), OperationOptions::default())
        .await
        .unwrap_err();
    assert_eq!(stale.kind(), ErrorKind::Conflict);
    let mut read_only = token;
    read_only.0.dry_run = true;
    read_only.0.updates[0].target = Some(git(checkout.path(), &["rev-parse", "HEAD"]));
    assert!(matches!(
        client
            .resume_push(
                read_only,
                checkout.path().to_owned(),
                OperationOptions::default()
            )
            .await
            .unwrap(),
        LocalPushOutcome::DryRun { .. }
    ));

    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_push_publishes_staged_pointer_dependencies_first() {
    let checkout = tempfile::tempdir().unwrap();
    git(checkout.path(), &["init", "--initial-branch=main"]);
    let content = (0..2 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let path = checkout.path().join("large.bin");
    std::fs::write(&path, &content).unwrap();
    let staging = crab_staging::StagingArea::open(checkout.path().join(".crab").join("staging"))
        .await
        .unwrap();
    let staged = crab_staging::stream::stage_file_streaming(
        &path,
        checkout.path(),
        &staging,
        crab_staging::stream::StreamStageProgress {
            xorb_builder: Some(crab_staging::stream::StreamStageXorbBuilder::new(
                1,
                crab_xet::xorb::builder::XorbBuilder::new,
            )),
            ..Default::default()
        },
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&staged.batch_id).unwrap();
    let pointer = crab_types::pointer::Pointer {
        file_hash: staged.file_hash,
        size: staged.size,
        shard_hint: None,
    }
    .serialize();
    std::fs::write(&path, pointer).unwrap();
    git(checkout.path(), &["add", "large.bin"]);
    git(checkout.path(), &["commit", "-m", "large"]);
    staging.close().await.unwrap();

    let git_path = std::path::PathBuf::from("/usr/bin/git");
    let tools = crate::LocalTools::new(&git_path, &git_path).unwrap();
    let cache = tempfile::tempdir().unwrap();
    let client = crate::client::memory_local_client_with_cache(
        tools,
        crate::ContentCache::new(cache.path(), 4 * 1024 * 1024).unwrap(),
    );
    git(
        checkout.path(),
        &["remote", "add", "origin", "crab://bucket/repository"],
    );
    let locator = crate::RepositoryLocator::new("repository").unwrap();
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let root = checkout.path().canonicalize().unwrap();
    let repository = LocalRepository {
        client: client.clone(),
        root: root.clone(),
        git_dir: root.join(".git").canonicalize().unwrap(),
        common_dir: root.join(".git").canonicalize().unwrap(),
    };

    let prepared = repository
        .prepare_push(
            PushOptions::default()
                .with_destination("refs/heads/main")
                .unwrap(),
        )
        .await
        .unwrap_or_else(|error| {
            let mut message = error.to_string();
            let mut source = std::error::Error::source(&error);
            while let Some(error) = source {
                message.push_str(": ");
                message.push_str(&error.to_string());
                source = error.source();
            }
            panic!("{message}")
        });
    let result = prepared.execute().await.unwrap_or_else(|error| {
        let mut message = error.to_string();
        let mut source = std::error::Error::source(&error);
        while let Some(error) = source {
            message.push_str(": ");
            message.push_str(&error.to_string());
            source = error.source();
        }
        panic!("{message}")
    });

    assert!(matches!(result, LocalPushOutcome::Committed { .. }));
    let remote = client
        .open_remote_with_options(locator, OperationOptions::default())
        .await
        .unwrap();
    let snapshot = remote
        .snapshot(crate::Revision::branch("main").unwrap())
        .await
        .unwrap();
    let mut stream = snapshot
        .open_file(crate::GitPath::new("large.bin").unwrap())
        .await
        .unwrap();
    let mut hydrated = Vec::new();
    while let Some(chunk) = stream.next().await.unwrap() {
        hydrated.extend_from_slice(&chunk);
    }
    assert_eq!(hydrated, content);
    client.close().await.unwrap();
}
