//! Read-only pull mirrors fed by a stock Git host.

use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::native::{branches_and_tags, commit, harness};
use super::wait_for_state;
use crate::test_support::http::Harness;
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
        "without auth.state_key_file a mirror cannot keep a source token"
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
    // Holding the whole local staging budget proves the sync never stages on disk.
    let held = hold_local_staging(&h).await;
    let synced = sync_now(&h, &cookie, id).await;
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
    let current = sync_now(&h, &cookie, id).await;
    assert_eq!(current["status"]["updated_refs"], 0, "{current}");
    assert_eq!(host.fetches(), fetches);
    drop(held);
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

/// Request a sync and wait until the mirror records its outcome.
async fn sync_now(h: &Harness, cookie: &str, repository: Uuid) -> Value {
    let mirror = format!("/api/repositories/{repository}/mirror");
    let before = h.json(&mirror, cookie).await["status"]["checked_at_ms"].as_u64();
    let accepted = h
        .write(Method::POST, &format!("{mirror}/sync"), cookie, json!({}))
        .await;
    assert_eq!(accepted.status(), StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(600), async {
        loop {
            let current = h.json(&mirror, cookie).await;
            if current["status"]["checked_at_ms"].as_u64() != before {
                return current;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap()
}

/// Reserve every byte of local staging until dropped.
async fn hold_local_staging(h: &Harness) -> crate::local_disk::StagingDirectory {
    let staging = &h.server.local_staging;
    staging
        .create(
            staging.available_bytes(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn private_pull_mirror_syncs_with_its_sealed_token() {
    let h = Harness::with_ci_key("private-mirror").await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let cookie = h.login().await;
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "--initial-branch=main", "."]).await;
    commit(&source, "a.txt", "one\n", "First").await;
    let host = GitHost::serve(sources.path()).await;
    host.require_token("upstream-secret");
    let anonymous = h
        .write(
            Method::POST,
            "/api/imports/git",
            &cookie,
            json!({"request_id": Uuid::now_v7(), "source": host.url("source"), "owner": "team",
                "name": "anonymous", "mirror": true}),
        )
        .await;
    assert_eq!(anonymous.status(), StatusCode::ACCEPTED);
    let job: Value = anonymous.json().await.unwrap();
    let status = format!("/api/imports/git/{}", job["id"].as_str().unwrap());
    wait_for_state(&h, &cookie, &status, "failed").await;

    let started = h
        .write(
            Method::POST,
            "/api/imports/git",
            &cookie,
            json!({"request_id": Uuid::now_v7(), "source": host.url("source"), "owner": "team",
                "name": "mirror", "mirror": true, "token": "upstream-secret"}),
        )
        .await;
    assert_eq!(started.status(), StatusCode::ACCEPTED);
    let job: Value = started.json().await.unwrap();
    let status = format!("/api/imports/git/{}", job["id"].as_str().unwrap());
    wait_for_state(&h, &cookie, &status, "succeeded").await;
    let repository = h
        .server
        .repositories
        .get(&("team".to_owned(), "mirror".to_owned()))
        .unwrap();
    let id = repository.id;
    let mirror = format!("/api/repositories/{id}/mirror");
    assert_eq!(h.json(&mirror, &cookie).await["authenticated"], true);
    let catalog = h.server.catalog().unwrap();
    let root = catalog.root();
    let sealed = root
        .store
        .get_with_etag_bounded(
            &root.path(&format!(
                ".crab/http-server/v1/git-mirrors/{id}/credential.json"
            )),
            64 * 1024,
        )
        .await
        .unwrap()
        .0;
    assert!(
        !sealed
            .windows(b"upstream-secret".len())
            .any(|window| window == b"upstream-secret"),
        "the stored credential is sealed"
    );

    commit(&source, "b.txt", "two\n", "Second").await;
    let held = hold_local_staging(&h).await;
    let synced = sync_now(&h, &cookie, id).await;
    drop(held);
    assert!(synced["status"]["error"].is_null(), "{synced}");
    assert_eq!(synced["status"]["updated_refs"], 1);
    assert_eq!(
        repository.open_ref_view().await.unwrap().refs(),
        &branches_and_tags(&source).await
    );

    // A rotated upstream token fails until the mirror's credential is replaced.
    host.require_token("rotated-secret");
    commit(&source, "c.txt", "three\n", "Third").await;
    let failed = sync_now(&h, &cookie, id).await;
    assert_eq!(
        failed["status"]["error"],
        "The Git source denied access; check the token"
    );
    let credential = format!("{mirror}/credential");
    assert_eq!(
        h.write(
            Method::PUT,
            &credential,
            &cookie,
            json!({"token": "rotated-secret"})
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let synced = sync_now(&h, &cookie, id).await;
    assert!(synced["status"]["error"].is_null(), "{synced}");
    assert_eq!(synced["status"]["updated_refs"], 1);
    assert_eq!(
        repository.open_ref_view().await.unwrap().refs(),
        &branches_and_tags(&source).await
    );

    assert_eq!(
        h.write(Method::DELETE, &credential, &cookie, json!({}))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.json(&mirror, &cookie).await["authenticated"], false);
    assert!(sync_now(&h, &cookie, id).await["status"]["error"].is_string());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "set CRAB_MIRROR_QUALIFY_SOURCE to a local Git repository with real history"]
async fn mirror_catches_up_with_real_history_without_local_staging() {
    let original = std::env::var("CRAB_MIRROR_QUALIFY_SOURCE")
        .expect("CRAB_MIRROR_QUALIFY_SOURCE names a Git repository to copy");
    crate::test_support::server::init_test_tracing();
    let h = harness("qualify-mirror").await;
    let cookie = h.login().await;
    let sources = tempfile::tempdir().unwrap();
    let source = sources.path().join("source");
    // The qualification repository stays read-only; only the copy is rewound.
    git(
        sources.path(),
        &["clone", "--mirror", "--no-local", &original, "source"],
    )
    .await;
    let head = git(&source, &["symbolic-ref", "HEAD"]).await;
    let current = branches_and_tags(&source).await;
    let past = git(&source, &["rev-parse", &format!("{head}~200")]).await;
    for name in current.keys() {
        let merged = run_git(&source, &["merge-base", "--is-ancestor", name, &past])
            .await
            .status
            .success();
        if name != &head && !merged {
            git(&source, &["update-ref", "-d", name]).await;
        }
    }
    git(&source, &["update-ref", &head, &past]).await;
    let host = GitHost::serve(sources.path()).await;
    let started = h
        .write(
            Method::POST,
            "/api/imports/git",
            &cookie,
            json!({"request_id": Uuid::now_v7(), "source": host.url("source"), "owner": "team",
                "name": "mirror", "mirror": true}),
        )
        .await;
    let job: Value = started.json().await.unwrap();
    let status = format!("/api/imports/git/{}", job["id"].as_str().unwrap());
    wait_for_state(&h, &cookie, &status, "succeeded").await;
    for (name, oid) in &current {
        git(&source, &["update-ref", name, oid]).await;
    }
    let repository = h
        .server
        .repositories
        .get(&("team".to_owned(), "mirror".to_owned()))
        .unwrap();
    let held = hold_local_staging(&h).await;
    let started = std::time::Instant::now();
    let synced = sync_now(&h, &cookie, repository.id).await;
    drop(held);
    eprintln!("synced in {:?}: {synced}", started.elapsed());
    assert!(synced["status"]["error"].is_null(), "{synced}");
    assert_eq!(repository.open_ref_view().await.unwrap().refs(), &current);
    let clone = tempfile::tempdir().unwrap();
    git(
        clone.path(),
        &["clone", "--mirror", &remote(&h, &cookie, "read").await, "."],
    )
    .await;
    assert_eq!(branches_and_tags(clone.path()).await, current);
    git(clone.path(), &["fsck", "--strict", "--no-dangling"]).await;
    h.close().await;
}
