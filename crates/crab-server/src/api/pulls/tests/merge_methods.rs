use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn squash_and_rebase_publish_stock_git_equivalent_trees_with_distinct_histories() {
    let server = fixtures::fixture().await;
    server
        .cells()
        .unwrap()
        .initialize(crate::cells::Address::Directory)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let git_url = format!("{origin}/git/team/repo.git");
    let stop = CancellationToken::new();
    let app = router(Arc::clone(&server));
    let cancelled = stop.clone();
    let http = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(cancelled.cancelled_owned())
            .await
            .unwrap();
    });
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    git::success(path, &["init", "-b", "main", "--object-format=sha1", "."]).await;
    let write = |first: &str, middle: &str, last: &str| {
        std::fs::write(
            path.join("content.txt"),
            format!("{first}\none\ntwo\nthree\n{middle}\nfour\nfive\nsix\n{last}\n"),
        )
        .unwrap();
    };
    write("original first", "original middle", "original last");
    git::success(path, &["add", "content.txt"]).await;
    git::success(path, &["commit", "-m", "original"]).await;
    git::success(path, &["branch", "feature"]).await;
    write("base edit", "original middle", "original last");
    git::success(path, &["commit", "-am", "base edit"]).await;
    let base = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["branch", "release"]).await;
    git::success(path, &["checkout", "feature"]).await;
    write("original first", "original middle", "topic last edit");
    git::success(path, &["commit", "-am", "first topic change"]).await;
    let first = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(
        path,
        &["commit", "--allow-empty", "-m", "empty topic commit"],
    )
    .await;
    write("original first", "topic middle edit", "topic last edit");
    git::success(path, &["commit", "-am", "second topic change"]).await;
    let head = git::success(path, &["rev-parse", "HEAD"]).await;
    let expected = git::success(path, &["merge-tree", "--write-tree", &base, &head]).await;
    git::success(
        path,
        &["push", "--atomic", &git_url, "main", "release", "feature"],
    )
    .await;

    let repo = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let client = reqwest::Client::builder()
        .default_headers(reqwest::header::HeaderMap::from_iter([(
            reqwest::header::HeaderName::from_static("x-crab-repository-id"),
            repo.id.to_string().parse().unwrap(),
        )]))
        .build()
        .unwrap();
    let settings = format!("{origin}/api/repos/team/repo/settings/merge-methods");
    let applied = json_request(
        &client,
        reqwest::Method::PUT,
        &settings,
        json!({"expected_version":0,"enabled":["squash","rebase"]}),
    )
    .await;
    assert_eq!(
        applied,
        (
            StatusCode::OK,
            json!({"version":1,"enabled":["squash","rebase"]})
        )
    );
    let stale = json_request(
        &client,
        reqwest::Method::PUT,
        &settings,
        json!({"expected_version":0,"enabled":["fast_forward"]}),
    )
    .await;
    assert_eq!(stale.1["error"]["code"], "merge_methods_changed");

    let pulls = format!("{origin}/api/repos/team/repo/pulls");
    for (target, method) in [("main", "squash"), ("release", "rebase")] {
        let created = json_request(
            &client,
            reqwest::Method::POST,
            &pulls,
            json!({
                "request_id": uuid::Uuid::now_v7(),
                "title": "Combine independent edits", "body": "",
                "base_ref": format!("refs/heads/{target}"), "head_ref": "refs/heads/feature",
            }),
        )
        .await;
        assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
        assert_eq!(created.1["merge_methods"], json!(["squash", "rebase"]));
        let number = created.1["number"].as_u64().unwrap();
        let url = format!("{pulls}/{number}/merge");
        let mut body = json!({
            "request_id":uuid::Uuid::now_v7(), "version":created.1["version"], "method":"merge_commit",
            "base_oid": base, "head_oid":head,
        });
        let disabled = json_request(&client, reqwest::Method::POST, &url, body.clone()).await;
        assert_eq!(disabled.1["error"]["code"], "merge_method_disabled");
        body["method"] = json!(method);
        if method == "squash" {
            body["message"] = json!("Curated squash title");
        }
        let merged = json_request(&client, reqwest::Method::POST, &url, body.clone()).await;
        assert_eq!(merged.0, StatusCode::OK, "{}", merged.1);
        assert_eq!(merged.1["merge"]["method"], method);
        let commit = merged.1["merge"]["commit_oid"].as_str().unwrap();
        let repeated = json_request(&client, reqwest::Method::POST, &url, body).await;
        assert_eq!(repeated.1["merge"]["commit_oid"], commit);
        git::success(path, &["fetch", &git_url, target]).await;
        assert_eq!(
            git::success(path, &["rev-parse", "FETCH_HEAD"]).await,
            commit
        );
        assert_eq!(
            git::success(path, &["rev-parse", "FETCH_HEAD^{tree}"]).await,
            expected
        );
        let range = format!("{base}..FETCH_HEAD");
        let subjects = git::success(path, &["log", "--reverse", "--format=%s", &range]).await;
        if method == "squash" {
            assert_eq!(subjects, "Curated squash title");
            assert_eq!(
                git::success(path, &["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
                format!("{commit} {base}")
            );
        } else {
            assert_eq!(subjects, "first topic change\nsecond topic change");
            let replayed_first = git::success(path, &["rev-parse", "FETCH_HEAD^"]).await;
            assert_ne!(replayed_first, first);
            assert_ne!(commit, head);
            assert_eq!(
                git::success(path, &["rev-parse", "FETCH_HEAD^^"]).await,
                base
            );
            let original_author =
                git::success(path, &["show", "-s", "--format=%an <%ae> %aI", &first]).await;
            assert_eq!(
                git::success(
                    path,
                    &["show", "-s", "--format=%an <%ae> %aI", "FETCH_HEAD^"]
                )
                .await,
                original_author
            );
            assert_eq!(
                git::success(path, &["show", "-s", "--format=%cn", "FETCH_HEAD^"]).await,
                "Local operator"
            );
        }
    }
    let feature = git::success(path, &["ls-remote", &git_url, "refs/heads/feature"]).await;
    assert_eq!(feature, format!("{head}\trefs/heads/feature"));
    // Flattening a merge is only safe when it preserves edits from the merge
    // itself. A merge-only file cannot disappear from a browser rebase.
    git::success(path, &["checkout", "-b", "merge-only", &base]).await;
    git::success(path, &["branch", "merge-side"]).await;
    std::fs::write(path.join("first.txt"), "first").unwrap();
    git::success(path, &["add", "first.txt"]).await;
    git::success(path, &["commit", "-m", "topic parent"]).await;
    git::success(path, &["checkout", "merge-side"]).await;
    std::fs::write(path.join("second.txt"), "second").unwrap();
    git::success(path, &["add", "second.txt"]).await;
    git::success(path, &["commit", "-m", "side parent"]).await;
    git::success(path, &["checkout", "merge-only"]).await;
    git::success(path, &["merge", "--no-ff", "--no-commit", "merge-side"]).await;
    std::fs::write(path.join("only-in-merge.txt"), "resolution").unwrap();
    git::success(path, &["add", "only-in-merge.txt"]).await;
    git::success(path, &["commit", "-m", "resolve topic merge"]).await;
    git::success(path, &["push", &git_url, "merge-only"]).await;
    let before = git::success(path, &["ls-remote", &git_url, "refs/heads/main"]).await;
    let created = json_request(
        &client,
        reqwest::Method::POST,
        &pulls,
        json!({
            "request_id": uuid::Uuid::now_v7(), "title": "Merge only edit", "body": "",
            "base_ref": "refs/heads/main", "head_ref": "refs/heads/merge-only",
        }),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.1);
    let number = created.1["number"].as_u64().unwrap();
    let refusal = json_request(
        &client,
        reqwest::Method::POST,
        &format!("{pulls}/{number}/merge"),
        json!({
            "request_id":uuid::Uuid::now_v7(), "version":created.1["version"], "method":"rebase",
            "base_oid": created.1["base_oid"], "head_oid": created.1["head_oid"],
        }),
    )
    .await;
    assert_eq!(refusal.0, StatusCode::CONFLICT, "{}", refusal.1);
    assert_eq!(refusal.1["error"]["code"], "unsafe_rebase");
    let current: Value = client
        .get(format!("{pulls}/{number}"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(current["merge_pending"].is_null());
    assert_eq!(
        git::success(path, &["ls-remote", &git_url, "refs/heads/main"]).await,
        before
    );

    stop.cancel();
    http.await.unwrap();
    fixtures::close(&server).await;
}

#[tokio::test]
async fn only_current_repository_admins_can_change_merge_methods() {
    let h = crate::test_support::http::Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let path = "/api/repos/team/private/settings/merge-methods";
    let input = json!({"expected_version":0,"enabled":["rebase"]});
    let denied = h
        .write(reqwest::Method::PUT, path, &bob, input.clone())
        .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    assert_eq!(h.json(path, &alice).await["version"], 0);
    let changed = h.write(reqwest::Method::PUT, path, &alice, input).await;
    assert_eq!(changed.status(), StatusCode::OK);
    assert_eq!(
        h.json(path, &alice).await,
        json!({"version":1,"enabled":["rebase"]})
    );
    h.close().await;
}
