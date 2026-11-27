//! Read-only pull mirrors fed by a stock Git host.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::native::{branches_and_tags, commit, harness};
use super::wait_for_state;
use crate::test_support::{git::git as run_git, git::success as git, git_host::GitHost};

#[tokio::test(flavor = "multi_thread")]
async fn pull_mirror_follows_its_upstream_and_refuses_every_other_write() {
    crate::test_support::server::init_test_tracing();
    let h = harness("pull-mirror").await;
    let cookie = h.login().await;
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main", "."]).await;
    commit(&source, "a.txt", "one\n", "First").await;
    let first = git(&source, &["rev-parse", "HEAD"]).await;
    commit(&source, "b.txt", "two\n", "Second").await;
    git(&source, &["branch", "topic"]).await;
    git(&source, &["branch", "gone"]).await;
    git(&source, &["tag", "-a", "v1", "-m", "Release one"]).await;
    let host = GitHost::serve(sources.path()).await;

    let request = |mirror: bool, token: Option<&str>| {
        json!({
            "request_id": Uuid::now_v7(),
            "source": host.url("source"),
            "owner": "team",
            "name": "mirror",
            "mirror": mirror,
            "token": token,
        })
    };
    let refused = h
        .write(
            Method::POST,
            "/api/imports/git",
            &cookie,
            request(true, Some("fixture-secret")),
        )
        .await;
    assert_eq!(
        refused.status(),
        StatusCode::BAD_REQUEST,
        "mirrors never keep a source credential"
    );
    let started = h
        .write(
            Method::POST,
            "/api/imports/git",
            &cookie,
            request(true, None),
        )
        .await;
    assert_eq!(started.status(), StatusCode::ACCEPTED);
    let job: Value = started.json().await.unwrap();
    assert_eq!(job["mirror"], true);
    let status = format!("/api/imports/git/{}", job["id"].as_str().unwrap());
    wait_for_state(&h, &cookie, &status, "succeeded").await;
    let key = ("team".to_owned(), "mirror".to_owned());
    let repository = h.server.repositories.get(&key).unwrap();
    assert_eq!(
        repository.mirror.as_deref(),
        Some(host.url("source").as_str())
    );
    assert_eq!(
        repository.open_ref_view().await.unwrap().refs(),
        &branches_and_tags(&source).await
    );

    // Every write path refuses the mirror, starting before a push uploads.
    let push_url = remote(&h, &cookie, "write").await;
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &["clone", &remote(&h, &cookie, "read").await, "."],
    )
    .await;
    commit(clone.path(), "local.txt", "local\n", "Local change").await;
    let push = run_git(clone.path(), &["push", &push_url, "HEAD:main"]).await;
    assert!(!push.status.success());
    assert!(
        String::from_utf8_lossy(&push.stderr).contains("read-only mirror"),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    let session = h.json("/api/session", &cookie).await;
    let branch = h
        .http
        .post(format!("{}/api/repos/team/mirror/branches", h.origin))
        .header("cookie", &cookie)
        .header("origin", &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repository.id.to_string())
        .json(&json!({"name": "local", "source_oid": first}))
        .send()
        .await
        .unwrap();
    let status_code = branch.status();
    let body = branch.text().await.unwrap();
    assert_eq!(status_code, StatusCode::FORBIDDEN, "{body}");

    // The upstream moves: a fast-forward, a rewind, a deletion, new refs, a
    // new default branch, and a large file stored in its LFS server.
    commit(&source, "c.txt", "three\n", "Third").await;
    git(&source, &["branch", "-f", "topic", &first]).await;
    git(&source, &["branch", "-D", "gone"]).await;
    git(&source, &["checkout", "-b", "release"]).await;
    let payload = b"mirrored large payload".repeat(512);
    let pointer = crab_git::LfsPointer {
        oid: Sha256::digest(&payload).into(),
        size: payload.len() as u64,
        extensions: Vec::new(),
    };
    std::fs::write(source.join("asset.bin"), pointer.serialize()).unwrap();
    git(&source, &["add", "asset.bin"]).await;
    git(&source, &["commit", "-m", "Add large file"]).await;
    git(&source, &["tag", "-a", "v2", "-m", "Release two"]).await;
    host.offer_lfs(&payload);
    let id = repository.id;
    let sync = format!("/api/repositories/{id}/mirror/sync");
    let synced: Value = h
        .write(Method::POST, &sync, &cookie, json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(synced["status"]["updated_refs"], 5, "{synced}");
    assert!(synced["status"]["error"].is_null());
    let view = repository.open_ref_view().await.unwrap();
    assert_eq!(view.refs(), &branches_and_tags(&source).await);
    assert_eq!(view.head(), "refs/heads/release");
    let stored = crab_lfs::LfsObjectStore::new(
        repository.layout.store().clone(),
        repository.layout.repo_prefix(),
    );
    assert!(stored.exists(&pointer.oid).await.unwrap());
    let verify = tempfile::tempdir().unwrap();
    git(
        verify.path(),
        &["clone", "--mirror", &remote(&h, &cookie, "read").await, "."],
    )
    .await;
    assert_eq!(
        branches_and_tags(verify.path()).await,
        branches_and_tags(&source).await
    );
    git(verify.path(), &["fsck", "--strict", "--no-dangling"]).await;

    // An unchanged upstream is checked without fetching any pack.
    let fetches = host.fetches();
    let current: Value = h
        .write(Method::POST, &sync, &cookie, json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(current["status"]["updated_refs"], 0, "{current}");
    assert_eq!(host.fetches(), fetches);
    assert_eq!(
        h.json(&format!("/api/repositories/{id}/mirror"), &cookie)
            .await["source"],
        host.url("source")
    );

    // Stopping the mirror makes the repository an ordinary writable one.
    let stopped = h
        .write(
            Method::DELETE,
            &format!("/api/repositories/{id}/mirror"),
            &cookie,
            json!({}),
        )
        .await;
    assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
    assert!(h.server.repositories.get(&key).unwrap().mirror.is_none());
    assert_eq!(
        h.write(Method::POST, &sync, &cookie, json!({}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    git(clone.path(), &["fetch", &push_url, "release"]).await;
    git(clone.path(), &["reset", "--hard", "FETCH_HEAD"]).await;
    commit(clone.path(), "local.txt", "local\n", "Local change").await;
    git(clone.path(), &["push", &push_url, "HEAD:release"]).await;
    h.close().await;
}

async fn remote(h: &crate::test_support::http::Harness, cookie: &str, access: &str) -> String {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            cookie,
            json!({"owner":"team","repository":"mirror","access":access}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/mirror.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote
        .set_password(Some(token["token"].as_str().unwrap()))
        .unwrap();
    remote.into()
}
