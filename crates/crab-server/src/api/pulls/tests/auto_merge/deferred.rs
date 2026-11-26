use crate::test_support::http::browser::Browser;

use super::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Chromium and the built embedded UI"]
async fn native_browser_merge_when_ready_queues_and_publishes_the_tested_head() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
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
    let checkout = tempfile::tempdir().unwrap();
    let path = checkout.path();
    let first = git::push_review_branches(path, remote.as_str()).await;
    let git = async |args: &[&str]| git::success(path, args).await;
    let base = git(&["rev-parse", "main"]).await;
    h.write(
        Method::PUT,
        &format!("{REPO}/settings/branch-protections"),
        &alice,
        json!({"expected_version":0,"rules":[{"branch":"main",
            "required_approvals":1,"required_checks":["ci"],"require_merge_queue":true}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{REPO}/pulls"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Merge after the head passes","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
    )
    .await
    .error_for_status()
    .unwrap();

    let status = h.json(AUTO, &alice).await;
    assert_eq!(status["can_enable"], false);
    assert_eq!(status["can_wait_for_queue"], true);
    assert_eq!(status["can_enqueue"], false);
    let detail = h.json(PULL, &alice).await;
    let mut input = json!({"request_id":Uuid::now_v7(),"version":detail["version"],
        "base_oid":base,"head_oid":first,"method":"merge_commit","queue":true});
    assert_eq!(
        h.write(Method::PUT, AUTO, &alice, input.clone())
            .await
            .status(),
        StatusCode::CONFLICT,
    );
    input["when_ready"] = json!(true);
    let schedule = async || {
        h.write(Method::PUT, AUTO, &alice, input.clone())
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let waiting = schedule().await;
    assert_eq!(waiting["request"]["waiting_for_queue"], true);
    assert_eq!(schedule().await["request"], waiting["request"]);
    let queue = format!("{REPO}/pulls/queue?branch=refs%2Fheads%2Fmain");
    let queued_search = format!("{REPO}/pulls?q=is%3Aqueued");
    assert_eq!(h.json(&queue, &alice).await["items"], json!([]));
    assert_eq!(h.json(&queued_search, &alice).await["items"], json!([]));

    git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance the pull request head",
    ])
    .await;
    git(&["push", remote.as_str(), "feature"]).await;
    let head = git(&["rev-parse", "HEAD"]).await;
    let check = async |oid: &str| {
        h.write(
            Method::POST,
            &format!("{REPO}/statuses/{oid}"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"context":"ci","state":"success"}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    check(&first).await;
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    tokio::time::timeout(Duration::from_secs(20), async {
        while h.json(AUTO, &alice).await["request"]["stopped"] != "head_changed" {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.json(&queue, &alice).await["items"], json!([]));

    let browser = Browser::new("merge-when-ready");
    browser
        .run(&["open", &format!("{}/team/private/pull/1", h.origin)])
        .await;
    browser.sign_in().await;
    for action in ["Merge when ready", "Confirm merge when ready"] {
        browser.run(&["wait", "--text", action]).await;
        browser
            .run(&[
                "find", "role", "button", "click", "--name", action, "--exact",
            ])
            .await;
    }
    browser
        .run(&["wait", "--text", "Waiting for merge requirements"])
        .await;
    let waiting = h.json(AUTO, &alice).await;
    let request = &waiting["request"]["request_id"];
    assert_eq!(waiting["request"]["head_oid"], head);
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "Waiting for merge requirements"])
        .await;
    assert_eq!(
        &h.json(AUTO, &alice).await["request"]["request_id"],
        request
    );
    assert_eq!(h.json(&queue, &alice).await["items"], json!([]));
    check(&head).await;
    let members = format!("{REPO}/members");
    let revision = h.json(&members, &alice).await["revision"].clone();
    h.write(
        Method::PUT,
        &members,
        &alice,
        json!({
            "expected_revision":revision,"members":[
                {"subject":"alice-id","name":"Alice","access":"admin"},
                {"subject":"bob-id","name":"Bob","access":"write"}],"groups":[]
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(4), async {
            while h.json(AUTO, &alice).await["request"]["waiting_for_queue"] == true {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        })
        .await
        .is_err()
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        &format!("{PULL}/reviews"),
        &bob,
        json!({"request_id":Uuid::now_v7(),"state":"approved","commit_oid":head,"body":"Reviewed"}),
    )
    .await
    .error_for_status()
    .unwrap();

    let candidate_path = format!("{PULL}/merge-candidate");
    let tested = tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            let state = h.json(&candidate_path, &alice).await;
            if state["candidate"]["current"] == true {
                break state["candidate"]["commit"].as_str().unwrap().to_owned();
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    browser
        .run(&["wait", "--text", "Testing merge candidate"])
        .await;
    let queued = h.json(AUTO, &alice).await;
    assert_eq!(&queued["request"]["request_id"], request);
    assert_eq!(queued["request"]["waiting_for_queue"], false);
    assert_eq!(
        h.json(&queued_search, &alice).await["items"][0]["number"],
        1
    );
    assert_eq!(
        h.json(&queue, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        git(&["ls-remote", remote.as_str(), "refs/heads/main"]).await,
        format!("{base}\trefs/heads/main")
    );

    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Candidate checks",
            "--exact",
        ])
        .await;
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "Required checks are waiting"])
        .await;
    browser
        .run(&["wait", &format!(".required-checks code[title='{tested}']")])
        .await;
    check(&tested).await;
    browser
        .run(&["wait", "--text", "Tested merge commit"])
        .await;
    browser
        .run(&[
            "wait",
            &format!("[aria-label='Merge check history'] code[title='{tested}']"),
        ])
        .await;
    git(&["fetch", remote.as_str(), "main"]).await;
    assert_eq!(
        git(&["rev-list", "--parents", "-1", "FETCH_HEAD"]).await,
        format!("{tested} {base} {head}")
    );
    drop(browser);
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    h.close().await;
}
