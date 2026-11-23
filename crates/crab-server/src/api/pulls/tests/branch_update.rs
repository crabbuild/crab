use crate::test_support::{git, http::Harness};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const API: &str = "/api/repos/team/private";
const UPDATE: &str = "/api/repos/team/private/pulls/1/update-branch";

#[tokio::test(flavor = "multi_thread")]
async fn rebase_updates_the_exact_head_and_replaces_its_visibility() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &writer,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let head = git::push_review_branches(path, remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    git(&["checkout", "-b", "integration", "main"]).await;
    std::fs::write(path.join("BASE.txt"), "new base\n").unwrap();
    git(&["add", "BASE.txt"]).await;
    git(&["commit", "-m", "Advance the base independently"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    h.write(Method::POST, &format!("{API}/pulls"), &writer,
        json!({"request_id":Uuid::now_v7(),"title":"Rebase feature","body":"","base_ref":"refs/heads/integration","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    let capture = async || {
        let mut input = h.json(UPDATE, &writer).await["update"].clone();
        input["method"] = json!("rebase");
        input
    };
    let stale = capture().await;
    git(&["commit", "--allow-empty", "-m", "Move the previewed base"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    let base = git(&["rev-parse", "HEAD"]).await;
    let stale = h.write(Method::POST, UPDATE, &writer, stale).await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale.json::<Value>().await.unwrap()["error"]["code"],
        "branch_update_changed"
    );
    let picked = capture().await;
    let mut invalid = picked.clone();
    invalid["resolutions"] = json!([]);
    assert_eq!(
        h.write(Method::POST, UPDATE, &writer, invalid)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let protection = format!("{API}/settings/branch-protections");
    h.write(
        Method::PUT,
        &protection,
        &writer,
        json!({"expected_version":0,
        "rules":[{"branch":"feature","required_approvals":1,"required_checks":[]}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(Method::POST, UPDATE, &writer, picked.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    git(&["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, head);
    h.write(
        Method::PUT,
        &protection,
        &writer,
        json!({"expected_version":1,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();

    let result: Value = h
        .write(Method::POST, UPDATE, &writer, picked.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let rebased = result["commit"].as_str().unwrap();
    assert_ne!(rebased, head);
    git(&["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{rebased} {base}")
    );
    assert_eq!(
        git(&["rev-parse", "FETCH_HEAD^{tree}"]).await,
        git(&["merge-tree", "--write-tree", &base, &head]).await
    );
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let view = repo.open_git_view().await.unwrap();
    assert!(
        !view
            .git_visibility_index()
            .await
            .unwrap()
            .contains_hex_in_ref("refs/heads/feature", &head)
    );
    assert_eq!(view.refs().get("refs/heads/integration"), Some(&base));

    let mut different = picked.clone();
    different["method"] = json!("merge_commit");
    assert_eq!(
        h.write(Method::POST, UPDATE, &writer, different)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    git(&["cherry-pick", &head]).await;
    std::fs::write(path.join("BASE.txt"), "base advanced again\n").unwrap();
    git(&["commit", "-am", "Advance past an already integrated topic"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    let newer_base = git(&["rev-parse", "HEAD"]).await;
    let updated: Value = h
        .write(Method::POST, UPDATE, &writer, capture().await)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["commit"], newer_base);
    let replay: Value = h
        .write(Method::POST, UPDATE, &writer, picked.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay, result);
    git(&["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, newer_base);

    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance before choosing a new head",
    ])
    .await;
    git(&["push", remote.as_str(), "integration"]).await;
    let advanced_base = git(&["rev-parse", "HEAD"]).await;
    let mut replacement = h.json(UPDATE, &writer).await["update"].clone();
    for occupied in ["main", "feature/nested"] {
        replacement["new_branch"] = json!(occupied);
        let rejected = h
            .write(Method::POST, UPDATE, &writer, replacement.clone())
            .await;
        assert_eq!(rejected.status(), StatusCode::CONFLICT);
        assert_eq!(
            rejected.json::<Value>().await.unwrap()["error"]["code"],
            "branch_update_destination"
        );
    }
    replacement["new_branch"] = json!("continued-feature");
    let switched: Value = h
        .write(Method::POST, UPDATE, &writer, replacement.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(switched["pull_updated"], true);
    assert_eq!(switched["branch"], "refs/heads/continued-feature");
    let detail = h.json(&format!("{API}/pulls/1"), &writer).await;
    assert_eq!(detail["head_ref"], switched["branch"]);
    assert_eq!(detail["head_oid"], switched["commit"]);
    git(&["fetch", remote.as_str(), "continued-feature"]).await;
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!(
            "{} {newer_base} {advanced_base}",
            switched["commit"].as_str().unwrap()
        )
    );
    git(&["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, newer_base);
    for (input, expected) in [(replacement, switched), (picked, result)] {
        let replay: Value = h
            .write(Method::POST, UPDATE, &writer, input)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(replay, expected);
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_only_the_head_and_rejects_stale_protected_and_conflicting_inputs() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &writer,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let head = git::push_review_branches(path, remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    git(&["checkout", "-b", "integration", "main"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    h.write(Method::POST, &format!("{API}/pulls"), &writer,
        json!({"request_id":Uuid::now_v7(),"title":"Join changes","body":"","base_ref":"refs/heads/integration","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    assert!(h.json(UPDATE, &writer).await["update"].is_null());

    std::fs::write(path.join("INTEGRATION.txt"), "integration branch\n").unwrap();
    std::fs::create_dir(path.join("docs")).unwrap();
    std::fs::write(path.join("docs/SUMMARY.txt"), "original summary\n").unwrap();
    git(&["add", "INTEGRATION.txt", "docs/SUMMARY.txt"]).await;
    git(&["commit", "-m", "Change the base branch"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    let preview = h.json(UPDATE, &writer).await;
    let protection = format!("{API}/settings/branch-protections");
    h.write(
        Method::PUT,
        &protection,
        &writer,
        json!({"expected_version":0,
        "rules":[{"branch":"feature","required_approvals":1,"required_checks":[]}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let blocked = h
        .write(Method::POST, UPDATE, &writer, preview["update"].clone())
        .await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    h.write(
        Method::PUT,
        &protection,
        &writer,
        json!({"expected_version":1,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance the previewed base",
    ])
    .await;
    git(&["push", remote.as_str(), "integration"]).await;
    let base = git(&["rev-parse", "HEAD"]).await;
    let stale = h
        .write(Method::POST, UPDATE, &writer, preview["update"].clone())
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale.json::<Value>().await.unwrap()["error"]["code"],
        "branch_update_changed"
    );

    let fresh = h.json(UPDATE, &writer).await["update"].clone();
    let first: Value = h
        .write(Method::POST, UPDATE, &writer, fresh.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let replay: Value = h
        .write(Method::POST, UPDATE, &writer, fresh)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, replay);
    git(&["fetch", remote.as_str(), "feature"]).await;
    let updated = git(&["rev-parse", "FETCH_HEAD"]).await;
    assert_eq!(first["commit"], updated);
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{updated} {head} {base}")
    );
    assert_eq!(
        git(&["show", "FETCH_HEAD:INTEGRATION.txt"]).await,
        "integration branch"
    );
    assert!(h.json(UPDATE, &writer).await["update"].is_null());
    git(&["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, base);

    std::fs::write(path.join("README.md"), "integration version\n").unwrap();
    std::fs::write(path.join("docs/SUMMARY.txt"), "integration summary\n").unwrap();
    std::fs::write(path.join("BASE-ONLY.txt"), "preserve the clean addition\n").unwrap();
    git(&["add", "BASE-ONLY.txt"]).await;
    git(&["commit", "-am", "Edit the same lines on the base"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    let integration_tip = git(&["rev-parse", "HEAD"]).await;
    git(&["checkout", "feature"]).await;
    git(&["merge", "--ff-only", &updated]).await;
    std::fs::write(path.join("README.md"), "feature version\n").unwrap();
    std::fs::write(path.join("docs/SUMMARY.txt"), "feature summary\n").unwrap();
    git(&["commit", "-am", "Edit the same lines on the head"]).await;
    git(&["push", remote.as_str(), "feature"]).await;
    let conflicted_head = git(&["rev-parse", "HEAD"]).await;
    let input = h.json(UPDATE, &writer).await["update"].clone();
    let conflicted = h.write(Method::POST, UPDATE, &writer, input).await;
    assert_eq!(conflicted.status(), StatusCode::CONFLICT);
    assert_eq!(
        conflicted.json::<Value>().await.unwrap()["error"]["code"],
        "branch_update_conflict"
    );
    git(&["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, conflicted_head);

    let endpoint = format!("{UPDATE}?conflicts=true");
    let preview = h.json(&endpoint, &writer).await;
    let conflicts = preview["conflicts"].as_array().unwrap();
    assert_eq!(conflicts.len(), 2);
    assert_eq!(conflicts[0]["path"], "README.md");
    assert_eq!(conflicts[1]["path"], "docs/SUMMARY.txt");
    let marked = conflicts[0]["text"].as_str().unwrap();
    assert!(marked.contains("<<<<<<< PR head\nfeature version"));
    assert!(marked.contains("integration version\n>>>>>>> Base branch"));
    let readme_text = "combined readme\n".repeat(6_000);
    let summary_text = "combined summary\n".repeat(6_000);
    let readme = json!({"path_hex":conflicts[0]["path_hex"],"text":readme_text});
    let summary = json!({"path_hex":conflicts[1]["path_hex"],"text":summary_text});
    let resolve = |preview: &Value, edits: Value| {
        let mut input = preview["update"].clone();
        input["resolutions"] = edits;
        input
    };
    for invalid in [
        json!([readme]),
        json!([readme, summary, {"path_hex":crate::api::encode_hex(b"BASE-ONLY.txt"),"text":"overwrite the clean addition"}]),
        json!([{ "path_hex":conflicts[0]["path_hex"], "text":marked }, summary]),
    ] {
        assert_eq!(
            h.write(Method::POST, UPDATE, &writer, resolve(&preview, invalid))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    git(&["commit", "--allow-empty", "-m", "Move the previewed head"]).await;
    git(&["push", remote.as_str(), "feature"]).await;
    let new_head = git(&["rev-parse", "HEAD"]).await;
    let stale = h
        .write(
            Method::POST,
            UPDATE,
            &writer,
            resolve(&preview, json!([readme, summary])),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale.json::<Value>().await.unwrap()["error"]["code"],
        "branch_update_changed"
    );
    let preview = h.json(&endpoint, &writer).await;
    let input = resolve(&preview, json!([readme, summary]));
    let first: Value = h
        .write(Method::POST, UPDATE, &writer, input.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let replay: Value = h
        .write(Method::POST, UPDATE, &writer, input)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, replay);
    let commit = first["commit"].as_str().unwrap();
    git(&["fetch", remote.as_str(), "feature"]).await;
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{commit} {new_head} {integration_tip}")
    );
    for (file, content) in [
        ("README.md", readme_text.trim_end()),
        ("docs/SUMMARY.txt", summary_text.trim_end()),
        ("BASE-ONLY.txt", "preserve the clean addition"),
    ] {
        assert_eq!(git(&["show", &format!("FETCH_HEAD:{file}")]).await, content);
    }
    git(&["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, integration_tip);
    h.close().await;
}
