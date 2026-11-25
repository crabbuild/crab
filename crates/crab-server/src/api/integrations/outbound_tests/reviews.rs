use super::*;

const PULL: &str = "/api/repos/team/private/pulls/1";

async fn open_reviewable_pull(h: &Harness, alice: &str) -> String {
    let head = stock_push(h, alice).await;
    open(h, alice, &json!({"request_id":Uuid::now_v7(),"title":"Review delivery","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await;
    head
}

#[tokio::test]
async fn review_events_are_signed_once_for_committed_public_revisions() {
    let (endpoint, received, target) = receiver(false).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    let credentials: Value = h
        .write(
            Method::POST,
            PATH,
            &alice,
            json!({
                "kind":"configure","id":Uuid::now_v7(),"expected":null,
                "push":false,"pull_request":false,"pull_request_review":true,
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let head = open_reviewable_pull(&h, &alice).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let submit = json!({"request_id":Uuid::now_v7(),"state":"approved","body":"Approval body","commit_oid":head});
    let dismiss = json!({"request_id":Uuid::now_v7(),"version":2,"body":"Dismissal reason"});
    let reviews = format!("{PULL}/reviews");
    let first = format!("{reviews}/1");
    let dismissals = format!("{first}/dismissals");
    for (method, path, cookie, input) in [
        (Method::POST, &reviews, &bob, submit.clone()),
        (Method::POST, &reviews, &bob, submit),
        (
            Method::PATCH,
            &first,
            &bob,
            json!({"version":1,"body":"Updated approval body"}),
        ),
        (Method::PUT, &dismissals, &alice, dismiss.clone()),
        (Method::PUT, &dismissals, &alice, dismiss),
    ] {
        h.write(method, path, cookie, input)
            .await
            .error_for_status()
            .unwrap();
    }
    assert_eq!(
        h.write(
            Method::PATCH,
            &first,
            &bob,
            json!({"version":1,"body":"Stale edit"})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );

    let comparison = h.json("/api/repos/team/private/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true", &bob).await;
    let draft = format!("{PULL}/pending-review");
    let version = Uuid::now_v7();
    h.write(Method::PUT, &draft, &bob, json!({
        "expected":null,"request_id":version,
        "edit":{"action":"save","comment":{
            "request_id":version,"body":"Private pending comment","path_hex":comparison["changes"][0]["path_hex"],
            "base_oid":comparison["base"],"head_oid":head,"side":"new","start_line":2,"end_line":2
        }}
    })).await.error_for_status().unwrap();
    drain(&h).await;
    assert_eq!(
        received.lock().await.len(),
        3,
        "saving a draft sends no event"
    );
    let pending = json!({"request_id":Uuid::now_v7(),"expected":version,"body":"Published pending summary","state":"commented"});
    for request in [pending.clone(), pending] {
        h.write(Method::POST, &format!("{draft}/submit"), &bob, request)
            .await
            .error_for_status()
            .unwrap();
    }
    drain(&h).await;
    let receipts = received.lock().await.clone();
    let expected = [
        ("submitted", 1, 1, "approved"),
        ("edited", 1, 2, "approved"),
        ("dismissed", 1, 3, "approved"),
        ("submitted", 2, 1, "commented"),
    ];
    assert_eq!(receipts.len(), expected.len());
    for (row, (action, review, version, decision)) in receipts.iter().zip(expected) {
        assert_eq!(row.event, "pull_request_review");
        let mut mac =
            Hmac::<Sha256>::new_from_slice(credentials["secret"].as_str().unwrap().as_bytes())
                .unwrap();
        mac.update(&row.body);
        let digest = blake3::Hash::from_bytes(mac.finalize().into_bytes().into());
        assert_eq!(row.signature, format!("sha256={digest}"));
        let payload: Value = serde_json::from_slice(&row.body).unwrap();
        assert_eq!(payload["delivery"], json!(row.id));
        assert_eq!(
            payload["data"],
            json!({
                "event":"pull_request_review","action":action,"pull":1,"review":review,
                "version":version,"commit_oid":head,"decision":decision,
            })
        );
    }
    h.close().await;
    target.abort();
    let _ = target.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the embedded UI with native Cells"]
async fn native_browser_subscribes_to_and_opens_a_delivered_review() {
    let (endpoint, received, target) = receiver(false).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    let browser = Browser::new("review-webhook");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/webhooks", h.origin),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Enable webhook"]).await;
    for name in ["Git pushes", "Pull requests", "Pull request reviews"] {
        browser
            .run(&[
                "find", "role", "checkbox", "click", "--name", name, "--exact",
            ])
            .await;
    }
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
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Hide secret",
            "--exact",
        ])
        .await;
    assert_eq!(
        h.json(PATH, &alice).await["subscription"]["pull_request_review"],
        true
    );
    let head = open_reviewable_pull(&h, &alice).await;
    h.write(Method::POST, &format!("{PULL}/reviews"), &alice, json!({
        "request_id":Uuid::now_v7(),"state":"commented","body":"Browser review delivery","commit_oid":head,
    })).await.error_for_status().unwrap();
    drain(&h).await;
    assert_eq!(received.lock().await.len(), 1);
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Refresh deliveries",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", &format!("code[title='{head}']")])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Pull request #1 · Review #1",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "#pullrequestreview-1"]).await;
    assert!(
        browser
            .run(&["get", "text", "#pullrequestreview-1"])
            .await
            .contains("Browser review delivery")
    );
    drop(browser);
    h.close().await;
    target.abort();
    let _ = target.await;
}
