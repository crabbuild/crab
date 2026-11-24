use super::branch_restore::{open_pull, remote};
use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

const ROOT: &str = "/api/repos/team/private";
const PULL: &str = "/api/repos/team/private/pulls/1";

async fn squash_tested_candidate(h: &Harness, alice: &str) -> (String, String, String) {
    let remote = remote(h, alice).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let (head, opened) = open_pull(h, alice, remote.as_str(), path).await;
    for (setting, input) in [
        (
            "branch-protections",
            json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_checks":["ci/required"]}]}),
        ),
        (
            "merge-methods",
            json!({"expected_version":0,"enabled":["squash"]}),
        ),
    ] {
        h.write(
            Method::PUT,
            &format!("{ROOT}/settings/{setting}"),
            alice,
            input,
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let route = format!("{PULL}/merge-candidate");
    let mut preview = h.json(&route, alice).await["preview"].clone();
    preview["method"] = json!("squash");
    let prepared: Value = h
        .write(Method::POST, &route, alice, preview)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let tested = prepared["commit"].as_str().unwrap().to_owned();
    assert_ne!(head, tested);
    for (oid, name) in [(&head, "reviewed-head"), (&tested, "tested-squash")] {
        h.write(Method::POST, &format!("{ROOT}/check-runs"), alice, json!({
            "request_id":Uuid::now_v7(),"head_sha":oid,"name":name,"status":"completed","conclusion":"success",
            "details_url":null,"output":{"title":format!("Result for {name}"),"summary":format!("Report for {name}")},
        })).await.error_for_status().unwrap();
        for context in ["ci/required".to_owned(), format!("status/{name}")] {
            h.write(
                Method::POST,
                &format!("{ROOT}/statuses/{oid}"),
                alice,
                json!({
                    "request_id":Uuid::now_v7(),"context":context,"state":"success",
                }),
            )
            .await
            .error_for_status()
            .unwrap();
        }
    }
    let merged: Value = h.write(Method::POST, &format!("{PULL}/merge"), alice, json!({
        "request_id":Uuid::now_v7(),"version":opened["version"],"method":"squash","base_oid":opened["base_oid"],
        "head_oid":head,"tested_candidate_oid":tested,
    })).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(merged["merge"]["tested_candidate_oid"], tested);
    git::success(path, &["push", remote.as_str(), "--delete", "feature"]).await;
    let base = opened["base_oid"].as_str().unwrap().to_owned();
    (head, tested, base)
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_merge_authorizes_only_exact_recorded_check_reads() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (head, tested, base) = squash_tested_candidate(&h, &alice).await;
    for (oid, name) in [(&head, "reviewed-head"), (&tested, "tested-squash")] {
        let checks = format!("{ROOT}/commits/{oid}/check-runs");
        let result = h.json(&format!("{checks}?pull=1"), &alice).await;
        assert_eq!(result["sha"], *oid);
        assert_eq!(result["items"][0]["name"], name);
        let number = result["items"][0]["id"].as_u64().unwrap();
        let run = h.json(&format!("{checks}/{number}?pull=1"), &alice).await;
        assert_eq!(run["head_sha"], *oid);
        assert_eq!(run["output"]["summary"], format!("Report for {name}"));
        let status = h
            .json(&format!("{ROOT}/commits/{oid}/status?pull=1"), &alice)
            .await;
        assert!(
            status["statuses"]
                .as_array()
                .unwrap()
                .iter()
                .any(|status| status["context"] == format!("status/{name}"))
        );
    }
    for address in [
        format!("{ROOT}/commits/{head}/check-runs"),
        format!("{ROOT}/commits/{head}/check-runs?pull=2"),
        format!("{ROOT}/commits/{base}/check-runs?pull=1"),
    ] {
        let response = h
            .http
            .get(format!("{}{address}", h.origin))
            .header(reqwest::header::COOKIE, &alice)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{address}");
    }
    assert_eq!(
        h.write(
            Method::POST,
            &format!("{ROOT}/statuses/{head}?pull=1"),
            &alice,
            json!({
                "request_id":Uuid::now_v7(),"context":"late-write","state":"failure"
            })
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the embedded UI with native Cells"]
async fn native_browser_reads_both_completed_merge_check_commits() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (head, tested, _) = squash_tested_candidate(&h, &alice).await;
    let browser = Browser::new("merge-checks");
    browser
        .run(&["open", &format!("{}/team/private/pull/1/checks", h.origin)])
        .await;
    browser.sign_in().await;
    browser
        .run(&["wait", &format!("code[title='{head}']")])
        .await;
    browser
        .run(&["wait", "--text", "status/reviewed-head"])
        .await;
    browser
        .run(&["wait", "--text", "Report for reviewed-head"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Tested merge checks",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", &format!("code[title='{tested}']")])
        .await;
    browser
        .run(&["wait", "--text", "status/tested-squash"])
        .await;
    browser
        .run(&["wait", "--text", "Report for tested-squash"])
        .await;
    drop(browser);
    h.close().await;
}
