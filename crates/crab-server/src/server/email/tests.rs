use super::*;
use crate::test_support::http::{Harness, browser::Browser};
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::{sync::Mutex, task::JoinHandle};
use uuid::Uuid;

const SETTINGS: &str = "/api/notifications/email";
const REPOSITORY: &str = "/api/repos/team/private";

type Messages = Arc<Mutex<Vec<(String, Value)>>>;

async fn test_provider() -> (Url, Messages, JoinHandle<()>) {
    async fn receive(
        State(messages): State<Messages>,
        headers: HeaderMap,
        Json(data): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let key = headers["idempotency-key"].to_str().unwrap().to_owned();
        assert_eq!(headers[header::AUTHORIZATION], "Bearer fixture-mail-key");
        messages.lock().await.push((key, data));
        (StatusCode::OK, Json(json!({"id": "email-id"})))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        Url::parse(&format!("http://{}/emails", listener.local_addr().unwrap())).unwrap();
    let messages = Messages::default();
    let app = Router::new()
        .route("/emails", post(receive))
        .with_state(messages.clone());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (endpoint, messages, task)
}

async fn member(h: &Harness, address: &str, verified: bool) -> String {
    *h.provider.mode.lock().await = "member".into();
    *h.provider.claims.lock().await = json!({
        "email": address, "email_verified": verified
    })
    .as_object()
    .unwrap()
    .clone();
    h.login().await
}

async fn save(h: &Harness, cookie: &str, enabled: bool, version: &Value) -> Value {
    h.write(
        Method::PUT,
        SETTINGS,
        cookie,
        json!({"enabled": enabled, "version": version}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn handle(h: &Harness, bob: &str) {
    h.write(
        Method::POST,
        "/api/profile",
        bob,
        json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn mention(h: &Harness, alice: &str, title: &str) {
    h.write(
        Method::POST,
        &format!("{REPOSITORY}/issues"),
        alice,
        json!({"request_id":Uuid::now_v7(),"title":title,"body":"Please review, @bob."}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.deliver_notifications("team", "private").await;
}

async fn tick(h: &Harness) -> bool {
    deliver_next(
        &h.server,
        &h.server.cells().unwrap(),
        h.server.email.as_ref().unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn verified_email_requires_opt_in_and_new_addresses_disable_queued_delivery() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;
    let defaults = h.json(SETTINGS, &bob).await;
    assert_eq!(defaults["enabled"], false);
    assert_eq!(defaults["address"], "bob@example.test");
    assert_eq!(h.json(SETTINGS, &alice).await["address"], Value::Null);

    mention(&h, &alice, "Before opt-in").await;
    assert!(!tick(&h).await);
    let enabled = save(&h, &bob, true, &defaults["version"]).await;
    assert_eq!(enabled["enabled"], true);
    let stale = h
        .write(
            Method::PUT,
            SETTINGS,
            &bob,
            json!({
                "enabled": false, "version": defaults["version"]
            }),
        )
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    mention(&h, &alice, "Queued with the first address").await;
    let bob_changed = member(&h, "new@example.test", true).await;
    let changed = h.json(SETTINGS, &bob_changed).await;
    assert_eq!(changed["address"], "new@example.test");
    assert_eq!(changed["enabled"], false);
    assert_ne!(changed["version"], enabled["version"]);
    assert!(tick(&h).await);
    assert!(messages.lock().await.is_empty());

    save(&h, &bob_changed, true, &changed["version"]).await;
    mention(&h, &alice, "Keep this issue private").await;
    assert!(tick(&h).await);
    assert!(!tick(&h).await);
    let sent = messages.lock().await.clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].0.starts_with("crab-notice/"));
    assert_eq!(sent[0].1["to"], json!(["new@example.test"]));
    let text = sent[0].1["text"].as_str().unwrap();
    assert!(text.contains(&format!("{}/-/notifications", h.origin)));
    assert!(!text.contains("Keep this issue private"));
    assert!(!text.contains("team/private"));

    let unverified = member(&h, "new@example.test", false).await;
    let cleared = h.json(SETTINGS, &unverified).await;
    assert_eq!(cleared["address"], Value::Null);
    assert_eq!(cleared["enabled"], false);
    receiver.abort();
    h.close().await;
}

#[tokio::test]
async fn github_sign_in_uses_only_the_verified_primary_email() {
    let (endpoint, _, receiver) = test_provider().await;
    let h = Harness::with_notification_email(true, endpoint).await;
    let session = h.login().await;
    let defaults = h.json(SETTINGS, &session).await;
    assert_eq!(defaults["address"], "primary@example.test");
    save(&h, &session, true, &defaults["version"]).await;
    *h.provider.mode.lock().await = "unverified-email".into();
    let session = h.login().await;
    let settings = h.json(SETTINGS, &session).await;
    assert_eq!(settings["address"], Value::Null);
    assert_eq!(settings["enabled"], false);
    receiver.abort();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated OIDC, native Cells, and a local mail provider"]
async fn native_browser_opts_in_and_delivers_a_real_mention() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;
    let worker = tokio::spawn(run(h.server.clone()));
    let browser = Browser::new("notification-email");
    browser
        .run(&["open", &format!("{}/-/notifications", h.origin)])
        .await;
    let click = async |role: &str, label: &str| {
        browser.run(&["wait", "--text", label]).await;
        browser
            .run(&["find", "role", role, "click", "--name", label, "--exact"])
            .await;
    };
    click("button", "Continue to sign in").await;
    click("button", "Email settings").await;
    browser.run(&["wait", "--text", "bob@example.test"]).await;
    click("checkbox", "Send me notification emails").await;
    click("button", "Save preference").await;
    browser
        .run(&["wait", "--text", "Email notifications on"])
        .await;
    assert_eq!(h.json(SETTINGS, &bob).await["enabled"], true);
    mention(&h, &alice, "Browser email notification").await;
    tokio::time::timeout(Duration::from_secs(20), async {
        while messages.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        messages.lock().await[0].1["to"],
        json!(["bob@example.test"])
    );
    browser.run(&["close"]).await;
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();
    receiver.abort();
    h.close().await;
}

#[tokio::test]
async fn muted_and_revoked_repositories_suppress_already_queued_email() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;
    let defaults = h.json(SETTINGS, &bob).await;
    save(&h, &bob, true, &defaults["version"]).await;

    let preference = format!("{REPOSITORY}/subscription");
    mention(&h, &alice, "Queued before mute").await;
    h.write(
        Method::PUT,
        &preference,
        &bob,
        json!({"mode":"muted","version":0}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(tick(&h).await);
    assert!(messages.lock().await.is_empty());

    h.write(
        Method::PUT,
        &preference,
        &bob,
        json!({"mode":"direct","version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    mention(&h, &alice, "Queued before removing access").await;
    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "private",
            vec![crate::RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: crate::RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    assert!(tick(&h).await);
    assert!(messages.lock().await.is_empty());
    receiver.abort();
    h.close().await;
}

#[tokio::test]
#[ignore = "exercises the real 60-second re-delivery lease after losing the worker acknowledgement"]
async fn mail_recovery_reuses_provider_key_and_payload_after_lost_acknowledgement() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;
    let defaults = h.json(SETTINGS, &bob).await;
    save(&h, &bob, true, &defaults["version"]).await;
    mention(&h, &alice, "Waiting for an acknowledgement").await;

    let cells = h.server.cells().unwrap();
    let job = app::command_output(
        cells
            .command::<ClaimMail>(Address::Directory, app::mutation_identity().unwrap(), ())
            .await,
    )
    .unwrap()
    .unwrap();
    let attempt = MailAttempt {
        id: job.id,
        attempt: job.attempt,
    };
    let to = app::query_output(
        cells
            .query::<MailDestination>(Address::Directory, None, attempt)
            .await,
    )
    .unwrap()
    .unwrap();
    assert!(
        h.server
            .email
            .as_ref()
            .unwrap()
            .send(&job, &to)
            .await
            .is_none()
    );
    // The provider accepted the first request. Simulate a stopped worker by
    // dropping the claim without acknowledging it in the Directory.
    assert!(!tick(&h).await);
    tokio::time::timeout(Duration::from_secs(70), async {
        while !tick(&h).await {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(!tick(&h).await);
    let sent = messages.lock().await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1]);
    receiver.abort();
    h.close().await;
}

async fn ready_transfer_repository(h: &Harness) -> Uuid {
    let id = h.server.repositories.values()[0].id;
    let catalog = h.server.catalog().unwrap();
    catalog.mark_cell_ready(id).await.unwrap();
    catalog
        .set_members(
            "team",
            "private",
            vec![crate::RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: crate::RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    id
}

async fn offer_to_bob(h: &Harness, alice: &str, repository: Uuid) -> Uuid {
    let id = Uuid::now_v7();
    let result: Value = h
        .write(
            Method::POST,
            &format!("/api/repositories/{repository}/transfer"),
            alice,
            json!({
                "request_id":id,
                "recipient":{"kind":"account","handle":"bob"},
                "name":"private-destination-name",
                "version":0,
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["id"], json!(id));
    id
}

async fn cancel_transfer(h: &Harness, alice: &str, offer: Uuid) {
    h.write(
        Method::PUT,
        &format!("/api/repository-transfers/{offer}"),
        alice,
        json!({"decision":"cancel"}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn reconcile_transfers(h: &Harness) {
    // A fresh capture exercises restart and recovery after a lost queue reply.
    transfers::reconcile(&h.server, &h.server.cells().unwrap(), &mut None)
        .await
        .unwrap();
}

#[tokio::test]
async fn transfer_email_replay_preserves_opt_in_cancellation_and_delivery() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    let repository = ready_transfer_repository(&h).await;
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;

    let opted_out = offer_to_bob(&h, &alice, repository).await;
    reconcile_transfers(&h).await;
    assert!(!tick(&h).await);
    let defaults = h.json(SETTINGS, &bob).await;
    save(&h, &bob, true, &defaults["version"]).await;
    reconcile_transfers(&h).await;
    assert!(!tick(&h).await);
    cancel_transfer(&h, &alice, opted_out).await;

    let pending = offer_to_bob(&h, &alice, repository).await;
    reconcile_transfers(&h).await;
    cancel_transfer(&h, &alice, pending).await;
    assert!(tick(&h).await);
    assert!(messages.lock().await.is_empty());
    assert!(!tick(&h).await);

    offer_to_bob(&h, &alice, repository).await;
    reconcile_transfers(&h).await;
    reconcile_transfers(&h).await;
    assert!(tick(&h).await);
    reconcile_transfers(&h).await;
    assert!(!tick(&h).await);
    assert_eq!(messages.lock().await.len(), 1);
    receiver.abort();
    h.close().await;
}

#[tokio::test]
async fn email_worker_delivers_one_private_transfer_alert_to_the_verified_recipient() {
    let (endpoint, messages, receiver) = test_provider().await;
    let h = Harness::with_notification_email(false, endpoint).await;
    let repository = ready_transfer_repository(&h).await;
    let alice = h.login().await;
    let bob = member(&h, "bob@example.test", true).await;
    handle(&h, &bob).await;
    let settings = h.json(SETTINGS, &bob).await;
    save(&h, &bob, true, &settings["version"]).await;
    let offer = offer_to_bob(&h, &alice, repository).await;

    let worker = tokio::spawn(run(h.server.clone()));
    tokio::time::timeout(Duration::from_secs(20), async {
        while messages.lock().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    h.server.cancellation.cancel();
    worker.await.unwrap().unwrap();

    let sent = messages.lock().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["to"], json!(["bob@example.test"]));
    assert_eq!(sent[0].1["subject"], "New repository transfer in Crab");
    let text = sent[0].1["text"].as_str().unwrap();
    assert!(text.contains(&format!("{}/-/notifications?tab=transfers", h.origin)));
    assert!(!text.contains("team/private"));
    assert!(!text.contains("private-destination-name"));
    assert!(!text.contains(&offer.to_string()));
    drop(sent);
    receiver.abort();
    h.close().await;
}
