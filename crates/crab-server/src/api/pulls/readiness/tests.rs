use crate::cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor};
use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::json;

const ROOT: &str = "/api/repos/team/private/pulls/1";
async fn seed(h: &Harness) -> uuid::Uuid {
    let submission = uuid::Uuid::now_v7();
    let repository = h.server.repositories.values()[0].id;
    let cell = h.server.cells().unwrap().repository(repository).unwrap();
    cell.client
        .command::<CreatePull>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                draft: true,
                submission_id: *submission.as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Draft proposal".into(),
                body: "Original body".into(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    submission
}

#[tokio::test]
async fn readiness_and_merge_routes_enforce_drafts_versions_and_creation_replay() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let submission = seed(&h).await;
    let pull = h.json(ROOT, &alice).await;
    assert_eq!(pull["draft"], true);
    assert_eq!(pull["can_merge"], false);
    let merge = json!({"request_id":uuid::Uuid::now_v7(),"version":1,"method":"fast_forward","base_oid":"a".repeat(40),"head_oid":"b".repeat(40)});
    let response = h
        .write(Method::POST, &format!("{ROOT}/merge"), &alice, merge)
        .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "pull_draft"
    );
    let ready = h
        .write(
            Method::PUT,
            &format!("{ROOT}/readiness"),
            &alice,
            json!({"version":1,"draft":false}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(
        (ready["draft"].clone(), ready["version"].clone()),
        (json!(false), json!(2))
    );
    let repeated = h
        .write(
            Method::PUT,
            &format!("{ROOT}/readiness"),
            &alice,
            json!({"version":1,"draft":false}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(repeated["version"], 2);
    assert_eq!(
        h.write(
            Method::PUT,
            &format!("{ROOT}/readiness"),
            &alice,
            json!({"version":1,"draft":true})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let activity = h.json(&format!("{ROOT}/activity"), &alice).await;
    assert_eq!(activity["items"][0]["action"]["kind"], "ready_for_review");
    assert_eq!(h.json(ROOT, &alice).await["draft"], false);
    h.write(
        Method::PATCH,
        ROOT,
        &alice,
        json!({"version":2,"body":"Edited after readiness"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let creation = json!({"request_id":submission,"draft":true,"title":"Draft proposal","body":"Original body","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"});
    let recovered = h
        .write(
            Method::POST,
            "/api/repos/team/private/pulls",
            &alice,
            creation.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert_eq!(recovered["draft"], false);
    assert_eq!(recovered["body"], "Edited after readiness");
    assert_eq!(recovered["version"], 3);
    let mut changed_mode = creation;
    changed_mode["draft"] = json!(false);
    assert_eq!(
        h.write(
            Method::POST,
            "/api/repos/team/private/pulls",
            &alice,
            changed_mode
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test]
async fn only_authors_or_writers_change_readiness_with_csrf_and_archive_enforcement() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h).await;
    let url = format!("{ROOT}/readiness");
    let input = json!({"version":1,"draft":false});
    assert_eq!(
        h.http
            .put(format!("{}{url}", h.origin))
            .header(header::COOKIE, &alice)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.write(Method::PUT, &url, &bob, input.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(h.json(ROOT, &bob).await["draft"], true);
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(Method::PUT, &url, &alice, input).await.status(),
        StatusCode::FORBIDDEN
    );
    h.close().await;
}

async fn merge_recovery_after_archive(published: bool) {
    use crate::cells::repository::{
        CreatePullOutcome, GetPull, MergeMethod, PullMerge, PullMergeTransition, ReservePullMerge,
        ReservePullMergeInput, TransitionPullMerge, TransitionPullMergeInput,
        TransitionPullMergeOutcome,
    };
    use crate::test_support::{git, server as fixtures};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    let server = fixtures::fixture().await;
    let entry = server.repositories.values()[0].clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let app = crate::server::http::router(Arc::clone(&server));
    let http = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
            .unwrap()
    });
    let client = reqwest::Client::new();
    let origin = format!("http://{address}");
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    git::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    git::success(path, &["add", "README.md"]).await;
    git::success(path, &["commit", "-m", "base"]).await;
    let base_oid = git::success(path, &["rev-parse", "HEAD"]).await;
    let git_url = format!("{origin}/git/team/repo.git");
    git::success(path, &["push", &git_url, "main"]).await;
    git::success(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(path.join("README.md"), "base\nfeature\n").unwrap();
    git::success(path, &["commit", "-am", "feature"]).await;
    let head_oid = git::success(path, &["rev-parse", "HEAD"]).await;
    git::success(path, &["push", &git_url, "feature"]).await;
    let cell = server.cells().unwrap().repository(entry.id).unwrap();
    let principal = crate::auth::Principal::Local;
    let actor = crate::app::actor(&principal).unwrap();
    let created = cell
        .client
        .command::<CreatePull>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                draft: false,
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: (&actor).into(),
                title: "Recover admitted merge".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: base_oid.clone(),
                head_oid: head_oid.clone(),
            },
        )
        .await
        .unwrap();
    let CreatePullOutcome::Created(pull) = created.output else {
        panic!("pull not created")
    };
    let merge = PullMerge {
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        author: (&actor).into(),
        method: MergeMethod::FastForward,
        pull_version: pull.version,
        base_oid: base_oid.clone(),
        head_oid: head_oid.clone(),
        commit_oid: head_oid.clone(),
        message: String::new(),
        created_at_ms: 1,
    };
    cell.client
        .command::<ReservePullMerge>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            ReservePullMergeInput {
                pull: 1,
                merge: merge.clone(),
            },
        )
        .await
        .unwrap();
    let pending = cell
        .client
        .command::<TransitionPullMerge>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            TransitionPullMergeInput {
                pull: 1,
                merge: merge.clone(),
                transition: PullMergeTransition::Begin,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        pending.output,
        TransitionPullMergeOutcome::Applied(_)
    ));
    if published {
        // Exercise the same Git publication helper used by the merge handler,
        // leaving only its native completion to be recovered after archive.
        crate::git::receive::fast_forward(
            Arc::clone(&server),
            principal,
            ("team".into(), "repo".into()),
            "refs/heads/main".into(),
            gix_hash::ObjectId::from_hex(base_oid.as_bytes()).unwrap(),
            gix_hash::ObjectId::from_hex(head_oid.as_bytes()).unwrap(),
        )
        .await
        .unwrap();
    }
    client
        .put(format!("{origin}/api/repos/team/repo/settings/archive"))
        .json(&json!({"expected_version":0,"archived":true,"repository":"team/repo"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let response=client.post(format!("{origin}/api/repos/team/repo/pulls/1/merge")).json(&json!({"request_id":uuid::Uuid::from_bytes(merge.request_id),"version":merge.pull_version,"method":"fast_forward","base_oid":base_oid,"head_oid":head_oid})).send().await.unwrap();
    let status = response.status();
    let body = response.json::<serde_json::Value>().await.unwrap();
    let stored = cell
        .client
        .query::<GetPull>(&cell.target, None, 1)
        .await
        .unwrap()
        .output
        .unwrap();
    let current = entry
        .open_current(
            &server,
            crab_remote_git::RepositoryOptions::default(),
            &server.cancellation,
        )
        .await
        .unwrap();
    let actual = current
        .refs()
        .entries
        .iter()
        .find(|entry| entry.name == "refs/heads/main")
        .unwrap()
        .target
        .to_string();
    stop.cancel();
    http.await.unwrap();
    fixtures::close(&server).await;
    assert_eq!(
        status,
        if published {
            StatusCode::OK
        } else {
            StatusCode::FORBIDDEN
        },
        "{body}"
    );
    assert!(stored.merge_pending.is_none());
    assert_eq!(
        stored.state,
        if published {
            crate::cells::repository::PullState::Merged
        } else {
            crate::cells::repository::PullState::Open
        }
    );
    assert_eq!(actual, if published { head_oid } else { base_oid });
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_repository_recovers_the_metadata_of_an_already_published_merge() {
    merge_recovery_after_archive(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_repository_aborts_an_unpublished_merge_without_changing_git() {
    merge_recovery_after_archive(false).await;
}
