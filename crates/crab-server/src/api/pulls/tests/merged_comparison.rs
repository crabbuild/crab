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
const SAVED: &str = "/api/repos/team/private/pulls/1/merged-comparison";

async fn retrieve(h: &Harness, alice: &str, path: &str) -> reqwest::Response {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(reqwest::header::COOKIE, alice)
        .send()
        .await
        .unwrap()
}

async fn squash_and_delete(h: &Harness, alice: &str) -> (Vec<String>, Value) {
    let remote = remote(h, alice).await;
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    open_pull(h, alice, remote.as_str(), path).await;
    assert_eq!(
        retrieve(h, alice, SAVED).await.status(),
        StatusCode::NOT_FOUND
    );
    for (setting, input) in [
        (
            "branch-protections",
            json!({"expected_version":0,"rules":[]}),
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
    git::success(path, &["checkout", "feature"]).await;
    std::fs::write(path.join("README.md"), "base\nfeature\nfinished feature\n").unwrap();
    git::success(path, &["commit", "-am", "Finish reviewed feature"]).await;
    git::success(path, &["push", remote.as_str(), "feature"]).await;
    git::success(path, &["checkout", "main"]).await;
    std::fs::write(path.join("BASE.txt"), "base branch only\n").unwrap();
    git::success(path, &["add", "BASE.txt"]).await;
    git::success(path, &["commit", "-m", "Add a base-only file"]).await;
    git::success(path, &["push", remote.as_str(), "main"]).await;
    let commits: Vec<_> = git::success(path, &["rev-list", "main..feature"])
        .await
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(commits.len(), 2);
    let head = &commits[0];
    let compare = h
        .json(
            &format!("{ROOT}/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true"),
            alice,
        )
        .await;
    let changes = compare["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["path"], "README.md");
    let comment = json!({"request_id":Uuid::now_v7(),"base_oid":compare["base"],"head_oid":head,
        "path_hex":changes[0]["path_hex"],"side":"new","start_line":2,"end_line":2,
        "body":"Keep this inline review after branch cleanup"});
    h.write(Method::POST, &format!("{PULL}/threads"), alice, comment)
        .await
        .error_for_status()
        .unwrap();
    let pull = h.json(PULL, alice).await;
    h.write(
        Method::POST,
        &format!("{PULL}/merge"),
        alice,
        json!({
            "request_id":Uuid::now_v7(),"version":pull["version"],"method":"squash",
            "base_oid":pull["base_oid"],"head_oid":head,
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    git::success(path, &["push", remote.as_str(), "--delete", "feature"]).await;
    let general = format!(
        "{ROOT}/changes?rev={head}&base={}&merge_base=true",
        compare["base_tip"].as_str().unwrap()
    );
    assert_eq!(
        retrieve(h, alice, &general).await.status(),
        StatusCode::NOT_FOUND
    );
    (commits, compare)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saved_merge_comparison_keeps_original_commits_files_and_anchors() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (commits, compare) = squash_and_delete(&h, &alice).await;
    let head = commits[0].as_str();
    let saved = h.json(SAVED, &alice).await;
    for field in ["base", "base_tip", "commit", "changes"] {
        assert_eq!(saved[field], compare[field], "{field}");
    }
    let path = saved["changes"][0]["path_hex"].as_str().unwrap();
    let file = h
        .json(&format!("{SAVED}/file?path_hex={path}"), &alice)
        .await;
    assert_eq!(file["old"]["text"], "base\n");
    assert_eq!(file["new"]["text"], "base\nfeature\nfinished feature\n");
    let not_changed = format!(
        "{SAVED}/file?path_hex={}",
        crate::api::encode_hex(b"BASE.txt")
    );
    assert_eq!(
        retrieve(&h, &alice, &not_changed).await.status(),
        StatusCode::NOT_FOUND
    );
    let override_head = format!("{SAVED}?head_oid={head}");
    assert_eq!(
        retrieve(&h, &alice, &override_head).await.status(),
        StatusCode::BAD_REQUEST
    );
    let reply: Value = h.json(&format!("{PULL}/threads/1"), &alice).await;
    assert_eq!(reply["current"], true);
    assert_eq!(reply["head_oid"], head);
    let history = format!("{SAVED}/commits");
    assert_eq!(
        retrieve(&h, &alice, &format!("{history}?limit=201"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let page = h.json(&format!("{history}?limit=1"), &alice).await;
    assert_eq!(page["items"][0]["oid"], head);
    let cursor = page["next"].as_str().unwrap();
    let page = h
        .json(&format!("{history}?limit=1&cursor={cursor}"), &alice)
        .await;
    assert_eq!(page["items"][0]["oid"], commits[1]);
    assert!(page["next"].is_null());
    let detail = h.json(&format!("{history}/{head}"), &alice).await;
    assert_eq!(detail["metadata"]["oid"], head);
    assert_eq!(detail["base"], commits[1]);
    assert_eq!(detail["changes"].as_array().unwrap().len(), 1);
    let file = h
        .json(&format!("{history}/{head}/file?path_hex={path}"), &alice)
        .await;
    assert_eq!(file["old"]["text"], "base\nfeature\n");
    assert_eq!(file["new"]["text"], "base\nfeature\nfinished feature\n");
    for excluded in ["base", "base_tip"] {
        let oid = compare[excluded].as_str().unwrap();
        for route in [
            format!("{history}/{oid}"),
            format!("{history}/{oid}/file?path_hex={path}"),
        ] {
            assert_eq!(
                retrieve(&h, &alice, &route).await.status(),
                StatusCode::NOT_FOUND
            );
        }
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_reads_the_merged_history_after_source_deletion() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (commits, _) = squash_and_delete(&h, &alice).await;
    let browser = Browser::new("saved-pull-diff");
    browser
        .run(&["open", &format!("{}/team/private/pull/1/files", h.origin)])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "1 changed file"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Open review thread #1",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "Keep this inline review after branch cleanup",
        ])
        .await;
    browser
        .run(&["open", &format!("{}/team/private/pull/1/commits", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "Original commits at merge time."])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Finish reviewed feature",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--url",
            &format!("**/pull/1/commits/{}*", commits[0]),
        ])
        .await;
    browser.run(&["wait", "--text", "1 changed file"]).await;
    let rendered_addition = r#"document.querySelector('[data-change-path="README.md"] diffs-container')?.shadowRoot?.querySelector('[data-line][data-line-type="change-addition"]')?.textContent?.includes('finished feature') === true"#;
    browser.run(&["wait", "--fn", rendered_addition]).await;
    browser.run(&["set", "viewport", "390", "844"]).await;
    browser.run(&["reload"]).await;
    browser.run(&["wait", "--text", "1 changed file"]).await;
    browser.run(&["wait", "--fn", rendered_addition]).await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.documentElement.scrollWidth <= window.innerWidth",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Back to commits",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Original commits at merge time."])
        .await;
    drop(browser);
    h.close().await;
}
