use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::{git, http::Harness};

#[tokio::test(flavor = "multi_thread")]
async fn candidate_refs_bind_the_comparison_and_recover_without_recreating_removed_refs() {
    let h = Harness::new(false).await;
    let writer = h.login().await;
    let route = "/api/repos/team/private/pulls/1/merge-candidate";
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
    std::fs::write(path.join("BASE.txt"), "from the base\n").unwrap();
    git(&["add", "BASE.txt"]).await;
    git(&["commit", "-m", "Advance the base independently"]).await;
    git(&["push", remote.as_str(), "integration"]).await;
    let base = git(&["rev-parse", "HEAD"]).await;
    h.write(Method::POST, "/api/repos/team/private/pulls", &writer,
        json!({"request_id":Uuid::now_v7(),"title":"Verify a candidate","body":"","base_ref":"refs/heads/integration","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();

    let input = h.json(route, &writer).await["preview"].clone();
    let prepared: Value = h
        .write(Method::POST, route, &writer, input.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let commit = prepared["commit"].as_str().unwrap();
    let row = h.json(route, &writer).await["candidate"].clone();
    assert_eq!(row["current"], true);
    let target = row["ref_name"].as_str().unwrap();
    git(&["fetch", remote.as_str(), target]).await;
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{commit} {base} {head}")
    );
    assert_eq!(
        git(&["rev-parse", "FETCH_HEAD^{tree}"]).await,
        git(&["merge-tree", "--write-tree", &base, &head]).await
    );
    for spec in [
        format!(":{target}"),
        "HEAD:refs/crab/merge-queue/2".to_owned(),
    ] {
        assert!(
            !git::git(path, &["push", remote.as_str(), &spec])
                .await
                .status
                .success()
        );
    }
    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance the candidate base",
    ])
    .await;
    git(&["push", remote.as_str(), "integration"]).await;
    let new_base = git(&["rev-parse", "HEAD"]).await;
    let stale = h.json(route, &writer).await;
    assert_eq!(stale["candidate"]["current"], false);
    let obsolete = h
        .write(
            Method::POST,
            "/api/repos/team/private/pulls/1/merge",
            &writer,
            json!({"request_id":Uuid::now_v7(),"method":"merge_commit",
            "version":stale["preview"]["version"],"base_oid":new_base,"head_oid":head,
            "tested_candidate_oid":commit}),
        )
        .await;
    assert_eq!(obsolete.status(), StatusCode::CONFLICT);
    assert_eq!(
        obsolete.json::<Value>().await.unwrap()["error"]["code"],
        "merge_conflict"
    );
    let next: Value = h
        .write(Method::POST, route, &writer, stale["preview"].clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(next["commit"], commit);
    assert_eq!(
        h.write(Method::DELETE, route, &writer, json!({"commit":commit}))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.write(
        Method::DELETE,
        route,
        &writer,
        json!({"commit":next["commit"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let replay: Value = h
        .write(Method::POST, route, &writer, input)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay, prepared);
    assert!(h.json(route, &writer).await["candidate"].is_null());
    assert!(
        git(&["ls-remote", remote.as_str(), target])
            .await
            .is_empty()
    );
    git(&["fetch", remote.as_str(), "integration"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, new_base);
    h.close().await;
}
