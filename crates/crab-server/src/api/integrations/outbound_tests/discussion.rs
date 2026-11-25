use super::*;

const REPO: &str = "/api/repos/team/private";

#[tokio::test]
async fn signed_discussion_events_follow_committed_changes_and_receiver_selection() {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    let (issues_url, issues, issues_receiver) = receiver(false).await;
    let (comments_url, comments, comments_receiver) = receiver(false).await;
    let paths = [&issues_url, &comments_url].map(|url| {
        format!(
            "{PATH}?destination={}",
            crate::server::webhooks::destination(url)
        )
    });
    let h = Harness::with_webhook_receivers([issues_url, comments_url], None).await;
    let alice = h.login().await;
    let write = |method: Method, path: String, body: Value| {
        let (h, alice) = (&h, &alice);
        async move { h.write(method, &path, alice, body).await }
    };
    let mut credentials = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        credentials.push(
            write(
                Method::POST,
                path.clone(),
                json!({
                    "kind":"configure","id":Uuid::now_v7(),"expected":null,"push":false,
                    "pull_request":false,"issues":index == 0,"issue_comment":index == 1,
                }),
            )
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        );
    }
    let issue = json!({"request_id":Uuid::now_v7(),"title":"Mirror this issue","body":"Original description"});
    let close = json!({"request_id":Uuid::now_v7(),"version":3,"state":"closed","reason":"not_planned","body":"Closing comment"});
    let issue_path = format!("{REPO}/issues/1");
    let state_path = format!("{issue_path}/state");
    for (method, path, body) in [
        (Method::POST, format!("{REPO}/issues"), issue.clone()),
        (Method::POST, format!("{REPO}/issues"), issue),
        (
            Method::PATCH,
            issue_path.clone(),
            json!({"version":1,"title":"Updated issue"}),
        ),
        (
            Method::PATCH,
            issue_path.clone(),
            json!({"version":2,"title":"Updated issue"}),
        ),
        (Method::POST, state_path.clone(), close.clone()),
        (Method::POST, state_path.clone(), close),
        (
            Method::PATCH,
            format!("{issue_path}/comments/1"),
            json!({"version":1,"body":"Updated comment"}),
        ),
        (
            Method::PATCH,
            issue_path,
            json!({"version":4,"state":"open"}),
        ),
    ] {
        write(method, path, body).await.error_for_status().unwrap();
    }
    let stale = write(
        Method::POST,
        state_path,
        json!({
            "request_id":Uuid::now_v7(),"version":3,"state":"closed","body":"Rolled back comment"
        }),
    )
    .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);

    stock_push(&h, &alice).await;
    open(&h, &alice, &json!({"request_id":Uuid::now_v7(),"title":"Review discussion","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await;
    let pull_comments = format!("{REPO}/pulls/1/comments");
    let comment = json!({"request_id":Uuid::now_v7(),"body":"Review outside the diff"});
    for replay in [comment.clone(), comment] {
        write(Method::POST, pull_comments.clone(), replay)
            .await
            .error_for_status()
            .unwrap();
    }
    write(
        Method::PATCH,
        format!("{pull_comments}/1"),
        json!({"version":1,"body":"Updated review comment"}),
    )
    .await
    .error_for_status()
    .unwrap();

    drain(&h).await;
    let mut events = Vec::new();
    for (index, rows) in [&issues, &comments].into_iter().enumerate() {
        let mut payloads = Vec::new();
        for row in rows.lock().await.iter() {
            assert_eq!(row.event, ["issues", "issue_comment"][index]);
            let mut mac = Hmac::<Sha256>::new_from_slice(
                credentials[index]["secret"].as_str().unwrap().as_bytes(),
            )
            .unwrap();
            mac.update(&row.body);
            let digest = blake3::Hash::from_bytes(mac.finalize().into_bytes().into());
            assert_eq!(row.signature, format!("sha256={digest}"));
            let payload: Value = serde_json::from_slice(&row.body).unwrap();
            assert_eq!(payload["delivery"], json!(row.id));
            assert!(payload["data"].get("body").is_none());
            payloads.push(payload["data"].clone());
        }
        events.push(payloads);
    }
    assert_eq!(
        events[0]
            .iter()
            .map(|data| (&data["action"], &data["version"]))
            .collect::<Vec<_>>(),
        [
            (&json!("opened"), &json!(1)),
            (&json!("edited"), &json!(2)),
            (&json!("closed"), &json!(4)),
            (&json!("reopened"), &json!(5)),
        ]
    );
    assert_eq!(events[0][2]["state_reason"], "not_planned");
    assert_eq!(
        events[1]
            .iter()
            .map(|data| (&data["subject"]["kind"], &data["action"], &data["version"]))
            .collect::<Vec<_>>(),
        [
            (&json!("issue"), &json!("created"), &json!(1)),
            (&json!("issue"), &json!("edited"), &json!(2)),
            (&json!("pull"), &json!("created"), &json!(1)),
            (&json!("pull"), &json!("edited"), &json!(2)),
        ]
    );
    let view = h.json(&paths[1], &alice).await;
    assert_eq!(view["deliveries"][0]["comment"], 1);
    assert_eq!(view["deliveries"][0]["pull"], 1);
    assert_eq!(view["deliveries"][0]["status"], "delivered");
    h.close().await;
    for receiver in [issues_receiver, comments_receiver] {
        receiver.abort();
        let _ = receiver.await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the embedded UI with native Cells"]
async fn native_browser_subscribes_to_issues_and_opens_a_delivered_comment() {
    let (endpoint, received, target) = receiver(false).await;
    let h = Harness::with_webhook_receivers([endpoint], None).await;
    let alice = h.login().await;
    let browser = Browser::new("discussion-webhook");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/settings/webhooks", h.origin),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Enable webhook"]).await;
    for name in ["Issues", "Issue and pull request comments"] {
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
    h.write(
        Method::POST,
        &format!("{REPO}/issues"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Browser issue hook","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{REPO}/issues/1/comments"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"body":"Browser comment hook"}),
    )
    .await
    .error_for_status()
    .unwrap();
    drain(&h).await;
    assert_eq!(received.lock().await.len(), 2);
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
    browser.run(&["wait", "--text", "Comment Created"]).await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Issue #1 · Comment #1",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "#issuecomment-1"]).await;
    assert!(
        browser
            .run(&["get", "text", "#issuecomment-1"])
            .await
            .contains("Browser comment hook")
    );
    drop(browser);
    h.close().await;
    target.abort();
    let _ = target.await;
}
