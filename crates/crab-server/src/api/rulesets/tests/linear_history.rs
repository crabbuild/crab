use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn linear_history_checks_the_ref_range_and_publishes_a_tested_squash() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, _) = seed(&h, &alice).await;
    let path = work.path();
    let git = async |args: &[&str]| git::success(path, args).await;
    let protections = format!("{REPO}/settings/branch-protections");
    h.write(
        Method::PUT,
        &protections,
        &alice,
        json!({"expected_version":0,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    git(&["switch", "-c", "old-topic"]).await;
    git(&["commit", "--allow-empty", "-m", "Historical work"]).await;
    git(&["switch", "main"]).await;
    git(&["merge", "--no-ff", "-m", "Historical merge", "old-topic"]).await;
    git(&["push", remote.as_str(), "main"]).await;
    let mut branch = rule("Linear default", "branch", "~DEFAULT_BRANCH");
    branch["require_linear_history"] = json!(true);
    let mut tag = rule("Linear tags", "tag", "v*");
    tag["require_linear_history"] = json!(true);
    h.write(
        Method::PUT,
        RULESETS,
        &alice,
        json!({"expected_version":0,"rulesets":[branch,tag]}),
    )
    .await
    .error_for_status()
    .unwrap();
    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "One parent on protected main",
    ])
    .await;
    git(&["push", remote.as_str(), "main"]).await;
    let base = git(&["rev-parse", "HEAD"]).await;

    git(&["switch", "-c", "feature"]).await;
    std::fs::write(path.join("feature.txt"), "feature\n").unwrap();
    git(&["add", "feature.txt"]).await;
    git(&["commit", "-m", "Feature"]).await;
    git(&["push", remote.as_str(), "feature"]).await;
    let head = git(&["rev-parse", "HEAD"]).await;
    git(&["switch", "main"]).await;
    git(&["merge", "--no-ff", "-m", "New merge", "feature"]).await;
    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Single-parent tip over a new merge",
    ])
    .await;
    git(&["tag", "-a", "v1", "-m", "Tag with non-linear history"]).await;
    for refspec in ["main", "refs/tags/v1"] {
        let denied = git::git(path, &["push", remote.as_str(), refspec]).await;
        assert!(!denied.status.success());
        assert!(String::from_utf8_lossy(&denied.stderr).contains("requires linear history"));
    }
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
        format!("{base}\trefs/heads/main")
    );

    h.write(
        Method::PUT,
        &format!("{REPO}/settings/merge-methods"),
        &alice,
        json!({"expected_version":0,"enabled":["merge_commit","squash","rebase"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let pulls = format!("{REPO}/pulls");
    h.write(
        Method::POST,
        &pulls,
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"title":"Linear change","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let pull = format!("{pulls}/1");
    let detail = h.json(&pull, &alice).await;
    assert_eq!(detail["merge_methods"], json!(["squash", "rebase"]));
    let merge = format!("{pull}/merge");
    let rejected = h
        .write(
            Method::POST,
            &merge,
            &alice,
            json!({
                "request_id":Uuid::now_v7(),"version":detail["version"],"method":"merge_commit",
                "base_oid":base,"head_oid":head
            }),
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::CONFLICT);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "linear_history_required"
    );

    // Move enforcement to classic protection and independently require CI.
    h.write(Method::PUT, &protections, &alice, json!({
        "expected_version":1,"rules":[{"branch":"main","require_linear_history":true,"required_checks":["ci/linear"]}]
    })).await.error_for_status().unwrap();
    h.write(
        Method::PUT,
        RULESETS,
        &alice,
        json!({"expected_version":1,"rulesets":[tag]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let candidate = format!("{pull}/merge-candidate");
    let preview = h.json(&candidate, &alice).await["preview"].clone();
    assert_eq!(preview["method"], "squash");
    let tested = h
        .write(Method::POST, &candidate, &alice, preview.clone())
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    for oid in [&head, &tested] {
        h.write(
            Method::POST,
            &format!("{REPO}/statuses/{oid}"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"context":"ci/linear","state":"success"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    assert_eq!(
        h.json(&candidate, &alice).await["candidate"]["promotion"],
        "ready"
    );
    let merged = h
        .write(
            Method::POST,
            &merge,
            &alice,
            json!({
                "request_id":Uuid::now_v7(),"version":preview["version"],
                "base_oid":base,"head_oid":head,"method":"squash","tested_candidate_oid":tested
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(merged["merge"]["commit_oid"], tested);
    git(&["fetch", remote.as_str(), "main"]).await;
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{tested} {base}")
    );
    h.close().await;
}
