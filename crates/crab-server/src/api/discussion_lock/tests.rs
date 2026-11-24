use crate::test_support::{
    git,
    http::{Harness, browser::Browser},
};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use uuid::Uuid;

const ROOT: &str = "/api/repos/team/private";
const ISSUE: &str = "/api/repos/team/private/issues/1";
const PULL: &str = "/api/repos/team/private/pulls/1";

async fn issue() -> (Harness, String, String) {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Moderation test","body":"Description"}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    (h, alice, bob)
}

async fn grant(h: &Harness, owner: &str, access: &str) {
    let path = format!("{ROOT}/members");
    let current = h.json(&path, owner).await;
    h.write(
        Method::PUT,
        &path,
        owner,
        json!({"expected_revision":current["revision"],"members":[
            {"subject":"alice-id","name":"Alice","access":"admin"},
            {"subject":"bob-id","name":"Bob","access":access}
        ],"groups":[]}),
    )
    .await
    .error_for_status()
    .unwrap();
}

fn lock(version: u64) -> Value {
    json!({"version":version,"change":{"kind":"lock","reason":"off_topic"}})
}

async fn assert_locked(response: reqwest::Response) {
    assert_eq!(response.status(), StatusCode::LOCKED);
    assert_eq!(
        response.json::<Value>().await.unwrap()["error"]["code"],
        "conversation_locked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn issue_lock_blocks_new_contributions_and_keeps_comment_receipts() {
    let (h, alice, bob) = issue().await;
    grant(&h, &alice, "triage").await;
    let lock_path = format!("{ISSUE}/lock");
    let comments = format!("{ISSUE}/comments");
    let original = json!({"request_id":Uuid::now_v7(),"body":"Before the lock"});
    assert_eq!(
        h.write(Method::POST, &comments, &bob, original.clone())
            .await
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        h.write(Method::PUT, &lock_path, &bob, lock(0))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        let saved: Value = h
            .write(Method::PUT, &lock_path, &alice, lock(0))
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            saved,
            json!({"version":1,"locked":true,"reason":"off_topic","can_manage":true,"can_post":true})
        );
    }
    let updated = h.json(&lock_path, &bob).await;
    assert_eq!(updated["can_post"], false);
    assert_eq!(updated["can_manage"], false);
    assert_eq!(
        h.write(Method::POST, &comments, &bob, original)
            .await
            .status(),
        StatusCode::CREATED
    );
    let fresh = json!({"request_id":Uuid::now_v7(),"body":"After the lock"});
    assert_locked(h.write(Method::POST, &comments, &bob, fresh.clone()).await).await;
    assert_locked(
        h.write(
            Method::PATCH,
            &format!("{comments}/1"),
            &bob,
            json!({"version":1,"body":"Edited through a stale tab"}),
        )
        .await,
    )
    .await;
    assert_locked(
        h.write(
            Method::PUT,
            &format!("{ROOT}/reactions/issues/1/comments/1"),
            &alice,
            json!({"kind":"eyes","active":true}),
        )
        .await,
    )
    .await;

    let state_path = format!("{ISSUE}/state");
    let rejected = h.write(Method::POST, &state_path, &bob,
        json!({"request_id":Uuid::now_v7(),"version":1,"state":"closed","body":"Rejected atomically"})).await;
    assert_eq!(rejected.headers()["x-crab-write-outcome"], "rejected");
    assert_locked(rejected).await;
    assert_eq!(h.json(ISSUE, &bob).await["state"], "open");
    h.write(
        Method::POST,
        &state_path,
        &bob,
        json!({"request_id":Uuid::now_v7(),"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(ISSUE, &bob).await["state"], "closed");
    let unlock = |version| json!({"version":version,"change":{"kind":"unlock"}});
    assert_eq!(
        h.write(Method::PUT, &lock_path, &alice, unlock(0))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.write(Method::PUT, &lock_path, &alice, unlock(1))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.write(Method::POST, &comments, &bob, fresh).await.status(),
        StatusCode::CREATED
    );
    let activity = h.json(&format!("{ISSUE}/activity"), &bob).await;
    let moderation: Vec<_> = activity["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| {
            let kind = event["action"]["kind"].as_str()?;
            matches!(kind, "locked" | "unlocked").then_some(kind)
        })
        .collect();
    assert_eq!(moderation, ["unlocked", "locked"]);
    let unlocked = format!("{ROOT}/issues?q=is:issue%20is:unlocked");
    let locked = format!("{ROOT}/issues?q=is:issue%20is:locked");
    assert_eq!(h.json(&unlocked, &bob).await["matching"], 1);
    h.write(
        Method::PUT,
        &format!("{ROOT}/settings/archive"),
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&locked, &bob).await["matching"], 1);
    assert_eq!(h.json(&unlocked, &bob).await["matching"], 0);
    let closed_for_archive = h.json(&lock_path, &bob).await;
    assert_eq!(closed_for_archive["locked"], true);
    assert_eq!(closed_for_archive["can_post"], false);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_lock_checks_live_access_and_keeps_a_rejected_review_private() {
    let (h, alice, bob) = issue().await;
    grant(&h, &alice, "moderate").await;
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
    let mut remote = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    git::push_review_branches(source.path(), remote.as_str()).await;
    h.write(
        Method::POST,
        &format!("{ROOT}/pulls"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Lock review test","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let comparison = h
        .json(
            &format!("{ROOT}/changes?base=refs/heads/main&rev=refs/heads/feature&merge_base=true"),
            &bob,
        )
        .await;
    let inline = |request_id| {
        json!({
            "request_id":request_id,"body":"Inline comment","side":"new","start_line":2,"end_line":2,
            "path_hex":comparison["changes"][0]["path_hex"],"base_oid":comparison["base"],"head_oid":comparison["commit"],
        })
    };
    let threads = format!("{PULL}/threads");
    let opened = inline(Uuid::now_v7());
    h.write(Method::POST, &threads, &bob, opened.clone())
        .await
        .error_for_status()
        .unwrap();
    let draft_path = format!("{PULL}/pending-review");
    let draft = Uuid::now_v7();
    let pending = json!({"request_id":draft,"expected":null,"edit":{"action":"save","comment":inline(draft)}});
    h.write(Method::PUT, &draft_path, &bob, pending)
        .await
        .error_for_status()
        .unwrap();
    let lock_path = format!("{PULL}/lock");
    assert_eq!(
        h.write(Method::PUT, &lock_path, &bob, lock(0))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    h.write(Method::PUT, &lock_path, &alice, lock(0))
        .await
        .error_for_status()
        .unwrap();
    let locked = h.json(&format!("{ROOT}/pulls?q=is:locked"), &bob).await;
    assert_eq!(locked["matching"], 1);
    assert_eq!(locked["items"][0]["number"], 1);
    assert_eq!(
        h.json(&format!("{ROOT}/issues?q=is:issue%20is:locked"), &bob)
            .await["matching"],
        0
    );
    let submit = format!("{draft_path}/submit");
    let batch = json!({"request_id":Uuid::now_v7(),"expected":draft,"body":"Review summary","state":"commented"});
    assert_locked(h.write(Method::POST, &submit, &bob, batch.clone()).await).await;
    for (path, input) in [
        (
            format!("{PULL}/comments"),
            json!({"request_id":Uuid::now_v7(),"body":"Plain comment"}),
        ),
        (
            format!("{PULL}/reviews"),
            json!({"request_id":Uuid::now_v7(),"body":"Immediate review","state":"commented","commit_oid":comparison["commit"]}),
        ),
        (threads.clone(), inline(Uuid::now_v7())),
        (
            format!("{threads}/1/replies"),
            json!({"request_id":Uuid::now_v7(),"body":"Inline reply"}),
        ),
    ] {
        assert_locked(h.write(Method::POST, &path, &bob, input).await).await;
    }
    assert_eq!(
        h.json(&format!("{PULL}/reviews"), &alice).await["items"],
        json!([])
    );
    assert_eq!(
        h.json(&draft_path, &bob).await["version"],
        draft.to_string()
    );
    assert_eq!(
        h.write(Method::POST, &threads, &bob, opened).await.status(),
        StatusCode::CREATED
    );
    grant(&h, &alice, "write").await;
    assert_eq!(h.json(&lock_path, &bob).await["can_post"], true);
    let saved: Value = h
        .write(Method::POST, &submit, &bob, batch.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    grant(&h, &alice, "read").await;
    let replay: Value = h
        .write(Method::POST, &submit, &bob, batch)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved, replay);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn moderator_can_lock_and_unlock_from_the_browser() {
    let (h, alice, bob) = issue().await;
    grant(&h, &alice, "moderate").await;
    let browser = Browser::new("conversation-lock");
    browser
        .run(&["open", &format!("{}/team/private/issues/1", h.origin)])
        .await;
    let click = async |name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    browser.sign_in().await;
    click("Lock conversation").await;
    browser
        .run(&[
            "select",
            "[role=dialog] label:nth-of-type(1) select",
            "off_topic",
        ])
        .await;
    browser
        .run(&[
            "select",
            "[role=dialog] label:nth-of-type(2) select",
            "hour",
        ])
        .await;
    click("Lock").await;
    browser
        .run(&["wait", "--text", "Conversation locked · Off topic"])
        .await;
    assert_eq!(
        h.json(&format!("{ISSUE}/lock"), &bob).await["reason"],
        "off_topic"
    );
    let timed = h.json(&format!("{ISSUE}/lock"), &bob).await;
    assert!(timed["expires_at_ms"].as_u64().is_some());
    assert!(
        timed["expires_in_ms"]
            .as_u64()
            .is_some_and(|delay| delay > 3_590_000)
    );
    browser
        .run(&["find", "role", "link", "click", "--name", "Back to issues"])
        .await;
    browser
        .run(&["wait", "button[aria-label='Filter by conversation lock']"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Filter by conversation lock",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "menuitemradio",
            "click",
            "--name",
            "Locked",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--fn",
            "new URLSearchParams(location.search).get('q') === 'is:issue is:open is:locked'",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Moderation test",
            "--exact",
        ])
        .await;
    click("Unlock conversation").await;
    browser.run(&["wait", "--text", "Lock conversation"]).await;
    assert_eq!(
        h.json(&format!("{ISSUE}/lock"), &bob).await["locked"],
        false
    );
    browser.run(&["close"]).await;
    h.close().await;
}
