use std::sync::Arc;

use axum::{Router, body::Bytes, extract::State, http::HeaderMap, routing::post};
use reqwest::Method;
use tokio::sync::Mutex;

use super::*;
use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};

mod discussion;
mod synchronize;

const PATH: &str = "/api/repos/team/private/settings/webhooks";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Received {
    id: Uuid,
    event: String,
    signature: String,
    body: Bytes,
}

async fn receiver(
    fail_first_pull: bool,
) -> (
    url::Url,
    Arc<Mutex<Vec<Received>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/events", listener.local_addr().unwrap());
    let rows = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/events",
            post(
                async move |State(rows): State<Arc<Mutex<Vec<Received>>>>,
                            headers: HeaderMap,
                            body: Bytes| {
                    let mut rows = rows.lock().await;
                    let event = headers["x-crab-event"].to_str().unwrap();
                    let fail = fail_first_pull
                        && event == "pull_request"
                        && !rows.iter().any(|received| received.event == "pull_request");
                    rows.push(Received {
                        id: Uuid::parse_str(headers["x-crab-delivery"].to_str().unwrap()).unwrap(),
                        event: event.into(),
                        signature: headers["x-hub-signature-256"].to_str().unwrap().into(),
                        body,
                    });
                    if fail {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        )
        .with_state(rows.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url::Url::parse(&endpoint).unwrap(), rows, task)
}

async fn git_remote(h: &Harness, account: &str, owner: &str, name: &str) -> url::Url {
    let credential: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            account,
            json!({"owner":owner,"repository":name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/{owner}/{name}.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(credential["token"].as_str()).unwrap();
    remote
}

async fn stock_push(h: &Harness, account: &str) -> String {
    let clone = tempfile::tempdir().unwrap();
    let remote = git_remote(h, account, "team", "private").await;
    git::push_review_branches(clone.path(), remote.as_str()).await
}

async fn checkout_feature(h: &Harness, account: &str) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let remote = git_remote(h, account, "team", "private").await;
    let cloned = git::git(
        directory.path(),
        &["clone", "--branch", "feature", remote.as_str(), "."],
    )
    .await;
    assert!(cloned.status.success(), "stock Git clone failed");
    directory
}

async fn advance_feature(checkout: &std::path::Path, remote: &str) -> String {
    git::success(
        checkout,
        &["commit", "--allow-empty", "-m", "Advance feature"],
    )
    .await;
    let head = git::success(checkout, &["rev-parse", "HEAD"]).await;
    let pushed = git::git(checkout, &["push", remote, "HEAD:refs/heads/feature"]).await;
    assert!(pushed.status.success(), "stock Git push failed");
    head
}

async fn open(h: &Harness, admin: &str, request: &Value) {
    h.write(
        Method::POST,
        "/api/repos/team/private/pulls",
        admin,
        request.clone(),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn drain(h: &Harness) {
    let repo = &h.server.repositories.values()[0];
    crate::server::webhooks::drain(&h.server, repo, h.server.webhook_sender.as_ref().unwrap())
        .await
        .unwrap();
}

#[tokio::test]
async fn outbound_delivery_binds_publication_and_replays_signed_bytes() {
    let (endpoint, received, target) = receiver(false).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    let id = Uuid::now_v7();
    let configure =
        json!({"kind":"configure","id":id,"expected":null,"push":true,"pull_request":true});
    let credential: Value = h
        .write(Method::POST, PATH, &alice, configure.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let replay: Value = h
        .write(Method::POST, PATH, &alice, configure)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        credential == replay,
        "setup replay must return the original signing secret"
    );
    let secret = credential["secret"].as_str().unwrap();
    assert!(h.json(PATH, &alice).await.get("secret").is_none());
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{PATH}", h.origin))
            .header(reqwest::header::COOKIE, bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    let repository = &h.server.repositories.values()[0];
    crate::server::webhooks::reserve_push(&h.server, repository, &"9".repeat(64))
        .await
        .unwrap();
    drain(&h).await;
    assert!(
        received.lock().await.is_empty(),
        "an uncommitted plan must not leave the server"
    );
    let head = stock_push(&h, &alice).await;
    let request = json!({"request_id":Uuid::now_v7(),"title":"Trigger CI","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"});
    open(&h, &alice, &request).await;
    open(&h, &alice, &request).await;
    drain(&h).await;
    let rows = received.lock().await.clone();
    assert_eq!(rows.len(), 3, "two Git pushes and one PR creation");
    for row in &rows {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&row.body);
        let expected = blake3::Hash::from_bytes(mac.finalize().into_bytes().into());
        assert_eq!(row.signature, format!("sha256={expected}"));
    }
    let feature = rows
        .iter()
        .find(|row| {
            row.event == "push" && String::from_utf8_lossy(&row.body).contains("refs/heads/feature")
        })
        .unwrap();
    let body: Value = serde_json::from_slice(&feature.body).unwrap();
    assert_eq!(
        body["data"]["updates"],
        json!([{"ref":"refs/heads/feature","before":null,"after":head}])
    );
    let opened = rows.iter().find(|row| row.event == "pull_request").unwrap();
    let body: Value = serde_json::from_slice(&opened.body).unwrap();
    assert_eq!(body["data"]["action"], "opened");
    assert_eq!(body["data"]["head_sha"], head);
    let deliveries = h.json(PATH, &alice).await;
    let previous = deliveries["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["id"] == json!(opened.id))
        .unwrap();
    assert_eq!(previous["status"], "delivered");
    let retry = json!({"kind":"redeliver","id":opened.id,"version":previous["version"]});
    h.write(Method::POST, PATH, &alice, retry.clone())
        .await
        .error_for_status()
        .unwrap();
    drain(&h).await;
    h.write(Method::POST, PATH, &alice, retry)
        .await
        .error_for_status()
        .unwrap();
    drain(&h).await;
    assert_eq!(
        received.lock().await.as_slice(),
        &[
            rows[0].clone(),
            rows[1].clone(),
            rows[2].clone(),
            opened.clone()
        ]
    );
    h.write(
        Method::POST,
        PATH,
        &alice,
        json!({"kind":"disable","expected":id}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(PATH, &alice).await["active"], false);
    h.close().await;
    target.abort();
    let _ = target.await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the embedded UI with native Git and Cells"]
async fn native_browser_isolates_subscriptions_and_delivery_history_by_destination() {
    let (first, first_events, first_target) = receiver(false).await;
    let (second, second_events, second_target) = receiver(false).await;
    let keys = [&first, &second].map(crate::server::webhooks::destination);
    let h = Harness::with_webhook_receivers([first, second], None).await;
    let alice = h.login().await;
    stock_push(&h, &alice).await;
    let no_selection = h.write(Method::POST, PATH, &alice,
        json!({"kind":"configure","id":Uuid::now_v7(),"expected":null,"push":true,"pull_request":true})).await;
    assert_eq!(no_selection.status(), StatusCode::BAD_REQUEST);

    let browser = Browser::new("webhook-destinations");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/webhooks", h.origin),
        ])
        .await;
    browser.sign_in().await;
    let picker = "#webhook-destination-picker";
    let select = async |key: &str| {
        browser.run(&["wait", picker]).await;
        browser.run(&["select", picker, key]).await;
        browser
            .run(&[
                "wait",
                "--fn",
                &format!("document.querySelector('{picker}')?.value === '{key}'"),
            ])
            .await;
    };
    for key in &keys {
        select(key).await;
        browser
            .run(&[
                "find",
                "role",
                "button",
                "click",
                "--name",
                "Enable webhook",
                "--exact",
            ])
            .await;
        browser.run(&["wait", "--text", "Hide secret"]).await;
        browser.run(&["wait", "--text", "Disable webhook"]).await;
    }

    open(
        &h,
        &alice,
        &json!({
            "request_id":Uuid::now_v7(),"title":"Notify both receivers","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
        }),
    )
    .await;
    let checkout = checkout_feature(&h, &alice).await;
    let remote = git_remote(&h, &alice, "team", "private").await;
    let head = advance_feature(checkout.path(), remote.as_str()).await;
    drain(&h).await;

    let first_rows = first_events.lock().await.clone();
    let second_rows = second_events.lock().await.clone();
    let paths = keys
        .each_ref()
        .map(|key| format!("{PATH}?destination={key}"));
    let repo = h.server.repositories.values()[0].id;
    let mut secrets = Vec::new();
    for (path, rows) in paths.iter().zip([&first_rows, &second_rows]) {
        let view = h.json(path, &alice).await;
        let subscription = Uuid::parse_str(view["subscription"]["id"].as_str().unwrap()).unwrap();
        let secret = crate::server::webhooks::secret(&h.server, repo, subscription).unwrap();
        assert_eq!(rows.len(), 3);
        let history = view["deliveries"].as_array().unwrap();
        assert_eq!(history.len(), rows.len());
        for row in rows {
            assert!(
                history
                    .iter()
                    .any(|entry| entry["id"] == json!(row.id) && entry["status"] == "delivered")
            );
            let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
            mac.update(&row.body);
            let digest = blake3::Hash::from_bytes(mac.finalize().into_bytes().into());
            assert_eq!(row.signature, format!("sha256={digest}"));
        }
        assert!(rows.iter().any(|row| {
            let payload: Value = serde_json::from_slice(&row.body).unwrap();
            payload["data"]["action"] == "synchronize" && payload["data"]["head_sha"] == head
        }));
        secrets.push(secret);
    }
    assert!(
        secrets[0] != secrets[1],
        "receivers must use different secrets"
    );
    assert!(
        first_rows
            .iter()
            .all(|first| second_rows.iter().all(|second| first.id != second.id))
    );
    let delivery = &h.json(&paths[0], &alice).await["deliveries"][0];
    let cross_retry = h
        .write(
            Method::POST,
            &paths[1],
            &alice,
            json!({"kind":"redeliver","id":delivery["id"],"version":delivery["version"]}),
        )
        .await;
    assert_eq!(cross_retry.status(), StatusCode::CONFLICT);

    select(&keys[0]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Disable webhook",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Enable webhook"]).await;
    select(&keys[1]).await;
    browser
        .run(&["wait", "--text", "Pull Request Head Updated"])
        .await;
    advance_feature(checkout.path(), remote.as_str()).await;
    drain(&h).await;
    assert_eq!(first_events.lock().await.len(), 3);
    assert_eq!(second_events.lock().await.len(), 5);
    drop(browser);
    h.close().await;
    for target in [first_target, second_target] {
        target.abort();
        let _ = target.await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the embedded UI with native Git and Cells"]
async fn native_browser_enables_an_outbound_webhook_and_reads_the_delivered_pull() {
    let (endpoint, received, target) = receiver(true).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    stock_push(&h, &alice).await;
    let worker = tokio::spawn(crate::server::webhooks::run(h.server.clone()));
    let browser = Browser::new("webhooks");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/webhooks", h.origin),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Enable webhook"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Enable webhook",
        ])
        .await;
    browser.run(&["wait", "--text", "Hide secret"]).await;
    browser
        .run(&["find", "role", "button", "click", "--name", "Hide secret"])
        .await;
    open(&h,&alice,&json!({"request_id":Uuid::now_v7(),"title":"Browser configured hook","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            if !received.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
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
    browser.run(&["wait", "--text", "Pull request #1"]).await;
    tokio::time::timeout(std::time::Duration::from_secs(18), async {
        loop {
            if received.lock().await.len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    // The receiver and Cell commit are separate. Wait for the canonical
    // completion before verifying the browser's refreshed presentation.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if h.json(PATH, &alice).await["deliveries"][0]["status"] == "delivered" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
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
    browser
        .run(&["wait", "--text", "Delivery details · 2 attempts"])
        .await;
    let snapshot = browser.run(&["snapshot"]).await;
    assert!(snapshot.contains("Delivered"));
    {
        let rows = received.lock().await;
        assert_eq!(rows[0], rows[1]);
        assert_eq!(rows[0].event, "pull_request");
    }
    let view = h.json(PATH, &alice).await;
    assert_eq!(
        view["deliveries"][0]["recent_attempts"][1]["http_status"],
        503
    );
    let checkout = checkout_feature(&h, &alice).await;
    let head = advance_feature(checkout.path(), "origin").await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            let history = h.json(PATH, &alice).await;
            if history["deliveries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["head_change"]["after"] == head && row["status"] == "delivered")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
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
    browser
        .run(&["wait", "--text", "Pull Request Head Updated"])
        .await;
    browser
        .run(&["wait", &format!("code[title='{head}']")])
        .await;
    browser.run(&["close"]).await;
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
    target.abort();
    let _ = target.await;
}

async fn enable_push(h: &Harness, account: &str) {
    h.write(
        Method::POST,
        PATH,
        account,
        json!({"kind":"configure","id":Uuid::now_v7(),"expected":null,"push":true,"pull_request":false}),
    )
    .await
    .error_for_status()
    .unwrap();
}

#[tokio::test]
async fn browser_writes_emit_exact_public_refs_even_when_a_branch_address_is_reused() {
    let (endpoint, received, target) = receiver(false).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    let head = stock_push(&h, &alice).await;
    enable_push(&h, &alice).await;
    let root = "/api/repos/team/private";
    let branches = format!("{root}/branches");
    let file = json!({"branch":"refs/heads/feature","expected_head":head,"path_hex":"63692e747874","content":"Run browser CI\n","message":"Trigger browser CI"});
    let edited: Value = h
        .write(
            Method::POST,
            &format!("{root}/contents"),
            &alice,
            file.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let commit = &edited["commit"];
    assert_eq!(
        h.write(Method::POST, &format!("{root}/contents"), &alice, file)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let create = json!({"name":"browser-ci","source_oid":commit});
    h.write(Method::POST, &branches, &alice, create.clone())
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.write(Method::POST, &branches, &alice, create.clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.write(
        Method::DELETE,
        &branches,
        &alice,
        json!({"name":"browser-ci","expected_oid":commit}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(Method::POST, &branches, &alice, create)
        .await
        .error_for_status()
        .unwrap();

    let release = json!({"request_id":Uuid::now_v7(),"tag_name":"v1.0.0","target_oid":commit,"title":"Build from Crab","body":"","prerelease":false,"draft":false});
    h.write(
        Method::POST,
        &format!("{root}/releases"),
        &alice,
        release.clone(),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(Method::POST, &format!("{root}/releases"), &alice, release)
        .await
        .error_for_status()
        .unwrap();
    drain(&h).await;
    let expected = [
        json!({"ref":"refs/heads/feature","before":head,"after":commit}),
        json!({"ref":"refs/heads/browser-ci","before":null,"after":commit}),
        json!({"ref":"refs/heads/browser-ci","before":commit,"after":null}),
        json!({"ref":"refs/heads/browser-ci","before":null,"after":commit}),
        json!({"ref":"refs/tags/v1.0.0","before":null,"after":commit}),
    ];
    {
        let rows = received.lock().await;
        assert_eq!(rows.len(), expected.len());
        let mut ids = std::collections::HashSet::new();
        for (row, update) in rows.iter().zip(expected) {
            assert_eq!(row.event, "push");
            assert!(ids.insert(row.id));
            let body: Value = serde_json::from_slice(&row.body).unwrap();
            assert_eq!(body["data"]["updates"], json!([update]));
        }
    }

    h.write(
        Method::PUT,
        &format!("{root}/settings/branch-protections"),
        &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_checks":["ci"]}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    open(&h, &alice, &json!({"request_id":Uuid::now_v7(),"title":"Merge a browser change","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await;
    let candidate_route = format!("{root}/pulls/1/merge-candidate");
    let preview = h.json(&candidate_route, &alice).await["preview"].clone();
    let candidate: Value = h
        .write(Method::POST, &candidate_route, &alice, preview.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    drain(&h).await;
    assert_eq!(
        h.json(PATH, &alice).await["deliveries"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let merge_route = format!("{root}/pulls/1/merge");
    let merge = json!({"request_id":Uuid::now_v7(),"method":"merge_commit","version":preview["version"],"base_oid":preview["base_oid"],"head_oid":commit,"tested_candidate_oid":candidate["commit"]});
    let report = async |oid: &Value| {
        h.write(
            Method::POST,
            &format!("{root}/statuses/{}", oid.as_str().unwrap()),
            &alice,
            json!({"request_id":Uuid::now_v7(),"context":"ci","state":"success"}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    report(commit).await;
    assert_eq!(
        h.write(Method::POST, &merge_route, &alice, merge.clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    drain(&h).await;
    assert_eq!(received.lock().await.len(), 5);
    report(&candidate["commit"]).await;
    h.drop_next_merge_reply
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let lost = h
        .write(Method::POST, &merge_route, &alice, merge.clone())
        .await
        .error_for_status()
        .unwrap();
    assert!(lost.bytes().await.unwrap().is_empty());
    h.write(Method::POST, &merge_route, &alice, merge)
        .await
        .error_for_status()
        .unwrap();
    drain(&h).await;
    {
        let rows = received.lock().await;
        assert_eq!(rows.len(), 6);
        let body: Value = serde_json::from_slice(&rows[5].body).unwrap();
        assert_eq!(
            body["data"]["updates"],
            json!([{"ref":"refs/heads/main","before":preview["base_oid"],"after":candidate["commit"]}])
        );
    }
    h.close().await;
    target.abort();
    let _ = target.await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and embedded assets for browser Git write and delivery verification"]
async fn native_browser_file_edit_triggers_a_push_webhook() {
    let (endpoint, received, target) = receiver(false).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    let before = stock_push(&h, &alice).await;
    enable_push(&h, &alice).await;
    let worker = tokio::spawn(crate::server::webhooks::run(h.server.clone()));
    let browser = Browser::new("edit-webhook");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/edit/feature/README.md", h.origin),
        ])
        .await;
    browser.sign_in().await;
    let editor = "[aria-label='File content']";
    browser.run(&["wait", editor]).await;
    browser.run(&["fill", editor, "Browser edit for CI"]).await;
    browser
        .run(&["fill", "#commit-message", "Trigger CI from Crab"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Commit changes",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/blob/feature/README.md*"])
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(12), async {
        loop {
            if h.json(PATH, &alice).await["deliveries"][0]["status"] == "delivered" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let refs = h.json("/api/repos/team/private/refs", &alice).await;
    let after = &refs["refs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|reference| reference["name"] == "refs/heads/feature")
        .unwrap()["oid"];
    assert_ne!(after, &json!(before));
    {
        let rows = received.lock().await;
        assert_eq!(rows.len(), 1);
        let body: Value = serde_json::from_slice(&rows[0].body).unwrap();
        assert_eq!(
            body["data"]["updates"],
            json!([{"ref":"refs/heads/feature","before":before,"after":after}])
        );
    }
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/webhooks", h.origin),
        ])
        .await;
    browser
        .run(&["wait", "--text", "Delivery details · 1 attempt"])
        .await;
    let snapshot = browser.run(&["snapshot"]).await;
    assert!(snapshot.contains("Delivered"));
    browser.run(&["close"]).await;
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
    target.abort();
    let _ = target.await;
}
