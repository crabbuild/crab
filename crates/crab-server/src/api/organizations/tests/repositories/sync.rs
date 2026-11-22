use std::{path::Path, sync::atomic::Ordering};

use super::{Harness, Method, StatusCode, Uuid, Value, json, repository_write};
use crate::test_support::git::success as git;

pub(super) const PAYLOAD_OID: &str =
    "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

pub(super) async fn exercise(
    h: &Harness,
    alice: &str,
    bob: &str,
    fork: &str,
    id: Uuid,
    upstream_remote: &str,
    upstream_work: &Path,
    fork_work: &Path,
) {
    let sync = format!("{fork}/fork-sync");
    let preview = format!("{sync}?branch=refs%2Fheads%2Fmain");
    let comparison = |value: Value| json!({"branch":value["branch"],"fork_oid":value["fork_oid"],"upstream_oid":value["upstream_oid"]});
    let update = |value| repository_write(h, Method::POST, &sync, bob, id, value);
    assert_eq!(h.json(&preview, bob).await["state"], "up_to_date");
    git(upstream_work, &["checkout", "main"]).await;
    advance(upstream_work, upstream_remote, "first upstream change").await;
    let old = h.json(&preview, bob).await;
    assert_eq!(old["state"], "behind");
    let digest = PAYLOAD_OID;
    let mut upstream = url::Url::parse(upstream_remote).unwrap();
    let secret = upstream.password().unwrap().to_owned();
    upstream.set_username("").unwrap();
    upstream.set_password(None).unwrap();
    h.http
        .put(format!("{upstream}/info/lfs/objects/{digest}?size=5"))
        .basic_auth("crab", Some(secret))
        .body("hello")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let pointer =
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{digest}\nsize 5\n");
    std::fs::write(upstream_work.join("large-file"), pointer).unwrap();
    git(upstream_work, &["add", "large-file"]).await;
    advance(upstream_work, upstream_remote, "second upstream change").await;
    assert_eq!(update(comparison(old)).await.status(), StatusCode::CONFLICT);
    let snapshot = h.json(&preview, bob).await;
    let body = comparison(snapshot.clone());
    let protection = format!("{fork}/settings/branch-protections");
    repository_write(h, Method::PUT, &protection, alice, id, json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_checks":[]}]}))
        .await.error_for_status().unwrap();
    assert_eq!(update(body.clone()).await.status(), StatusCode::FORBIDDEN);
    repository_write(
        h,
        Method::PUT,
        &protection,
        alice,
        id,
        json!({"expected_version":1,"rules":[]}),
    )
    .await
    .error_for_status()
    .unwrap();

    let session = h.json("/api/session", bob).await;
    h.drop_next_fork_sync_reply.store(true, Ordering::SeqCst);
    let response = h
        .http
        .post(format!("{}{sync}", h.origin))
        .header("cookie", bob)
        .header("origin", &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", id.to_string())
        .json(&body)
        .send()
        .await;
    if let Ok(response) = response {
        assert!(response.bytes().await.is_err());
    }
    assert!(!h.drop_next_fork_sync_reply.load(Ordering::SeqCst));
    assert_eq!(h.json(&preview, bob).await["state"], "up_to_date");
    git(fork_work, &["fetch", "origin", "main"]).await;
    git(fork_work, &["merge", "--ff-only", "FETCH_HEAD"]).await;
    advance(fork_work, "origin", "fork branch change").await;
    assert_eq!(h.json(&preview, bob).await["state"], "ahead");
    advance(upstream_work, upstream_remote, "third upstream change").await;

    let replay: Value = update(body)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["commit"], snapshot["upstream_oid"]);
    let diverged = h.json(&preview, bob).await;
    assert_eq!(diverged["state"], "diverged");
    assert_eq!(
        update(comparison(diverged.clone())).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json(&preview, bob).await["fork_oid"],
        diverged["fork_oid"]
    );
}

async fn advance(work: &Path, remote: &str, message: &str) {
    std::fs::write(work.join("version.txt"), message).unwrap();
    git(work, &["add", "version.txt"]).await;
    git(work, &["commit", "-m", message]).await;
    git(work, &["push", remote, "main"]).await;
}
