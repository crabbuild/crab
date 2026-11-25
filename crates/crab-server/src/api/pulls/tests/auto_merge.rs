use std::{sync::Arc, time::Duration};

use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::{git, http::Harness};

const REPO: &str = "/api/repos/team/private";
const PULL: &str = "/api/repos/team/private/pulls/1";
const AUTO: &str = "/api/repos/team/private/pulls/1/auto-merge";

mod deferred;
mod fork;

#[tokio::test(flavor = "multi_thread")]
async fn scheduled_merge_waits_for_the_new_head_and_disarms_after_revocation() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let ready = format!("{REPO}/settings/merge-methods");
    let protection = format!("{REPO}/settings/branch-protections");
    let members = format!("{REPO}/members");
    h.write(
        Method::PUT,
        &ready,
        &alice,
        json!({"expected_version":0,"auto_merge":true,"enabled":["merge_commit"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(Method::PUT, &protection, &alice, json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_checks":["ci"]}]})).await.error_for_status().unwrap();
    let write_access = async |access: &str| {
        let expected = h.json(&members, &alice).await["revision"].clone();
        h.write(Method::PUT, &members, &alice, json!({"expected_revision":expected,"members":[
            {"subject":"alice-id","name":"Alice","access":"admin"}, {"subject":"bob-id","name":"Bob","access":access}],"groups":[]}))
            .await.error_for_status().unwrap();
    };
    write_access("write").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
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
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    let first = git::push_review_branches(path, remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    let base = git(&["rev-parse", "main"]).await;
    h.write(Method::POST, &format!("{REPO}/pulls"), &alice, json!({"request_id":Uuid::now_v7(),"title":"Merge after CI","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    let input = async || {
        let pull = h.json(PULL, &alice).await;
        json!({"request_id":Uuid::new_v4(),"version":pull["version"],"base_oid":pull["base_oid"],"head_oid":pull["head_oid"],"method":"merge_commit","message":"Automatically verified"})
    };
    let schedule = async |cookie: &str, input: Value| {
        h.write(Method::PUT, AUTO, cookie, input)
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let wait = async |reason: &str| {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = h.json(AUTO, &alice).await;
                if state["request"]["stopped"] == reason {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap();
    };
    let canceled = input().await;
    schedule(&bob, canceled.clone()).await;
    h.write(
        Method::DELETE,
        AUTO,
        &alice,
        json!({"request_id":canceled["request_id"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(AUTO, &alice).await["request"]["stopped"], "canceled");
    let revoked = input().await;
    let recorded = schedule(&bob, revoked.clone()).await;
    assert_eq!(
        schedule(&bob, revoked).await["request"],
        recorded["request"]
    );
    write_access("read").await;
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    wait("permission_changed").await;
    let post_check = async |oid: &str| {
        h.write(
            Method::POST,
            &format!("{REPO}/statuses/{oid}"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"state":"success","context":"ci"}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    let armed = input().await;
    schedule(&alice, armed.clone()).await;
    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Move the scheduled head before CI",
    ])
    .await;
    git(&["push", remote.as_str(), "feature"]).await;
    let head = git(&["rev-parse", "HEAD"]).await;
    post_check(&first).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let state = h.json(AUTO, &alice).await;
            assert!(state["request"]["stopped"].is_null(), "{state}");
            if state["request"]["head_oid"] == head {
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    let replayed = schedule(&alice, armed.clone()).await;
    assert_eq!(replayed["request"]["request_id"], armed["request_id"]);
    assert_eq!(replayed["request"]["head_oid"], head);
    assert_eq!(h.json(PULL, &alice).await["state"], "open");
    assert_eq!(
        h.write(Method::PUT, AUTO, &alice, canceled).await.status(),
        StatusCode::CONFLICT
    );
    for (enabled, version) in [(false, 1), (true, 2)] {
        h.write(
            Method::PUT,
            &ready,
            &alice,
            json!({"expected_version":version,"enabled":["merge_commit"],"auto_merge":enabled}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    wait("disabled").await;
    let saved = schedule(&alice, input().await).await;
    assert!(saved["request"]["stopped"].is_null());
    git(&["fetch", remote.as_str(), "main"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, base);
    post_check(&head).await;
    wait("merged").await;
    let merged = h.json(PULL, &alice).await;
    assert_eq!(merged["state"], "merged");
    assert_eq!(merged["head_oid"], head);
    git(&["fetch", remote.as_str(), "main"]).await;
    let id = merged["merge"]["commit_oid"].as_str().unwrap();
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{id} {base} {head}")
    );
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_publishes_tested_squash_and_rebase_history_and_expires_stale_methods() {
    for (method, commits) in [("squash", "1"), ("rebase", "2")] {
        let h = Harness::new(false).await;
        let alice = h.login().await;
        let settings = format!("{REPO}/settings/merge-methods");
        let save_method = async |version, queue_method: &str| {
            h.write(Method::PUT, &settings, &alice, json!({
                "expected_version":version,"enabled":["fast_forward"],"merge_queue_method":queue_method,
                "delete_head_branches":true,
            })).await
        };
        assert_eq!(
            save_method(0, "fast_forward").await.status(),
            StatusCode::BAD_REQUEST
        );
        save_method(0, method).await.error_for_status().unwrap();
        h.write(Method::PUT, &format!("{REPO}/settings/branch-protections"), &alice, json!({
            "expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_checks":["ci"],
                "require_merge_queue":true,"require_linear_history":true}],
        })).await.error_for_status().unwrap();
        let credential: Value = h
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
        let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
        remote.set_username("crab").unwrap();
        remote.set_password(credential["token"].as_str()).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path();
        // Seed the default ref under the same required-queue rule used by the worker.
        git::push_review_branches(path, remote.as_str()).await;
        let git = async |args: &[&str]| git::success(path, args).await;
        std::fs::write(path.join("topic.txt"), "second change\n").unwrap();
        git(&["add", "topic.txt"]).await;
        git(&["commit", "-m", "Second topic change"]).await;
        git(&["push", remote.as_str(), "feature"]).await;
        let head = git(&["rev-parse", "feature"]).await;
        let base = git(&["rev-parse", "main"]).await;
        let topics = git(&["log", "--format=%s", "main..feature"]).await;
        h.write(
            Method::POST,
            &format!("{REPO}/pulls"),
            &alice,
            json!({
                "request_id":Uuid::now_v7(),"title":"Verified linear history","body":"",
                "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
            }),
        )
        .await
        .error_for_status()
        .unwrap();

        let success = async |oid: &str| {
            h.write(
                Method::POST,
                &format!("{REPO}/statuses/{oid}"),
                &alice,
                json!({
                    "request_id":Uuid::now_v7(),"context":"ci","state":"success",
                }),
            )
            .await
        };
        success(&head).await.error_for_status().unwrap();
        let enqueue = async |selected: &str| {
            let pull = h.json(PULL, &alice).await;
            h.write(Method::PUT, AUTO, &alice, json!({
                "request_id":Uuid::now_v7(),"version":pull["version"],"base_oid":base,"head_oid":head,
                "queue":true,"method":selected,
            })).await
        };
        let offered = h.json(AUTO, &alice).await;
        assert_eq!(offered["queue_method"], method);
        assert_eq!(offered["can_enqueue"], true);
        assert_eq!(enqueue("merge_commit").await.status(), StatusCode::CONFLICT);
        enqueue(method).await.error_for_status().unwrap();
        let candidate = format!("{PULL}/merge-candidate");
        let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
        let wait_for_candidate = async || {
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let state = h.json(&candidate, &alice).await;
                    if state["candidate"]["current"] == true {
                        assert_eq!(state["candidate"]["method"], method);
                        break state["candidate"]["commit"].as_str().unwrap().to_owned();
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            })
            .await
            .unwrap()
        };
        let mut tested = wait_for_candidate().await;
        if method == "squash" {
            save_method(1, "rebase").await.error_for_status().unwrap();
            save_method(2, "squash").await.error_for_status().unwrap();
            // A result on the old SHA must not release the previous entry,
            // even when the selected method returns to its original value.
            success(&tested).await.error_for_status().unwrap();
            tokio::time::timeout(Duration::from_secs(30), async {
                while h.json(AUTO, &alice).await["request"]["stopped"] != "method_disabled" {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
                format!("{base}\trefs/heads/main")
            );
            enqueue(method).await.error_for_status().unwrap();
            let next = wait_for_candidate().await;
            assert_ne!(next, tested);
            tested = next;
        }
        git(&["fetch", remote.as_str(), "refs/crab/merge-queue/1"]).await;
        assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, tested);
        assert_eq!(
            git(&["rev-list", "--count", "main..FETCH_HEAD"]).await,
            commits
        );
        assert_eq!(
            git(&["rev-list", "--min-parents=2", "main..FETCH_HEAD"]).await,
            ""
        );
        assert_eq!(
            git(&["rev-parse", &format!("FETCH_HEAD~{commits}")]).await,
            base
        );
        if method == "rebase" {
            assert_eq!(
                git(&["log", "--format=%s", "main..FETCH_HEAD"]).await,
                topics
            );
        }
        success(&tested).await.error_for_status().unwrap();
        let done = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let detail = h.json(PULL, &alice).await;
                if detail["state"] == "merged" {
                    break detail;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(done["merge"]["method"], method);
        assert_eq!(done["merge"]["tested_candidate_oid"], tested);
        assert_eq!(
            git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
            format!("{tested}\trefs/heads/main")
        );
        tokio::time::timeout(Duration::from_secs(15), async {
            while h.json(&format!("{PULL}/head-branch"), &alice).await["branch"]["action"]
                != "restore"
            {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            git(&["ls-remote", remote.as_str(), "refs/heads/feature"])
                .await
                .is_empty()
        );
        worker.abort();
        h.close().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn grouped_checks_build_in_a_bounded_window_rebuild_after_removal_and_publish_in_order() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let settings = format!("{REPO}/settings/merge-methods");
    let write = async |method, path: &str, input| h.write(method, path, &alice, input).await;
    for invalid in [0, 101] {
        let response = write(
            Method::PUT,
            &settings,
            json!({
                "expected_version":0,"enabled":["merge_commit"],"merge_queue_builds":invalid,
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    write(
        Method::PUT,
        &settings,
        json!({
            "expected_version":0,"enabled":["merge_commit"],
            "merge_queue_branches":[{"branch":"main","builds":2}],
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    write(
        Method::PUT,
        &format!("{REPO}/settings/branch-protections"),
        json!({
            "expected_version":0,"rules":[{"branch":"main","required_approvals":0,
                "required_checks":["ci"],"require_merge_queue":true}],
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    let credential: Value = write(
        Method::POST,
        "/api/git-token",
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
    remote.set_password(credential["token"].as_str()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    git::push_review_branches(path, remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    let base = git(&["rev-parse", "main"]).await;
    for branch in ["second", "third"] {
        git(&["checkout", "-b", branch, "main"]).await;
        std::fs::write(path.join(format!("{branch}.txt")), branch).unwrap();
        git(&["add", "-A"]).await;
        git(&["commit", "-m", branch]).await;
        git(&["push", remote.as_str(), branch]).await;
    }
    let success = async |oid: &str| {
        write(
            Method::POST,
            &format!("{REPO}/statuses/{oid}"),
            json!({
                "request_id":Uuid::now_v7(),"context":"ci","state":"success",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    let mut entries = Vec::new();
    for branch in ["feature", "second", "third"] {
        let created: Value = write(
            Method::POST,
            &format!("{REPO}/pulls"),
            json!({
                "request_id":Uuid::now_v7(),"title":branch,"body":"",
                "base_ref":"refs/heads/main","head_ref":format!("refs/heads/{branch}"),
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
        let number = created["number"].as_u64().unwrap();
        let pull = format!("{REPO}/pulls/{number}");
        let detail = h.json(&pull, &alice).await;
        let head = detail["head_oid"].as_str().unwrap();
        success(head).await;
        let request = Uuid::now_v7();
        write(
            Method::PUT,
            &format!("{pull}/auto-merge"),
            json!({
                "request_id":request,"version":detail["version"],"base_oid":base,"head_oid":head,
                "queue":true,"method":"merge_commit",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        entries.push((pull, request));
    }
    let [first, second, third] = entries.as_slice() else {
        unreachable!()
    };
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    let wait_for_candidate = async |pull: &str, parent: &str, grouped: bool| {
        let route = format!("{pull}/merge-candidate");
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let response = h.json(&route, &alice).await;
                let candidate = &response["candidate"];
                if candidate["current"] == true && candidate["base_oid"] == parent {
                    assert_eq!(candidate["grouped"], grouped);
                    break candidate["commit"].as_str().unwrap().to_owned();
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap()
    };
    let first_tip = wait_for_candidate(&first.0, &base, false).await;
    let obsolete = wait_for_candidate(&second.0, &first_tip, true).await;
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let obsolete_oid = gix_hash::ObjectId::from_hex(obsolete.as_bytes()).unwrap();
    assert!(
        super::super::candidate::require_check_target(&h.server, &repo, 2, obsolete_oid)
            .await
            .unwrap()
    );
    assert!(
        h.json(&format!("{}/merge-candidate", third.0), &alice)
            .await["candidate"]
            .is_null()
    );
    git(&["fetch", remote.as_str(), "refs/crab/merge-queue/2"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD^1"]).await, first_tip);
    assert!(
        git(&["show", "FETCH_HEAD:README.md"])
            .await
            .contains("feature")
    );
    success(&obsolete).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
        format!("{base}\trefs/heads/main")
    );

    // Both entries already have candidates. Move the second to the front,
    // then restore the first and replay the first move to model a lost reply.
    let promote = async |entry: &(String, Uuid), request_id: Uuid| {
        write(
            Method::PUT,
            &format!("{}/merge-queue/priority", entry.0),
            json!({"request_id":request_id,"queue_id":entry.1}),
        )
        .await
    };
    let first_move = Uuid::now_v7();
    promote(second, first_move)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json(&format!("{}/auto-merge", second.0), &alice).await["request"]["first"],
        true
    );
    assert!(matches!(
        super::super::candidate::require_check_target(
            &h.server,
            &repo,
            1,
            gix_hash::ObjectId::from_hex(first_tip.as_bytes()).unwrap(),
        )
        .await,
        Err(crate::app::Error::CommitNotFound)
    ));
    let promoted = wait_for_candidate(&second.0, &base, false).await;
    assert_ne!(promoted, obsolete);

    promote(first, Uuid::now_v7())
        .await
        .error_for_status()
        .unwrap();
    promote(second, first_move)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        promote(first, first_move).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json(&format!("{}/auto-merge", first.0), &alice).await["request"]["first"],
        true
    );
    let rebuilt = wait_for_candidate(&first.0, &base, false).await;
    assert_ne!(rebuilt, first_tip);
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
        format!("{base}\trefs/heads/main")
    );

    write(
        Method::DELETE,
        &format!("{}/auto-merge", first.0),
        json!({"request_id":first.1}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(matches!(
        super::super::candidate::require_check_target(&h.server, &repo, 2, obsolete_oid).await,
        Err(crate::app::Error::CommitNotFound)
    ));
    let second_tip = wait_for_candidate(&second.0, &base, false).await;
    assert_ne!(second_tip, obsolete);
    assert_ne!(second_tip, promoted);
    git(&["fetch", remote.as_str(), "refs/crab/merge-queue/2"]).await;
    assert_eq!(git(&["show", "FETCH_HEAD:README.md"]).await, "base");
    let third_tip = wait_for_candidate(&third.0, &second_tip, true).await;
    success(&third_tip).await;
    success(&second_tip).await;
    tokio::time::timeout(Duration::from_secs(40), async {
        while h.json(&third.0, &alice).await["state"] != "merged" {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.json(&first.0, &alice).await["state"], "open");
    assert_eq!(
        h.json(&second.0, &alice).await["merge"]["tested_candidate_oid"],
        second_tip
    );
    assert_eq!(
        h.json(&third.0, &alice).await["merge"]["tested_candidate_oid"],
        third_tip
    );
    git(&["fetch", remote.as_str(), "main"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, third_tip);
    assert_eq!(git(&["rev-parse", "FETCH_HEAD^1"]).await, second_tip);
    assert_eq!(git(&["show", "FETCH_HEAD:second.txt"]).await, "second");
    worker.abort();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn branch_queue_overrides_isolate_methods_build_limits_and_check_deadlines() {
    use crate::{
        app,
        cells::repository::pull_auto_merge::{Change, Edit, Outcome, QueueTest, Stop},
    };

    let h = Harness::new(false).await;
    let alice = h.login().await;
    let settings = format!("{REPO}/settings/merge-methods");
    let write = async |method, route: &str, body| h.write(method, route, &alice, body).await;
    let token: Value = write(
        Method::POST,
        "/api/git-token",
        json!({
            "owner":"team","repository":"private","access":"write",
        }),
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
    git(&["push", remote.as_str(), "main:refs/heads/stable"]).await;
    let base = git(&["rev-parse", "main"]).await;
    let status = async |oid: &str| {
        write(
            Method::POST,
            &format!("{REPO}/statuses/{oid}"),
            json!({
                "request_id":Uuid::now_v7(),"context":"ci","state":"success",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    status(&head).await;
    write(Method::PUT, &format!("{REPO}/settings/branch-protections"), json!({
        "expected_version":0, "rules":(["main","stable"].map(|branch| json!({
            "branch":branch,"required_approvals":0,"required_checks":["ci"],"require_merge_queue":true,
        }))),
    })).await.error_for_status().unwrap();
    let save = async |version, method: &str, main_method: Option<&str>| {
        let mut main = json!({"branch":"main","timeout_minutes":1});
        if let Some(method) = main_method {
            main["method"] = json!(method);
        }
        write(Method::PUT, &settings, json!({
            "expected_version":version,"enabled":["fast_forward"],"merge_queue_method":method,
            "merge_queue_branches":[main,{"branch":"stable","method":"squash","builds":2,"timeout_minutes":90}],
        })).await.error_for_status().unwrap();
    };
    let invalid = write(Method::PUT, &settings, json!({
        "expected_version":0,"enabled":["fast_forward"],
        "merge_queue_branches":[{"branch":"stable","builds":2},{"branch":"stable","timeout_minutes":90}],
    })).await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    save(0, "merge_commit", None).await;

    let enqueue = async |pull: &str, method: &str, request_id| {
        let detail = h.json(pull, &alice).await;
        write(
            Method::PUT,
            &format!("{pull}/auto-merge"),
            json!({
                "request_id": request_id, "version":detail["version"], "base_oid":base,
                "head_oid":head,"queue":true,"method": method,
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        detail["version"].as_u64().unwrap()
    };
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let cell = app::repository_cell(&h.server, &repo).unwrap();
    let old = u64::try_from(crate::cells::unix_now_ms().unwrap() / 1000).unwrap() - 120;
    let started = Uuid::new_v7(uuid::Timestamp::from_unix(uuid::NoContext, old, 0));
    let mut pulls = Vec::new();
    for (branch, method, builds, timeout) in
        [("main", "merge_commit", 1, 1), ("stable", "squash", 2, 90)]
    {
        let created: Value = write(
            Method::POST,
            &format!("{REPO}/pulls"),
            json!({
                "request_id":Uuid::now_v7(),"title":branch,"body":"",
                "base_ref":format!("refs/heads/{branch}"),"head_ref":"refs/heads/feature",
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
        let number = created["number"].as_u64().unwrap();
        let pull = format!("{REPO}/pulls/{number}");
        let state = h.json(&format!("{pull}/auto-merge"), &alice).await;
        assert_eq!(state["queue_method"], method);
        assert_eq!(state["queue_builds"], builds);
        assert_eq!(state["queue_timeout_minutes"], timeout);
        let request = Uuid::now_v7();
        let version = enqueue(&pull, method, request).await;
        // Give the two live workers the same already-aged check start time.
        let outcome = app::command_output(
            cell.client
                .command::<Edit>(
                    &cell.target,
                    app::mutation_identity().unwrap(),
                    Change::Test {
                        pull: number,
                        id: request,
                        attempt: QueueTest {
                            request_id: started,
                            version,
                            base_oid: base.clone(),
                            previous_oid: None,
                            order_revision: 0,
                        },
                        configured_queue_required: false,
                        ruleset_queue_required: false,
                    },
                )
                .await,
        )
        .unwrap();
        assert!(matches!(outcome, Outcome::Saved));
        if branch == "stable" {
            let early = app::command_output(
                cell.client
                    .command::<Edit>(
                        &cell.target,
                        app::mutation_identity().unwrap(),
                        Change::Disarm {
                            pull: number,
                            id: request,
                            reason: Stop::ChecksTimedOut,
                            front_revision: Some(0),
                        },
                    )
                    .await,
            )
            .unwrap();
            assert!(matches!(early, Outcome::Changed));
        }
        pulls.push(pull);
    }
    let [main, stable] = pulls.as_slice() else {
        unreachable!()
    };
    let main_state = format!("{main}/auto-merge");
    let stable_candidate = format!("{stable}/merge-candidate");
    let wait_for_stop = async |reason: &str| {
        tokio::time::timeout(Duration::from_secs(30), async {
            while h.json(&main_state, &alice).await["request"]["stopped"] != reason {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .unwrap();
    };
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    wait_for_stop("checks_timed_out").await;
    let stable_sha = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let info = h.json(&stable_candidate, &alice).await;
            if info["candidate"]["current"] == true {
                break info["candidate"]["commit"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    let _ = worker.await;
    enqueue(main, "merge_commit", Uuid::now_v7()).await;
    // An override can change and return to inheritance before the worker sees it.
    save(1, "merge_commit", Some("rebase")).await;
    save(2, "merge_commit", None).await;
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    wait_for_stop("method_disabled").await;
    // A repository default change leaves a branch with an explicit method intact.
    save(3, "rebase", None).await;
    assert_eq!(
        h.json(&stable_candidate, &alice).await["candidate"]["commit"],
        stable_sha
    );
    status(&stable_sha).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while h.json(stable, &alice).await["state"] != "merged" {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    git(&["fetch", remote.as_str(), "stable"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, stable_sha);
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{stable_sha} {base}")
    );
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
        format!("{base}\trefs/heads/main")
    );
    worker.abort();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_builds_a_checked_candidate_past_the_previous_capacity() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let write = async |method, path: &str, input| h.write(method, path, &alice, input).await;
    let token: Value = write(
        Method::POST,
        "/api/git-token",
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
    git::push_review_branches(path, remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    let base = git(&["rev-parse", "main"]).await;
    for number in 1..=10 {
        let branch = format!("topic-{number}");
        git(&["checkout", "-b", &branch, "main"]).await;
        std::fs::write(path.join(&branch), &branch).unwrap();
        git(&["add", &branch]).await;
        git(&["commit", "-m", &branch]).await;
    }
    git(&["push", remote.as_str(), "--all"]).await;

    write(
        Method::PUT,
        &format!("{REPO}/settings/merge-methods"),
        json!({"expected_version":0,"enabled":["merge_commit"],"merge_queue_builds":100}),
    )
    .await
    .error_for_status()
    .unwrap();
    write(
        Method::PUT,
        &format!("{REPO}/settings/branch-protections"),
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,
            "required_checks":["ci"],"require_merge_queue":true}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let status = async |commit: &str| {
        write(
            Method::POST,
            &format!("{REPO}/statuses/{commit}"),
            json!({
                "request_id":Uuid::now_v7(), "state":"success", "context":"ci",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    for number in 1..=10 {
        let branch = format!("topic-{number}");
        let head = git(&["rev-parse", &branch]).await;
        status(&head).await;
        let created: Value = write(
            Method::POST,
            &format!("{REPO}/pulls"),
            json!({
                "request_id":Uuid::now_v7(), "title":branch, "body":"",
                "base_ref":"refs/heads/main", "head_ref":format!("refs/heads/{branch}"),
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
        assert_eq!(created["number"], number);
        write(
            Method::PUT,
            &format!("{REPO}/pulls/{number}/auto-merge"),
            json!({
                "request_id":Uuid::now_v7(), "version":created["version"], "base_oid":base,
                "head_oid":head, "queue":true, "method":"merge_commit",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    }

    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    let candidate = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let candidate = h
                .json(&format!("{REPO}/pulls/10/merge-candidate"), &alice)
                .await["candidate"]
                .clone();
            if candidate["current"] == true {
                break candidate;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(candidate["grouped"], true);
    let tested = candidate["commit"].as_str().unwrap();
    status(tested).await;
    git(&["fetch", remote.as_str(), "refs/crab/merge-queue/10"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, tested);
    assert_eq!(
        git(&["ls-tree", "--name-only", "FETCH_HEAD"])
            .await
            .lines()
            .filter(|path| path.starts_with("topic-"))
            .count(),
        10
    );
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
        format!("{base}\trefs/heads/main")
    );
    worker.abort();
    h.close().await;
}
