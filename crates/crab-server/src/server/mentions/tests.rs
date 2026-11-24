use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::http::{Harness, organizations::bootstrap};

const ROOT: &str = "/api/repos/team/private";
const ISSUES: &str = "/api/repos/team/private/issues";
const INBOX: &str = "/api/notifications";
const UNREAD: &str = "/api/notifications/unread";

async fn login(h: &Harness) -> (String, String) {
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    (alice, bob)
}

async fn enroll_bob(h: &Harness, cookie: &str) -> Uuid {
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            cookie,
            json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    serde_json::from_value(profile["id"].clone()).unwrap()
}

async fn issue(h: &Harness, cookie: &str, input: Value) -> Value {
    let response = h.write(Method::POST, ISSUES, cookie, input).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    response.json().await.unwrap()
}

#[tokio::test]
async fn committed_mentions_deliver_once_and_inbox_hides_revoked_repository() {
    let h = Harness::new(false).await;
    let (alice, bob) = login(&h).await;
    let bob_id = enroll_bob(&h, &bob).await;
    let repo = &h.server.repositories.values()[0];

    issue(
        &h,
        &alice,
        json!({
            "request_id":Uuid::now_v7(),
            "title":"Inert terms",
            "body":"`@bob` and \\@bob are examples.\n\n```text\n@bob\n```"
        }),
    )
    .await;
    super::drain_repository(&h.server, repo).await.unwrap();
    assert_eq!(h.json(INBOX, &bob).await["items"], json!([]));

    let input = json!({
        "request_id":Uuid::now_v7(),
        "title":"Review the contribution",
        "body":"Can @bob take a look?"
    });
    let posted = issue(&h, &alice, input.clone()).await;
    let number = posted["number"].as_u64().unwrap();
    assert_eq!(issue(&h, &alice, input.clone()).await["number"], number);
    super::drain_repository(&h.server, repo).await.unwrap();
    let inbox = h.json(INBOX, &bob).await;
    let [notice] = inbox["items"].as_array().unwrap().as_slice() else {
        panic!("the post must produce one mention notification")
    };
    assert_eq!(notice["reason"], "mention");
    assert_eq!(notice["read"], false);
    assert_eq!(notice["title"], "Review the contribution");
    assert_eq!(
        notice["repository"],
        json!({"id":repo.id,"owner":"team","name":"private"})
    );
    assert_eq!(
        notice["href"],
        format!("/team/private/issues/{number}?repository_id={}", repo.id)
    );
    let path = format!("{INBOX}/{}/state", notice["id"].as_str().unwrap());
    h.write(
        Method::PUT,
        &path,
        &bob,
        json!({"repository_id":repo.id,"expected":notice["updated"],"action":{"read":true}}),
    )
    .await
    .error_for_status()
    .unwrap();

    // Another envelope replays the permanent issue creation. Changing prose
    // and adding a picker link update the same conversation with its original reason.
    assert_eq!(issue(&h, &alice, input).await["number"], number);
    h.write(
        Method::PATCH,
        &format!("{ISSUES}/{number}"),
        &alice,
        json!({"version":posted["version"],"body":format!("Added context for @bob and [@bob](/-/users/{bob_id}).")}),
    )
    .await
    .error_for_status()
    .unwrap();
    super::drain_repository(&h.server, repo).await.unwrap();
    let mentions = format!("{INBOX}?reason=mention");
    let inbox = h.json(&mentions, &bob).await;
    let [current] = inbox["items"].as_array().unwrap().as_slice() else {
        panic!("replay and equivalent mention spellings must not add a mention")
    };
    assert_eq!(current["id"], notice["id"]);
    assert_eq!(current["read"], false);
    assert_ne!(current["updated"], notice["updated"]);
    assert_eq!(h.json(INBOX, &alice).await["items"], json!([]));

    let mut members = repo.config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(h.json(INBOX, &bob).await, json!({"items":[],"next":null}));
    h.close().await;
}

#[tokio::test]
async fn bound_team_mention_delivers_and_direct_mention_wins_for_shared_recipient() {
    team_notifications(false).await;
}

#[tokio::test]
async fn group_only_mentions_reach_offline_recipients_with_current_signed_access() {
    team_notifications(true).await;
}

async fn team_notifications(group_only: bool) {
    let h = Harness::new(false).await;
    let (organization, _, bob_id) = bootstrap(&h).await;
    let (alice, bob) = login(&h).await;
    assert_eq!(enroll_bob(&h, &bob).await, bob_id);
    let offer: Value = h
        .write(
            Method::PUT,
            &format!("/api/organizations/{organization}/invitations/{bob_id}"),
            &alice,
            json!({"version":0,"role":"member"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    h.write(
        Method::POST,
        &format!("/api/organizations/{organization}/invitation"),
        &bob,
        json!({"version":offer["version"],"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let team = Uuid::now_v7();
    let teams = format!("/api/organizations/{organization}/teams/commands");
    h.write(
        Method::POST,
        &teams,
        &alice,
        json!({"request_id":team,"command":{"kind":"create","name":"Maintainers","description":"","visibility":"closed"}}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &teams,
        &alice,
        json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":bob_id,"version":0,"role":"member"}}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        &format!("{ROOT}/mention-settings"),
        &alice,
        json!({"organization":organization,"namespace":"crab","teams":[{"id":team,"slug":"maintainers"}],"version":0}),
    )
    .await
    .error_for_status()
    .unwrap();

    if group_only {
        group_grant(&h, &alice, true).await;
    }
    let mut cases = vec![
        ("Team only", "Can @crab/maintainers review?"),
        (
            "Several spellings",
            "Can @crab/maintainers and @bob review?",
        ),
    ];
    let stable_mention = format!("Can [@bob](/-/users/{bob_id}) review?");
    if group_only {
        cases.push(("Stable person mention", &stable_mention));
    }
    for (title, body) in &cases {
        issue(
            &h,
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":body}),
        )
        .await;
    }
    let repo = &h.server.repositories.values()[0];
    super::drain_repository(&h.server, repo).await.unwrap();
    let bob = if group_only {
        // No matching session exists when the worker delivers the mention.
        assert_eq!(h.json(INBOX, &bob).await, json!({"items":[],"next":null}));
        assert_eq!(h.json(UNREAD, &bob).await, json!({"unread":false}));
        *h.provider.mode.lock().await = "member".into();
        h.provider
            .claims
            .lock()
            .await
            .insert("groups".into(), json!(["reviewers"]));
        h.login().await
    } else {
        bob
    };
    let inbox = h.json(INBOX, &bob).await;
    let items = inbox["items"].as_array().unwrap();
    assert_eq!(items.len(), cases.len());
    for &(title, _) in &cases {
        let reason = if title == "Team only" {
            "team_mention"
        } else {
            "mention"
        };
        let item = items.iter().find(|item| item["title"] == title).unwrap();
        assert_eq!(item["reason"], reason);
    }
    let preference = format!("{ISSUES}/1/subscription");
    let expected = if group_only {
        json!({"mode":"inherit","version":0})
    } else {
        json!({"mode":"follow","version":1})
    };
    assert_eq!(h.json(&preference, &bob).await, expected);
    assert_eq!(h.json(INBOX, &alice).await["items"], json!([]));
    if group_only {
        assert_eq!(h.json(UNREAD, &bob).await, json!({"unread":true}));
        let item = &items[0];
        h.write(
            Method::PUT,
            &format!("{INBOX}/{}/state", item["id"].as_str().unwrap()),
            &bob,
            json!({"repository_id":repo.id,"expected":item["updated"],"action":{"read":true}}),
        )
        .await
        .error_for_status()
        .unwrap();
        assert_eq!(h.json(INBOX, &bob).await["items"][0]["read"], true);
        issue(
            &h, &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Signed access","body":"@bob can you confirm?"}),
        ).await;
        super::drain_repository(&h.server, repo).await.unwrap();
        assert_eq!(
            h.json(&format!("{ISSUES}/4/subscription"), &bob).await,
            json!({"mode":"follow","version":1})
        );
        h.provider.claims.lock().await.remove("groups");
        let without_claim = h.login().await;
        assert_eq!(
            h.json(INBOX, &without_claim).await,
            json!({"items":[],"next":null})
        );
        assert_eq!(
            h.json(UNREAD, &without_claim).await,
            json!({"unread":false})
        );
        group_grant(&h, &alice, false).await;
        assert_eq!(h.json(INBOX, &bob).await, json!({"items":[],"next":null}));
        assert_eq!(h.json(UNREAD, &bob).await, json!({"unread":false}));
    } else {
        h.add_repository("library").await;
        let path = format!("{ISSUES}/1");
        let version = h.json(&path, &alice).await["version"].clone();
        let target = h
            .server
            .repositories
            .get(&("team".into(), "library".into()))
            .unwrap();
        h.write(
            Method::POST,
            &format!("{path}/transfer"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"version":version,"destination":target.id}),
        )
        .await
        .error_for_status()
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                if h.json(&path, &alice).await["transfer"]["state"] == "ready" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        super::drain_repository(&h.server, &target).await.unwrap();
        let inbox = h
            .json(&format!("{INBOX}?reason=issue_transferred"), &bob)
            .await;
        let [notice] = inbox["items"].as_array().unwrap().as_slice() else {
            panic!("expected one transfer notice for the source team, got {inbox}");
        };
        assert_eq!(notice["title"], "Team only");
        assert_eq!(notice["repository"]["id"], json!(target.id));
    }
    h.close().await;
}

async fn group_grant(h: &Harness, alice: &str, enabled: bool) {
    let path = format!("{ROOT}/members");
    let current = h.json(&path, alice).await;
    let mut members = current["members"].as_array().unwrap().clone();
    members.retain(|member| member["subject"] != "bob-id");
    let groups = if enabled {
        json!([{"group":"reviewers","access":"read"}])
    } else {
        json!([])
    };
    h.write(
        Method::PUT,
        &path,
        alice,
        json!({"members":members,"groups":groups,"expected_revision":current["revision"]}),
    )
    .await
    .error_for_status()
    .unwrap();
}
