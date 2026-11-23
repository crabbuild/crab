use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::http::{Harness, browser::Browser};

const REPO: &str = "/api/repos/team/private";
const BLOCKED: &str = "/api/repos/team/private/issues/1/dependencies/blocked_by";
const BLOCKING: &str = "/api/repos/team/private/issues/2/dependencies/blocking";

async fn issues(titles: &[&str]) -> (Harness, String, String) {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    for title in titles {
        h.write(
            Method::POST,
            &format!("{REPO}/issues"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":"Private issue description"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    (h, alice, bob)
}

async fn triage(h: &Harness, alice: &str) {
    let path = format!("{REPO}/members");
    let current = h.json(&path, alice).await;
    h.write(
        Method::PUT,
        &path,
        alice,
        json!({"expected_revision":current["revision"],"groups":[],"members":[
            {"subject":"alice-id","name":"Alice","access":"admin"},
            {"subject":"bob-id","name":"Bob","access":"triage"}
        ]}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn update(
    h: &Harness,
    who: &str,
    path: &str,
    active: bool,
    version: u64,
) -> reqwest::Response {
    h.write(
        Method::PUT,
        path,
        who,
        json!({"active":active,"version":version}),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn dependencies_share_versions_activity_and_live_issue_state() {
    let (h, alice, bob) = issues(&["Ship the release", "Prepare release notes"]).await;
    let forward = format!("{BLOCKED}/2");
    let inverse = format!("{BLOCKING}/1");
    assert_eq!(h.json(BLOCKED, &bob).await["can_manage"], false);
    assert_eq!(
        update(&h, &bob, &forward, true, 0).await.status(),
        StatusCode::FORBIDDEN
    );
    triage(&h, &alice).await;
    assert_eq!(
        h.json(&forward, &bob).await,
        json!({"active":false,"version":0})
    );
    for path in [&forward, &inverse] {
        let result: Value = update(&h, &bob, path, true, 0)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result, json!({"active":true,"version":1}));
    }
    let opposite = h.json(BLOCKING, &bob).await;
    assert_eq!(opposite["items"][0]["work"]["title"], "Ship the release");
    assert_eq!(opposite["items"][0]["version"], 1);
    let mut event = None;
    for number in [1, 2] {
        let page = h
            .json(&format!("{REPO}/issues/{number}/activity"), &bob)
            .await;
        let items: Vec<_> = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["action"]["kind"] == "dependency_changed")
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["actor"]["name"], "Bob");
        assert!(
            event
                .replace(items[0]["number"].clone())
                .is_none_or(|prior| prior == items[0]["number"])
        );
    }
    h.write(
        Method::PATCH,
        &format!("{REPO}/issues/2"),
        &alice,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let forward_page = h.json(BLOCKED, &bob).await;
    assert_eq!(forward_page["items"][0]["work"]["state"], "closed");
    assert!(
        !forward_page
            .to_string()
            .contains("Private issue description")
    );
    update(&h, &bob, &inverse, false, 1)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(h.json(BLOCKED, &bob).await["items"], json!([]));
    assert_eq!(h.json(BLOCKING, &bob).await["items"], json!([]));
    assert_eq!(
        h.json(&forward, &bob).await,
        json!({"active":false,"version":2})
    );
    assert_eq!(
        update(&h, &bob, &forward, true, 0).await.status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dependency_cycle_checks_serialize_competing_edges() {
    let (h, alice, _) = issues(&["First", "Second", "Third", "Fourth", "Fifth"]).await;
    let first = format!("{BLOCKED}/2");
    let second = format!("{REPO}/issues/2/dependencies/blocked_by/3");
    for path in [&first, &second] {
        update(&h, &alice, path, true, 0)
            .await
            .error_for_status()
            .unwrap();
    }
    for path in [
        format!("{REPO}/issues/3/dependencies/blocked_by/1"),
        format!("{BLOCKED}/1"),
    ] {
        assert_eq!(
            update(&h, &alice, &path, true, 0).await.status(),
            StatusCode::BAD_REQUEST
        );
    }
    let four = format!("{REPO}/issues/4/dependencies/blocked_by/5");
    let five = format!("{REPO}/issues/5/dependencies/blocked_by/4");
    let (four, five) = tokio::join!(
        update(&h, &alice, &four, true, 0),
        update(&h, &alice, &five, true, 0)
    );
    let mut statuses = [four.status(), five.status()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::BAD_REQUEST]);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn dependency_pages_keep_exclusive_cursors_at_the_direction_limit() {
    let (h, alice, _) = issues(&["Release task"; 52]).await;
    for number in 2..=51 {
        update(&h, &alice, &format!("{BLOCKED}/{number}"), true, 0)
            .await
            .error_for_status()
            .unwrap();
    }
    let overflow = update(&h, &alice, &format!("{BLOCKED}/52"), true, 0).await;
    assert_eq!(overflow.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        overflow.json::<Value>().await.unwrap()["error"]["message"],
        "An issue can have at most 50 dependencies in either direction"
    );
    let first = h.json(BLOCKED, &alice).await;
    assert_eq!(first["next"], 31);
    let last = h.json(&format!("{BLOCKED}?after=31"), &alice).await;
    assert!(last["next"].is_null());
    let numbers = |page: &Value| {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["work"]["subject"]["number"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(numbers(&first), (2..=31).collect::<Vec<_>>());
    assert_eq!(numbers(&last), (32..=51).collect::<Vec<_>>());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn browser_adds_a_blocker_and_removes_it_from_the_other_issue() {
    let (h, alice, bob) = issues(&["Ship the release", "Prepare release notes"]).await;
    triage(&h, &alice).await;
    let browser = Browser::new("issue-dependencies");
    browser
        .run(&["open", &format!("{}/team/private/issues/1", h.origin)])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Ship the release"]).await;
    browser
        .run(&["click", "details.linked-work > summary"])
        .await;
    let click = async |name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    click("Add blocker").await;
    browser
        .run(&[
            "find",
            "role",
            "textbox",
            "fill",
            "Prepare",
            "--name",
            "Find an issue",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Prepare release notes"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Prepare release notes",
        ])
        .await;
    click("Add dependency").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    assert_eq!(
        h.json(BLOCKED, &bob).await["items"][0]["work"]["title"],
        "Prepare release notes"
    );
    browser
        .run(&["open", &format!("{}/team/private/issues/2", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "Prepare release notes"])
        .await;
    browser
        .run(&["click", "details.linked-work > summary"])
        .await;
    let remove = "[aria-label='Remove dependency #1']";
    browser.run(&["wait", remove]).await;
    browser.run(&["click", remove]).await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.querySelector('section[aria-label=Blocking]')?.innerText.includes('No issues.')",
        ])
        .await;
    assert_eq!(h.json(BLOCKING, &bob).await["items"], json!([]));
    assert_eq!(h.json(BLOCKED, &bob).await["items"], json!([]));
    browser.run(&["close"]).await;
    h.close().await;
}
