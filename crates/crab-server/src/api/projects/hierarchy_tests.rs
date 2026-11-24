use super::*;
use crate::{RepositoryAccess, test_support::http::Harness};
use reqwest::{Method, StatusCode};

const ROOT: &str = "/api/repos/team/private";
const COMMANDS: &str = "/api/repos/team/private/projects/commands";
const TREE: &str = "/api/repos/team/private/issues/1/hierarchy";

fn submission(command: Value) -> Value {
    json!({"request_id":Uuid::now_v7(),"command":command})
}

async fn command(h: &Harness, account: &str, body: Value) -> Value {
    h.write(Method::POST, COMMANDS, account, body)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn seed(h: &Harness, account: &str) {
    for title in ["Release", "Deploy", "Validate"] {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            account,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
}

fn set_parent(child: u64, parent: Option<u64>, version: u64) -> Value {
    submission(json!({"kind":"parent","child":child,"parent":parent,"version":version}))
}

fn numbers(tree: &Value) -> Vec<u64> {
    tree["hierarchy"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["subject"]["number"].as_u64().unwrap())
        .collect()
}

#[tokio::test]
async fn sub_issue_order_checks_current_siblings_and_replays_the_saved_move() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h, &alice).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let link = set_parent(2, Some(1), 0);
    assert_eq!(h.json(TREE, &bob).await["can_manage"], false);
    assert_eq!(
        h.write(Method::POST, COMMANDS, &bob, link.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    let repo = h.server.repositories.values()[0].clone();
    let mut members = repo.config.members.clone();
    members
        .iter_mut()
        .find(|member| member.subject == "bob-id")
        .unwrap()
        .access = RepositoryAccess::Triage;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(h.json(TREE, &bob).await["can_manage"], true);
    assert_eq!(command(&h, &bob, link).await["outcome"], "applied");
    assert_eq!(
        command(&h, &bob, set_parent(3, Some(1), 0)).await["outcome"],
        "applied"
    );
    assert_eq!(
        h.write(
            Method::POST,
            COMMANDS,
            &bob,
            submission(json!({
                "kind":"create","id":Uuid::now_v7(),"name":"Writer only","template":"kanban"
            }))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );

    let first_page = format!("{TREE}?limit=1");
    let before = h.json(&first_page, &bob).await;
    assert_eq!(numbers(&before), [2]);
    assert_eq!(before["hierarchy"]["next"], 2);
    let request = submission(json!({
        "kind":"reorder_sub_issue","parent":1,"child":3,"sibling":2,"position":"before",
        "order_token":before["hierarchy"]["order_token"]
    }));
    let saved = command(&h, &bob, request.clone()).await;
    assert_eq!(saved["outcome"], "applied");
    let after = h.json(&first_page, &bob).await;
    assert_eq!(numbers(&after), [3]);
    assert_eq!(after["hierarchy"]["next"], 3);
    assert_eq!(
        numbers(&h.json(&format!("{first_page}&after=3"), &bob).await),
        [2]
    );
    assert_eq!(numbers(&h.json(TREE, &bob).await), [3, 2]);

    assert_eq!(
        command(&h, &bob, set_parent(3, None, 1)).await["outcome"],
        "applied"
    );
    let mut stale = request.clone();
    stale["request_id"] = json!(Uuid::now_v7());
    assert_eq!(command(&h, &bob, stale).await["outcome"], "rejected");
    assert_eq!(
        command(&h, &bob, set_parent(3, Some(1), 2)).await["outcome"],
        "applied"
    );
    assert_eq!(command(&h, &bob, request).await, saved);
    assert_eq!(numbers(&h.json(TREE, &bob).await), [2, 3]);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and built embedded assets"]
async fn browser_prioritizes_a_sub_issue_and_reloads_the_saved_order() {
    use crate::test_support::http::browser::Browser;

    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h, &alice).await;
    for child in [2, 3] {
        assert_eq!(
            command(&h, &alice, set_parent(child, Some(1), 0)).await["outcome"],
            "applied"
        );
    }
    let browser = Browser::new("sub-issue-order");
    let issue = format!("{}/team/private/issues/1", h.origin);
    browser.run(&["open", &issue]).await;
    browser.sign_in().await;
    let move_up = r#"button[aria-label="Move #3 up"]"#;
    browser.run(&["wait", move_up]).await;
    browser.run(&["focus", move_up]).await;
    browser.run(&["press", "Enter"]).await;
    let first = r#"ul[aria-label="Sub-issues"] > li[data-sub-issue="3"]:first-child"#;
    browser.run(&["wait", first]).await;
    assert_eq!(numbers(&h.json(TREE, &alice).await), [3, 2]);
    browser.run(&["open", &issue]).await;
    browser.run(&["wait", first]).await;
    h.close().await;
}
