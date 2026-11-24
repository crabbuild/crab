use super::*;
use crate::{RepositoryAccess, RepositoryMember};

async fn repository_post(h: &Harness, cookie: &str, path: &str, body: Value) -> Value {
    let mut segments = path.strip_prefix("/api/repos/").unwrap().split('/');
    let owner = segments.next().unwrap();
    let name = segments.next().unwrap();
    let repository = h
        .server
        .repositories
        .get(&(owner.into(), name.into()))
        .unwrap();
    let session = h.json("/api/session", cookie).await;
    let response = h
        .http
        .post(format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repository.id.to_string())
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let payload = response.json::<Value>().await.unwrap();
    assert!(status.is_success(), "{path} returned {status}: {payload}");
    payload
}

async fn move_issue(h: &Harness, alice: &str, source: &str, destination: Uuid) -> Value {
    let issue = h.json(source, alice).await;
    let input = json!({
        "request_id": Uuid::now_v7(),
        "version": issue["version"],
        "destination": destination,
    });
    repository_post(h, alice, &format!("{source}/transfer"), input.clone()).await;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if h.json(source, alice).await["transfer"]["state"] == "ready" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    input
}

async fn create_issue(h: &Harness, alice: &str, repository: &str) -> String {
    let created = repository_post(
        h,
        alice,
        &format!("{repository}/issues"),
        json!({
            "request_id":Uuid::now_v7(),
            "title":"Confidential release issue",
            "body":"Internal release note for @bob."
        }),
    )
    .await;
    format!(
        "{repository}/issues/{}",
        created["number"].as_u64().unwrap()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn issue_move_mail_follows_opt_in_both_grants_and_the_published_issue() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    h.add_repository("library").await;
    h.add_repository("archive").await;
    let library = h
        .server
        .repositories
        .get(&("team".into(), "library".into()))
        .unwrap();
    let archive = h
        .server
        .repositories
        .get(&("team".into(), "archive".into()))
        .unwrap();
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;

    let path = create_issue(&h, &alice, REPOSITORY).await;
    move_issue(&h, &alice, &path, library.id).await;
    h.deliver_notifications("team", "library").await;
    assert!(!tick(&h).await);
    let settings = h.json(SETTINGS, &bob).await;
    save(&h, &bob, true, &settings["version"]).await;

    let path = create_issue(&h, &alice, REPOSITORY).await;
    let input = move_issue(&h, &alice, &path, library.id).await;
    h.deliver_notifications("team", "library").await;
    assert!(tick(&h).await);
    h.write(Method::POST, &format!("{path}/transfer"), &alice, input)
        .await
        .error_for_status()
        .unwrap();
    h.deliver_notifications("team", "library").await;
    assert!(!tick(&h).await);
    {
        let sent = messages.lock().await;
        let [(_, email)] = sent.as_slice() else {
            panic!("expected one email for an eligible issue transfer");
        };
        assert_eq!(email["subject"], "Issue transferred in Crab");
        assert_eq!(email["to"], json!(["bob@example.test"]));
        assert_eq!(
            email["text"],
            format!(
                "An issue that mentions you moved to another repository.\n\nOpen your notifications: {}/-/notifications?reason=issue_transferred\n",
                h.origin,
            )
        );
    }

    let admin = RepositoryMember {
        subject: "alice-id".into(),
        name: "Alice".into(),
        access: RepositoryAccess::Admin,
    };
    let catalog = h.server.catalog().unwrap();
    for repository in ["private", "library"] {
        let path = create_issue(&h, &alice, REPOSITORY).await;
        move_issue(&h, &alice, &path, library.id).await;
        h.deliver_notifications("team", "library").await;
        let previous = catalog.membership("team", repository).await.unwrap();
        catalog
            .set_members("team", repository, vec![admin.clone()], true)
            .await
            .unwrap();
        assert!(tick(&h).await);
        assert_eq!(messages.lock().await.len(), 1);
        catalog
            .set_members("team", repository, previous.members, true)
            .await
            .unwrap();
    }

    let path = create_issue(&h, &alice, REPOSITORY).await;
    move_issue(&h, &alice, &path, library.id).await;
    h.deliver_notifications("team", "library").await;
    let old_target = h.json(&path, &alice).await["transfer"]["number"]
        .as_u64()
        .unwrap();
    move_issue(
        &h,
        &alice,
        &format!("/api/repos/team/library/issues/{old_target}"),
        archive.id,
    )
    .await;
    assert!(tick(&h).await);
    assert_eq!(messages.lock().await.len(), 1);
    h.deliver_notifications("team", "archive").await;
    let settings = h.json(SETTINGS, &bob).await;
    save(&h, &bob, false, &settings["version"]).await;
    assert!(tick(&h).await);
    assert!(!tick(&h).await);
    assert_eq!(messages.lock().await.len(), 1);
    receiver.abort();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn organization_baseline_members_receive_issue_move_notifications() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    let (organization, _, bob_actor) =
        crate::test_support::http::organizations::bootstrap(&h).await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;
    let org_path = format!("/api/organizations/{organization}");
    for (method, path, cookie, body) in [
        (
            Method::PUT,
            format!("{org_path}/invitations/{bob_actor}"),
            &alice,
            json!({"version":0,"role":"member"}),
        ),
        (
            Method::POST,
            format!("{org_path}/invitation"),
            &bob,
            json!({"version":1,"accept":true}),
        ),
        (
            Method::PUT,
            format!("{org_path}/repository-policy"),
            &alice,
            json!({"version":0,"member_creation":false,"baseline":"read"}),
        ),
    ] {
        h.write(method, &path, cookie, body)
            .await
            .error_for_status()
            .unwrap();
    }
    let creation = format!("{org_path}/repositories/creation");
    let mut repositories = Vec::<Value>::new();
    for name in ["origin", "destination"] {
        repositories.push(
            h.write(
                Method::POST,
                &creation,
                &alice,
                json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap(),
        );
    }
    let owner = repositories[0]["owner"].as_str().unwrap();
    let destination: Uuid = serde_json::from_value(repositories[1]["id"].clone()).unwrap();
    let settings = h.json(SETTINGS, &bob).await;
    save(&h, &bob, true, &settings["version"]).await;
    let path = create_issue(&h, &alice, &format!("/api/repos/{owner}/origin")).await;
    move_issue(&h, &alice, &path, destination).await;
    h.deliver_notifications(owner, "destination").await;
    assert!(tick(&h).await);
    let inbox = h
        .json("/api/notifications?reason=issue_transferred", &bob)
        .await;
    assert_eq!(inbox["items"][0]["repository"]["id"], json!(destination));
    assert_eq!(
        messages.lock().await[0].1["to"],
        json!(["bob@example.test"])
    );

    let path = create_issue(&h, &alice, &format!("/api/repos/{owner}/origin")).await;
    move_issue(&h, &alice, &path, destination).await;
    h.deliver_notifications(owner, "destination").await;
    h.write(
        Method::PUT,
        &format!("{org_path}/repository-policy"),
        &alice,
        json!({"version":1,"member_creation":false,"baseline":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(tick(&h).await);
    assert!(!tick(&h).await);
    assert_eq!(messages.lock().await.len(), 1);
    assert_eq!(
        h.json("/api/notifications?reason=issue_transferred", &bob)
            .await["items"],
        json!([])
    );
    receiver.abort();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI with native Cells"]
async fn native_browser_follows_issue_move_email_to_the_destination() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    h.add_repository("library").await;
    let library = h
        .server
        .repositories
        .get(&("team".into(), "library".into()))
        .unwrap();
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;
    let settings = h.json(SETTINGS, &bob).await;
    save(&h, &bob, true, &settings["version"]).await;
    let path = create_issue(&h, &alice, REPOSITORY).await;
    move_issue(&h, &alice, &path, library.id).await;
    h.deliver_notifications("team", "library").await;
    assert!(tick(&h).await);
    let address = messages.lock().await[0].1["text"]
        .as_str()
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Open your notifications: "))
        .unwrap()
        .to_owned();
    let browser = Browser::new("issue-move-email");
    browser.run(&["open", &address]).await;
    browser.sign_in().await;
    browser
        .run(&[
            "wait",
            "--url",
            "**/-/notifications?reason=issue_transferred",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Confidential release issue"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Confidential release issue",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/team/library/issues/1*"])
        .await;
    browser
        .run(&["wait", "--text", "Internal release note for"])
        .await;
    drop(browser);
    receiver.abort();
    h.close().await;
}
