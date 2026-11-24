use super::*;
use crate::test_support::{
    git,
    http::{Harness, browser::Browser, github_workflow_delivery},
};
use reqwest::Method;

mod dispatch;

const SETTINGS: &str = "/api/repos/team/private/settings/integrations";
const ROOT: &str = "/api/repos/team/private";
const PULL: &str = "/api/repos/team/private/pulls/1";

#[tokio::test]
async fn signed_workflow_checks_gate_the_exact_commit_and_survive_redelivery() {
    let h = Harness::with_ci_key("").await;
    let alice = h.login().await;
    let id = Uuid::now_v7();
    let connect =
        json!({"kind":"connect","id":id,"name":"ci/test","github_repository":123,"workflow":456});
    let connected: Value = h
        .write(Method::POST, SETTINGS, &alice, connect.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let retry: Value = h
        .write(Method::POST, SETTINGS, &alice, connect.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        retry == connected,
        "a lost setup response must recover the same credential"
    );
    let history = format!("{SETTINGS}/{id}/deliveries");
    assert_eq!(h.json(&history, &alice).await, json!([]));
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        delivery_history_status(&h, &history, &bob).await,
        StatusCode::NOT_FOUND
    );
    *h.provider.mode.lock().await = "admin".into();
    let online = h.json(SETTINGS, &alice).await;
    assert_eq!(online["available"], true);
    assert!(online["items"][0].get("secret").is_none());
    let old_secret = connected["secret"].as_str().unwrap();
    let url = connected["connection"]["url"].as_str().unwrap();

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
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), remote.as_str()).await;
    let base = git::success(source.path(), &["rev-parse", "main"]).await;
    h.write(Method::PUT, &format!("{ROOT}/settings/branch-protections"), &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_integrations":[id]}]}))
        .await.error_for_status().unwrap();
    h.write(Method::POST, &format!("{ROOT}/pulls"), &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Check provider","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();
    h.write(Method::POST, &format!("{ROOT}/check-runs"), &alice,
        json!({"request_id":Uuid::now_v7(),"head_sha":head,"name":"ci/test","status":"completed","conclusion":"success","details_url":null,
            "output":{"title":"Manual report","summary":"Same name.","text":null}}))
        .await.error_for_status().unwrap();
    let waiting = h.json(PULL, &alice).await;
    assert!(!waiting["can_merge"].as_bool().unwrap());
    assert_eq!(
        waiting["merge_requirements"]["checks"][0]["integration_id"],
        json!(id)
    );

    let run = |oid: &str, attempt: u64, status: &str, conclusion: Value| {
        json!({
            "action":status,"repository":{"id":123,"full_name":"example/mirror"},
            "workflow_run":{"id":789,"workflow_id":456,"run_number":10,"run_attempt":attempt,
                "head_sha":oid,"event":"push","status":status,"conclusion":conclusion},
        })
    };
    let completed = run(&head, 1, "completed", json!("success"));
    let mut wrong_repository = completed.clone();
    wrong_repository["repository"]["id"] = json!(999);
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            old_secret,
            Uuid::now_v7(),
            &completed,
            &wrong_repository
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            old_secret,
            Uuid::now_v7(),
            &wrong_repository,
            &wrong_repository
        )
        .await,
        StatusCode::NOT_FOUND
    );
    let mut wrong_workflow = completed.clone();
    wrong_workflow["workflow_run"]["workflow_id"] = json!(987);
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            old_secret,
            Uuid::now_v7(),
            &wrong_workflow,
            &wrong_workflow
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let mut wrong_head = run(&base, 1, "completed", json!("success"));
    wrong_head["workflow_run"]["id"] = json!(788);
    wrong_head["workflow_run"]["run_number"] = json!(9);
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            old_secret,
            Uuid::now_v7(),
            &wrong_head,
            &wrong_head
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.json(PULL, &alice).await["can_merge"], false);

    let delivery = Uuid::now_v7();
    for _ in 0..2 {
        assert_eq!(
            github_workflow_delivery(&h, url, old_secret, delivery, &completed, &completed).await,
            StatusCode::NO_CONTENT
        );
    }
    let recent = h.json(&history, &alice).await;
    let rows = recent.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], json!(delivery));
    assert_eq!(rows[0]["oid"], head);
    assert_eq!(rows[0]["conclusion"], "success");
    assert_eq!(rows[0]["updated_check"], true);
    assert_eq!(rows[1]["oid"], base);
    let pull = h.json(PULL, &alice).await;
    let check = &pull["merge_requirements"]["checks"][0];
    assert_eq!(check["provider"], "github_actions");
    assert_eq!(check["head_sha"], head);
    assert_eq!(
        check["target_url"],
        "https://github.com/example/mirror/actions/runs/789"
    );
    assert_eq!(pull["can_merge"], true);

    let rotated: Value = h
        .write(
            Method::POST,
            SETTINGS,
            &alice,
            json!({"kind":"rotate","id":id,"expected":id,"credential":Uuid::now_v7()}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let current_secret = rotated["secret"].as_str().unwrap();
    let second_attempt = run(&head, 2, "in_progress", Value::Null);
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            old_secret,
            Uuid::now_v7(),
            &second_attempt,
            &second_attempt
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            current_secret,
            Uuid::now_v7(),
            &second_attempt,
            &second_attempt
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            current_secret,
            Uuid::now_v7(),
            &completed,
            &completed
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let recent = h.json(&history, &alice).await;
    assert_eq!(recent[0]["attempt"], 1);
    assert_eq!(recent[0]["updated_check"], false);
    assert_eq!(recent[1]["attempt"], 2);
    assert_eq!(recent[1]["updated_check"], true);
    assert_eq!(recent[1]["status"], "in_progress");
    let pending = h.json(PULL, &alice).await;
    assert_eq!(
        pending["merge_requirements"]["checks"][0]["state"],
        "pending"
    );
    assert_eq!(pending["can_merge"], false);
    let mark = |version| {
        json!({
            "request_id":Uuid::now_v7(),"version":version,"method":"fast_forward","base_oid":base,"head_oid":head
        })
    };
    assert_eq!(
        h.write(
            Method::POST,
            &format!("{PULL}/merge"),
            &alice,
            mark(pending["version"].clone())
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let done = run(&head, 2, "completed", json!("success"));
    assert_eq!(
        github_workflow_delivery(&h, url, current_secret, Uuid::now_v7(), &done, &done).await,
        StatusCode::NO_CONTENT
    );
    let checks = h
        .json(&format!("{ROOT}/commits/{head}/check-runs"), &alice)
        .await;
    assert_eq!(checks["items"].as_array().unwrap().len(), 3);
    assert_eq!(checks["items"][0]["version"], 2);
    let mut malformed = completed.clone();
    malformed["repository"]["full_name"] = json!(format!("example/{}", "x".repeat(101)));
    assert_eq!(
        github_workflow_delivery(
            &h,
            url,
            current_secret,
            Uuid::now_v7(),
            &malformed,
            &malformed
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let mut newest = Uuid::nil();
    for _ in 0..50 {
        newest = Uuid::now_v7();
        assert_eq!(
            github_workflow_delivery(&h, url, current_secret, newest, &done, &done).await,
            StatusCode::NO_CONTENT
        );
    }
    let bounded = h.json(&history, &alice).await;
    assert_eq!(bounded.as_array().unwrap().len(), 50);
    assert_eq!(bounded[0]["id"], json!(newest));
    assert_eq!(
        github_workflow_delivery(&h, url, current_secret, delivery, &completed, &completed).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.json(&history, &alice).await, bounded);
    let ready = h.json(PULL, &alice).await;
    h.write(
        Method::POST,
        &format!("{PULL}/merge"),
        &alice,
        mark(ready["version"].clone()),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        SETTINGS,
        &alice,
        json!({"kind":"disconnect","id":id,"expected":rotated["connection"]["credential_version"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        delivery_history_status(&h, &history, &alice).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        github_workflow_delivery(&h, url, current_secret, delivery, &completed, &completed).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.write(Method::POST, SETTINGS, &alice, connect)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json(PULL, &alice).await["merge_requirements"]["checks"][0]["state"],
        Value::Null
    );
    h.close().await;
}

async fn delivery_history_status(h: &Harness, path: &str, session: &str) -> StatusCode {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(reqwest::header::COOKIE, session)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native repository Cell"]
async fn native_browser_shows_a_new_github_actions_delivery_after_refresh() {
    let h = Harness::with_ci_key("").await;
    let alice = h.login().await;
    let connected: Value = h
        .write(
            Method::POST,
            SETTINGS,
            &alice,
            json!({"kind":"connect","id":Uuid::now_v7(),"name":"ci/history","github_repository":123,"workflow":456}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
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
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), remote.as_str()).await;

    let browser = Browser::new("ci-deliveries");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/integrations", h.origin),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Recent deliveries"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Recent deliveries for ci/history",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No accepted deliveries yet."])
        .await;

    let id = Uuid::now_v7();
    let report = json!({
        "action":"completed","repository":{"id":123,"full_name":"example/mirror"},
        "workflow_run":{"id":789,"workflow_id":456,"run_number":10,"run_attempt":1,
            "head_sha":head,"event":"push","status":"completed","conclusion":"success"},
    });
    assert_eq!(
        github_workflow_delivery(
            &h,
            connected["connection"]["url"].as_str().unwrap(),
            connected["secret"].as_str().unwrap(),
            id,
            &report,
            &report,
        )
        .await,
        StatusCode::NO_CONTENT
    );
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Refresh deliveries",
        ])
        .await;
    browser.run(&["wait", "--text", &id.to_string()]).await;
    let snapshot = browser.run(&["snapshot"]).await;
    assert!(snapshot.contains(&head));
    assert!(snapshot.contains("Check updated"));
    assert!(snapshot.contains("Open GitHub Actions run"));
    browser.run(&["close"]).await;
    h.close().await;
}
