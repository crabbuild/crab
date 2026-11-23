use super::*;
use crab_remote_git::{OperationKind, OperationLimits, RepositoryOptions, Revision};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires loopback RustFS, stock Git, and the embedded Chromium UI"]
async fn deep_pull_commit_pages_match_git_and_render_in_the_browser() {
    use std::collections::BTreeSet;
    use std::fmt::Write as _;
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;

    let local = tempfile::tempdir_in(std::env::var("CARGO_TARGET_DIR").unwrap()).unwrap();
    let source = local.path();
    git_ok(
        source,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    let mut import = String::new();
    for index in 0..10_050 {
        writeln!(
            import,
            "commit refs/heads/main\ncommitter History <history@example.invalid> {} +0000\ndata 6\nshared\n",
            1_700_000_000 + index,
        )
        .unwrap();
    }
    let mut process = tokio::process::Command::new("git")
        .current_dir(source)
        .args(["fast-import", "--quiet"])
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    process
        .stdin
        .take()
        .unwrap()
        .write_all(import.as_bytes())
        .await
        .unwrap();
    assert!(process.wait().await.unwrap().success());

    let common = git_ok(source, &["rev-parse", "main"]).await;
    let older = git_ok(source, &["rev-parse", "main~2"]).await;
    git_ok(source, &["branch", "feature"]).await;
    for (branch, file, message) in [
        ("main", "base.txt", "Base-only commit"),
        ("feature", "feature.txt", "Topic file"),
    ] {
        git_ok(source, &["checkout", branch]).await;
        std::fs::write(source.join(file), "content\n").unwrap();
        git_ok(source, &["add", file]).await;
        git_ok(source, &["commit", "-m", message]).await;
    }
    let topic = git_ok(source, &["rev-parse", "feature"]).await;
    let tree = git_ok(source, &["rev-parse", "feature^{tree}"]).await;
    let side = git_ok(
        source,
        &["commit-tree", &tree, "-p", &common, "-m", "Side lane first"],
    )
    .await;
    let side_tip = git_ok(
        source,
        &["commit-tree", &tree, "-p", &side, "-m", "Side lane tip"],
    )
    .await;
    let head = git_ok(
        source,
        &[
            "commit-tree",
            &tree,
            "-p",
            &topic,
            "-p",
            &side_tip,
            "-p",
            &older,
            "-m",
            "Topic merge",
        ],
    )
    .await;
    git_ok(source, &["update-ref", "refs/heads/feature", &head]).await;
    let base = git_ok(source, &["rev-parse", "main"]).await;
    let git_range = git_ok(source, &["rev-list", "main..feature"]).await;
    let expected = [head.clone(), topic, side_tip, side];
    assert_eq!(
        expected.iter().map(String::as_str).collect::<BTreeSet<_>>(),
        git_range.lines().collect(),
    );
    assert_eq!(
        git_ok(source, &["merge-base", "main", "feature"]).await,
        common
    );

    let h = Harness::new_with_store(false, false, rustfs_store("deep-pull-history")).await;
    h.initialize_directory().await;
    let maintenance = Arc::clone(&h.server.maintenance_admission);
    let slots = maintenance.available_permits() as u32;
    let permit = maintenance.acquire_many_owned(slots).await.unwrap();
    let alice = h.login().await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let remote = format!("{}/git/team/private.git", h.origin);
    let started = Instant::now();
    assert!(
        with_http_token(
            source,
            token["token"].as_str().unwrap(),
            &["push", "--atomic", &remote, "main", "feature"]
        )
        .await
    );
    eprintln!(
        "deep pull history push_ms={}",
        started.elapsed().as_millis()
    );

    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let unindexed = repo
        .open_current(
            &h.server,
            RepositoryOptions::default(),
            &h.server.cancellation,
        )
        .await
        .unwrap();
    assert!(!unindexed.commit_graph_available());
    let operation = unindexed
        .operation_with_limits(
            OperationKind::Compare,
            &h.server.cancellation,
            OperationLimits {
                max_history_commits: 10,
                ..OperationLimits::default()
            },
        )
        .await
        .unwrap();
    let ancestry = async {
        let base = unindexed
            .snapshot(&Revision::parse("refs/heads/main")?, &operation)
            .await?;
        let head = unindexed
            .snapshot(&Revision::parse("refs/heads/feature")?, &operation)
            .await?;
        head.merge_bases(&base, &operation).await
    }
    .await;
    assert_eq!(
        operation.finish(ancestry).await.unwrap(),
        [gix_hash::ObjectId::from_hex(common.as_bytes()).unwrap()],
    );

    let api = "/api/repos/team/private";
    let compare =
        format!("{api}/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true");
    let preview = h.json(&compare, &alice).await;
    assert_changes(
        &preview,
        &common,
        &base,
        &head,
        &git_changes(source, &common, &head).await,
    );
    let pull: Value = h
        .write(
            Method::POST,
            &format!("{api}/pulls"),
            &alice,
            json!({
                "request_id": Uuid::now_v7(), "title":"Deep branch review", "body":"",
                "base_ref":"refs/heads/main", "head_ref":"refs/heads/feature",
                "expected": {"base_oid": preview["base_tip"], "head_oid": preview["commit"]},
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pull["number"], 1);

    let range = format!("{api}/commits?rev={head}&base={base}&limit=2");
    let started = Instant::now();
    let first = h.json(&range, &alice).await;
    let next = first["next"].as_str().unwrap().to_owned();
    let second = h.json(&format!("{range}&cursor={next}"), &alice).await;
    let actual = [&first, &second]
        .into_iter()
        .flat_map(|page| page["items"].as_array().unwrap())
        .map(|commit| commit["oid"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        actual,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(second["next"].is_null());
    eprintln!(
        "deep pull history cold_pages_ms={}",
        started.elapsed().as_millis()
    );

    let browser = Browser::new("deep-pull-history");
    browser
        .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Deep branch review"]).await;
    browser
        .run(&[
            "find", "role", "link", "click", "--name", "Commits", "--exact",
        ])
        .await;
    for message in [
        "Topic merge",
        "Topic file",
        "Side lane tip",
        "Side lane first",
    ] {
        browser.run(&["wait", "--text", message]).await;
    }

    let started = Instant::now();
    assert!(
        crab_remote::browse_indexes::ensure(
            &repo.layout,
            &repo.identity,
            Arc::clone(&h.server.runtime),
            h.server.options,
            2 * 1024 * 1024 * 1024,
            &h.server.cancellation,
        )
        .await
        .unwrap()
        .is_some()
    );
    repo.expire_browse_probe().await;
    assert!(
        repo.open(&h.server, &h.server.cancellation)
            .await
            .unwrap()
            .git
            .commit_graph_available()
    );
    eprintln!(
        "deep pull history graph_ms={}",
        started.elapsed().as_millis()
    );

    let resumed = h.json(&format!("{range}&cursor={next}"), &alice).await;
    assert_eq!(resumed, second);
    assert_eq!(h.json(&range, &alice).await, first);
    browser.run(&["reload"]).await;
    browser.run(&["wait", "--text", "Side lane first"]).await;
    let snapshot = browser.run(&["snapshot"]).await;
    assert!(snapshot.find("Topic file").unwrap() < snapshot.find("Side lane tip").unwrap());
    drop(browser);
    drop(permit);
    h.close().await;
}
