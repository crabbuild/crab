use super::*;
use crate::{
    cells::{
        directory::notifications::{Access, Deliver, Delivery, Notice, Recipient},
        repository::{CreateIssue, CreateIssueInput, CreateIssueOutcome, RepositoryAuthor},
    },
    test_support::http::{Harness, browser::Browser, organizations::bootstrap},
};
use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};

const FILTERS: &str = "/api/notifications/filters";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notification_filters_are_private_versioned_and_bounded() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let id = Uuid::now_v7();
    let criteria = json!({"view":"read","kind":"pull","reason":"review_requested"});
    let changes = json!({"revision":0,"change":{"kind":"save","filter":{"id":id,"name":"PR reviews","criteria":criteria}}});
    assert_eq!(
        h.json(FILTERS, &alice).await,
        json!({"revision":0,"items":[]})
    );
    for _ in 0..2 {
        let saved: Value = h
            .write(Method::PUT, FILTERS, &alice, changes.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(saved["revision"], 1);
        assert_eq!(saved["items"], json!([changes["change"]["filter"]]));
    }
    assert_eq!(
        h.json(FILTERS, &bob).await,
        json!({"revision":0,"items":[]})
    );
    let rename = |revision| json!({"revision":revision,"change":{"kind":"save","filter":{"id":id,"name":"Read reviews","criteria":criteria}}});
    assert_eq!(
        h.write(Method::PUT, FILTERS, &alice, rename(0))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.write(Method::PUT, FILTERS, &alice, rename(1))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json(FILTERS, &alice).await["items"][0]["name"],
        "Read reviews"
    );
    for revision in 2..=15 {
        h.write(
            Method::PUT,
            FILTERS,
            &alice,
            json!({
                "revision":revision,"change":{"kind":"save","filter":{
                    "id":Uuid::now_v7(),"name":format!("Inbox {revision}"),"criteria":{"view":"all"}
                }}
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let too_many = json!({"revision":16,"change":{"kind":"save","filter":{
        "id":Uuid::now_v7(),"name":"One more","criteria":{"view":"all"}
    }}});
    assert_eq!(
        h.write(Method::PUT, FILTERS, &alice, too_many)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let delete = json!({"revision":16,"change":{"kind":"delete","id":id}});
    for _ in 0..2 {
        let removed: Value = h
            .write(Method::PUT, FILTERS, &alice, delete.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(removed["revision"], 17);
        assert_eq!(removed["items"].as_array().unwrap().len(), 14);
    }
    assert_eq!(
        h.write(Method::PUT, FILTERS, &alice, rename(1))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI with native Cells"]
async fn native_browser_saves_and_reopens_an_inbox_filter_across_sessions() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let alice_profile = profile(&h, &alice, "alice", "Alice").await;
    let author = alice_profile["id"].as_str().unwrap();
    let filtered = format!("**/-/notifications?reason=mention&author={author}&preset=*");
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST, "/api/repos/team/private/issues", &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Platform mention","body":"Please review, @bob."}),
    ).await.error_for_status().unwrap();
    h.deliver_notifications("team", "private").await;
    let url = format!("{}/-/notifications", h.origin);
    let browser = Browser::new("filters-first");
    browser.run(&["open", &url]).await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Platform mention"]).await;
    click(&browser, "Manage filters").await;
    click(&browser, "New filter").await;
    browser
        .run(&["fill", "input[name=filter-name]", "Platform triage"])
        .await;
    browser
        .choose_select(
            "[role=dialog] button[role=combobox][id$='-reason']",
            "mention",
        )
        .await;
    browser
        .run(&["fill", "input[name=filter-author]", "ali"])
        .await;
    click(&browser, "Alice (@alice)").await;
    click(&browser, "Save filter").await;
    browser.run(&["wait", "--url", &filtered]).await;
    browser.run(&["wait", "--text", "Platform mention"]).await;
    assert_eq!(
        h.json(FILTERS, &bob).await["items"][0]["name"],
        "Platform triage"
    );
    drop(browser);

    let browser = Browser::new("filters-second");
    browser.run(&["open", &url]).await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Platform mention"]).await;
    click(&browser, "Manage filters").await;
    click(&browser, "Platform triage").await;
    browser.run(&["wait", "--url", &filtered]).await;
    browser.run(&["wait", "--text", "Platform mention"]).await;
    click(&browser, "Manage filters").await;
    click(&browser, "Edit Platform triage").await;
    browser
        .run(&["fill", "input[name=filter-name]", "Platform mentions"])
        .await;
    click(&browser, "Save filter").await;
    click(&browser, "Manage filters").await;
    click(&browser, "Edit Platform mentions").await;
    click(&browser, "Delete filter").await;
    click(&browser, "Confirm deletion").await;
    browser.run(&["wait", "--text", "Filter deleted."]).await;
    assert_eq!(h.json(FILTERS, &bob).await["items"], json!([]));
    drop(browser);
    h.close().await;
}

async fn profile(h: &Harness, cookie: &str, handle: &str, name: &str) -> Value {
    h.write(
        Method::POST,
        "/api/profile",
        cookie,
        json!({"request_id":Uuid::now_v7(),"handle":handle,"name":name}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_author_matches_the_thread_opener_and_survives_handle_changes() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let original = profile(&h, &alice, "alice", "Alice").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let commenter = profile(&h, &bob, "bob", "Bob").await;
    let issues = "/api/repos/team/private/issues";
    for (author, title) in [(&alice, "Opened by Alice"), (&bob, "Opened by Bob")] {
        let issue: Value = h
            .write(
                Method::POST,
                issues,
                author,
                json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        h.write(
            Method::POST,
            &format!("{issues}/{}/comments", issue["number"].as_u64().unwrap()),
            &bob,
            json!({"request_id":Uuid::now_v7(),"body":"One question, @alice."}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.deliver_notifications("team", "private").await;
    let matches = h
        .json("/api/notifications/authors?prefix=%40AL", &alice)
        .await;
    assert_eq!(
        matches["items"],
        json!([{"id":original["id"],"handle":"alice","name":"Alice"}])
    );
    assert_eq!(matches["truncated"], false);
    let original_id = original["id"].as_str().unwrap();
    let commenter_id = commenter["id"].as_str().unwrap();
    let author_query = |author| format!("/api/notifications?author={author}&limit=1");
    for (author, title) in [
        (original_id, "Opened by Alice"),
        ("me", "Opened by Alice"),
        (commenter_id, "Opened by Bob"),
    ] {
        let page = h.json(&author_query(author), &alice).await;
        let [item] = page["items"].as_array().unwrap().as_slice() else {
            panic!("expected one notification for {title}: {page}");
        };
        assert_eq!(item["title"], title);
        assert_eq!(item["sender"], "Bob");
        assert!(page["next"].is_null());
    }
    h.write(
        Method::PUT, FILTERS, &alice,
        json!({"revision":0,"change":{"kind":"save","filter":{
            "id":Uuid::now_v7(),"name":"My own issues","criteria":{"view":"unread","author":original_id}
        }}}),
    ).await.error_for_status().unwrap();
    h.write(
        Method::PATCH,
        "/api/profile",
        &alice,
        json!({"version":original["version"],"handle":"alice-renamed","name":"Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let saved = h.json(FILTERS, &alice).await;
    let author = saved["items"][0]["criteria"]["author"].as_str().unwrap();
    assert_eq!(author, original_id);
    assert_eq!(
        h.json(&author_query(author), &alice).await["items"][0]["title"],
        "Opened by Alice"
    );
    let unknown = Uuid::now_v7().to_string();
    assert_eq!(
        h.json(&author_query(&unknown), &alice).await["items"],
        json!([])
    );
    h.close().await;
}

async fn click(browser: &Browser, name: &str) {
    let label = serde_json::to_string(name).unwrap();
    let ready = format!(
        concat!(
            "Array.from(document.querySelectorAll('button')).some(button => ",
            "!button.disabled && button.getClientRects().length && ",
            "!button.closest('[aria-hidden=true], [inert]') && ",
            "(button.getAttribute('aria-label') || button.textContent.trim()) === {})",
        ),
        label,
    );
    browser.run(&["wait", "--fn", &ready]).await;
    browser
        .run(&["find", "role", "button", "click", "--name", name, "--exact"])
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inbox_combines_organization_type_reason_and_read_before_returning_a_page() {
    let h = Harness::new(false).await;
    let (organization, actor, _) = bootstrap(&h).await;
    let alice = h.login().await;
    let primary = h.server.repositories.values()[0].clone();
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(primary.id)
        .await
        .unwrap();
    let repository: Value = h
        .write(
            Method::POST,
            &format!("/api/organizations/{organization}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"triage","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let scoped: Uuid = serde_json::from_value(repository["id"].clone()).unwrap();
    let client = h.server.cells().unwrap();
    let mut notices = Vec::new();
    for (repository, title, reason) in [
        (scoped, "Organization read", Reason::Mention),
        (scoped, "Organization unread", Reason::Mention),
        (scoped, "Other reason", Reason::Assigned),
        (primary.id, "Personal notification", Reason::Mention),
    ] {
        let output = client
            .command::<CreateIssue>(
                Address::Repository(repository),
                app::mutation_identity().unwrap(),
                CreateIssueInput {
                    metadata: Default::default(),
                    submission_id: *Uuid::now_v7().as_bytes(),
                    author: RepositoryAuthor {
                        issuer: h.provider.issuer.clone(),
                        subject: "alice-id".into(),
                        name: "Alice".into(),
                    },
                    title: title.into(),
                    body: String::new(),
                },
            )
            .await
            .unwrap()
            .output;
        let CreateIssueOutcome::Created(issue) = output else {
            panic!("issue creation failed: {output:?}");
        };
        let notice = Notice {
            id: Uuid::now_v7(),
            repository,
            target: Target::Issue {
                number: issue.number,
            }
            .into(),
            created_at_ms: notices.len() as u64 + 1,
            sender: "Alice".into(),
        };
        client
            .command::<Deliver>(
                Address::Directory,
                app::mutation_identity().unwrap(),
                Delivery {
                    email: false,
                    notice: notice.clone(),
                    recipients: vec![Recipient {
                        actor,
                        reason,
                        access: Access::Direct,
                    }],
                },
            )
            .await
            .unwrap();
        notices.push(notice);
    }
    let first = &notices[0];
    h.write(
        Method::PUT,
        &format!("/api/notifications/{}/state", first.thread_id()),
        &alice,
        json!({"repository_id":scoped,"expected":first.position(),"action":{"read":true}}),
    )
    .await
    .error_for_status()
    .unwrap();
    let url =
        format!("/api/notifications?organization={organization}&reason=mention&kind=issue&limit=1");
    let first_page = h.json(&url, &alice).await;
    assert_eq!(first_page["items"][0]["title"], "Organization unread");
    let second_page = h
        .json(
            &format!("{url}&before={}", first_page["next"].as_str().unwrap()),
            &alice,
        )
        .await;
    assert_eq!(second_page["items"][0]["title"], "Organization read");
    assert!(second_page["next"].is_null());
    let read = h.json(&format!("{url}&read=true"), &alice).await;
    assert_eq!(read["items"][0]["title"], "Organization read");
    assert!(read["next"].is_null());
    let unmatched = h
        .json(
            &format!("/api/notifications?organization={organization}&kind=release"),
            &alice,
        )
        .await;
    assert_eq!(unmatched["items"], json!([]));
    h.close().await;
}
