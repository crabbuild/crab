use crate::cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor};
use crate::test_support::{git, http::Harness};
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};

const ROOT: &str = "/api/repos/team/private/pulls/1/review-requests";
const REVIEWERS: &str = "/api/repos/team/private/pulls/1/reviewers";
async fn seed(h: &Harness) {
    let cell = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    cell.client
        .command::<CreatePull>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                draft: false,
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Review requests".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                base_oid: "a".repeat(40),
                head_ref: "refs/heads/feature".into(),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
}
async fn change(h: &Harness, cookie: &str, version: u64, change: Value) -> reqwest::Response {
    h.write(
        Method::PUT,
        ROOT,
        cookie,
        json!({"version":version,"change":change}),
    )
    .await
}

#[tokio::test]
async fn qualified_members_can_be_requested_cancelled_and_fulfilled_by_http_review() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let bob_id = h.assignee_id(&alice, "Bob").await;
    let token = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let mut url = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), url.as_str()).await;
    h.write(Method::POST,"/api/repos/team/private/pulls",&alice,json!({"request_id":uuid::Uuid::now_v7(),"title":"Please review","body":"Ready for another member","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"})).await.error_for_status().unwrap();
    let page = h.json(REVIEWERS, &alice).await;
    assert_eq!(
        page["candidates"],
        json!([{"id":bob_id,"name":"Bob","profile":null}])
    );
    let requested = change(&h, &alice, 1, json!({"kind":"request","actor_id":bob_id}))
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(requested["state"]["kind"], "requested");
    let id = requested["id"].as_str().unwrap();
    assert!(!requested.to_string().contains("bob-id"));
    assert!(!requested.to_string().contains(&h.provider.issuer));
    change(&h, &alice, 2, json!({"kind":"cancel","id":id}))
        .await
        .error_for_status()
        .unwrap();
    change(&h, &alice, 3, json!({"kind":"request","actor_id":bob_id}))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"cancel","id":id}))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let review = json!({"request_id":uuid::Uuid::now_v7(),"state":"approved","body":"Looks good","commit_oid":head});
    let created = h
        .write(
            Method::POST,
            "/api/repos/team/private/pulls/1/reviews",
            &bob,
            review.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let page = h.json(REVIEWERS, &alice).await;
    assert_eq!(page["version"], 5);
    assert_eq!(
        page["items"][0]["request"]["state"],
        json!({"kind":"fulfilled","review":created["number"]})
    );
    change(&h, &alice, 5, json!({"kind":"request","actor_id":bob_id}))
        .await
        .error_for_status()
        .unwrap();
    h.write(
        Method::POST,
        "/api/repos/team/private/pulls/1/reviews",
        &bob,
        review,
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(REVIEWERS, &alice).await["items"][0]["request"]["state"]["kind"],
        "requested"
    );
    h.close().await;
}

#[tokio::test]
async fn review_request_routes_enforce_membership_csrf_versions_and_archive() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let bob_id = h.assignee_id(&alice, "Bob").await;
    let alice_id = h.assignee_id(&alice, "Alice").await;
    seed(&h).await;
    let body = json!({"version":1,"change":{"kind":"request","actor_id":bob_id}});
    assert_eq!(
        h.http
            .put(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .header(header::ORIGIN, &h.origin)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        change(
            &h,
            &alice,
            1,
            json!({"kind":"request","actor_id":uuid::Uuid::now_v7()})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        change(&h, &alice, 1, json!({"kind":"request","actor_id":alice_id}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    for suffix in ["?limit=0", "?limit=51", "?after=invalid"] {
        assert_eq!(
            h.http
                .get(format!("{}{REVIEWERS}{suffix}", h.origin))
                .header(header::COOKIE, &alice)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(REVIEWERS, &bob).await["can_manage"], false);
    assert_eq!(
        change(&h, &bob, 1, json!({"kind":"request","actor_id":bob_id}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{REVIEWERS}", h.origin))
            .header(header::COOKIE, &outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    change(&h, &alice, 1, json!({"kind":"request","actor_id":bob_id}))
        .await
        .error_for_status()
        .unwrap();
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"request","actor_id":bob_id}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.json(REVIEWERS, &alice).await["items"][0]["request"]["state"]["kind"],
        "requested"
    );
    h.close().await;
}

#[tokio::test]
async fn renaming_or_revoking_a_member_preserves_the_request_identity_and_cancellation() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let bob_id = h.assignee_id(&alice, "Bob").await;
    seed(&h).await;
    let created = change(&h, &alice, 1, json!({"kind":"request","actor_id":bob_id}))
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let members_path = "/api/repos/team/private/members";
    let members = h.json(members_path, &alice).await;
    let renamed=h.write(Method::PUT,members_path,&alice,json!({"expected_revision":members["revision"],"members":[{"subject":"alice-id","name":"Alice","access":"admin"},{"subject":"bob-id","name":"Robert","access":"read"}]})).await.error_for_status().unwrap().json::<Value>().await.unwrap();
    let page = h.json(REVIEWERS, &alice).await;
    assert_eq!(page["items"][0]["reviewer"]["name"], "Bob");
    assert_eq!(page["candidates"][0]["name"], "Robert");
    assert_eq!(page["items"][0]["request"]["id"], created["id"]);
    h.write(Method::PUT,members_path,&alice,json!({"expected_revision":renamed["revision"],"members":[{"subject":"alice-id","name":"Alice","access":"admin"}]})).await.error_for_status().unwrap();
    let page = h.json(REVIEWERS, &alice).await;
    assert_eq!(page["items"][0]["candidate_id"], Value::Null);
    assert_eq!(page["candidates"], json!([]));
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"request","actor_id":bob_id}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    change(&h, &alice, 2, json!({"kind":"cancel","id":created["id"]}))
        .await
        .error_for_status()
        .unwrap();
    assert!(
        h.json(REVIEWERS, &alice).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    h.close().await;
}

#[tokio::test]
async fn reviewer_ids_and_profiles_stay_qualified_and_survive_renames_and_revocation() {
    use crate::cells::{
        Address,
        directory::{IdentityKey, RegisterAccounts},
    };
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h).await;
    let page = h.json(REVIEWERS, &alice).await;
    let bob_id = page["candidates"][0]["id"].clone();
    assert_eq!(page["candidates"][0]["name"], "Bob");
    for secret in ["issuer", "subject", "bob-id", "alice-id"] {
        assert!(!page.to_string().contains(secret));
    }
    let other = h
        .server
        .cells()
        .unwrap()
        .command::<RegisterAccounts>(
            Address::Directory,
            crate::app::mutation_identity().unwrap(),
            vec![IdentityKey {
                issuer: format!("{}/other", h.provider.issuer),
                subject: "bob-id".into(),
            }],
        )
        .await
        .unwrap()
        .output[0];
    assert_ne!(bob_id, json!(other));
    assert_eq!(
        change(&h, &alice, 1, json!({"kind":"request","actor_id":other}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        change(&h, &alice, 1, json!({"kind":"request","subject":"bob-id"}))
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &bob,
            json!({"request_id":uuid::Uuid::now_v7(),"handle":"public-bob","name":"Public Bob"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(profile["id"], bob_id);
    let created: Value = change(&h, &alice, 1, json!({"kind":"request","actor_id":bob_id}))
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["reviewer"]["profile"]["handle"], "public-bob");
    assert_eq!(created["candidate_id"], bob_id);
    assert_eq!(
        created["requested_by"],
        json!({"name":"Alice","profile":null})
    );
    h.write(
        Method::PATCH,
        "/api/profile",
        &bob,
        json!({"version":1,"handle":"renamed-bob","name":"Renamed Bob"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let page = h.json(REVIEWERS, &alice).await;
    assert_eq!(page["items"][0]["request"]["id"], created["id"]);
    assert_eq!(page["items"][0]["candidate_id"], bob_id);
    assert_eq!(
        page["items"][0]["reviewer"]["profile"]["handle"],
        "renamed-bob"
    );
    assert_eq!(page["candidates"][0]["id"], bob_id);
    assert_eq!(page["candidates"][0]["profile"]["handle"], "renamed-bob");
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
    let page = h.json(REVIEWERS, &alice).await;
    assert_eq!(page["items"][0]["candidate_id"], Value::Null);
    assert_eq!(page["items"][0]["reviewer"]["profile"]["id"], bob_id);
    assert_eq!(page["candidates"], json!([]));
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"request","actor_id":bob_id}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    change(&h, &alice, 2, json!({"kind":"cancel","id":created["id"]}))
        .await
        .error_for_status()
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn a_read_only_pull_author_cannot_manage_reviewer_requests_after_downgrade() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h).await;
    let bob_id = h.json(REVIEWERS, &alice).await["candidates"][0]["id"].clone();
    let request: Value = change(&h, &alice, 1, json!({"kind":"request","actor_id":bob_id}))
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut members = h.server.repositories.values()[0].config.members.clone();
    for member in &mut members {
        member.access = if member.subject == "alice-id" {
            crate::RepositoryAccess::Read
        } else {
            crate::RepositoryAccess::Admin
        };
    }
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(h.json(REVIEWERS, &alice).await["can_manage"], false);
    assert_eq!(
        change(&h, &alice, 2, json!({"kind":"cancel","id":request["id"]}))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.json(REVIEWERS, &alice).await["items"][0]["request"]["state"]["kind"],
        "requested"
    );
    h.close().await;
}
