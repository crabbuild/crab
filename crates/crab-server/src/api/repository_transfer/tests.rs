use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::{
        git,
        http::{Harness, browser::Browser, organizations},
    },
};

async fn fixture() -> (Harness, Uuid) {
    crate::test_support::server::init_test_tracing();
    let store =
        crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "repository-transfer").await;
    h.initialize_directory().await;
    let id = h.server.repositories.values()[0].id;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(id)
        .await
        .unwrap();
    (h, id)
}

async fn member(h: &Harness) -> String {
    *h.provider.mode.lock().await = "member".into();
    let cookie = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &cookie,
        json!({
            "request_id": Uuid::now_v7(), "handle":"bob", "name":"Bob"
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    cookie
}

async fn propose(h: &Harness, cookie: &str, repo: Uuid, recipient: Value) -> (String, Value) {
    let request =
        json!({"request_id":Uuid::now_v7(),"recipient":recipient,"version":0,"name":"moved"});
    let response: Value = h
        .write(
            Method::POST,
            &format!("/api/repositories/{repo}/transfer"),
            cookie,
            request.clone(),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["state"], "pending");
    assert_eq!(response["repository"], json!(repo));
    (
        format!(
            "/api/repository-transfers/{}",
            response["id"].as_str().unwrap()
        ),
        request,
    )
}

async fn get(h: &Harness, path: &str, cookie: &str) -> reqwest::Response {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
}

async fn git_remote(h: &Harness, cookie: &str, owner: &str, name: &str) -> url::Url {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            cookie,
            json!({"owner":owner,"repository":name,"access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut url = url::Url::parse(&format!("{}/git/{owner}/{name}.git", h.origin)).unwrap();
    url.set_username("crab").unwrap();
    url.set_password(token["token"].as_str()).unwrap();
    url
}

#[tokio::test]
async fn personal_transfer_requires_consent_and_preserves_repository_data() {
    let (h, id) = fixture().await;
    let alice = h.login().await;
    let bob = member(&h).await;
    let old = h.server.repositories.by_id(id).unwrap();
    let original = git_remote(&h, &alice, "team", "private").await;
    let deploy: Value = h
        .write(
            Method::POST,
            "/api/repos/team/private/settings/deploy-tokens",
            &alice,
            json!({"expected_version":0,"edit":{"kind":"create","name":"Deployment","days":7}}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let checkout = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(checkout.path(), original.as_str()).await;
    h.write(
        Method::POST,
        "/api/repos/team/private/issues",
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"title":"Follow the original issue","body":""
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    let (path, request) = propose(&h, &alice, id, json!({"kind":"account","handle":"bob"})).await;
    let replay = h
        .write(
            Method::POST,
            &format!("/api/repositories/{id}/transfer"),
            &alice,
            request,
        )
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(replay.json::<Value>().await.unwrap()["state"], "pending");
    assert_eq!(h.json(&path, &bob).await["can_accept"], true);
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        get(&h, &path, &outsider).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.write(Method::PUT, &path, &alice, json!({"decision":"accept"}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.server.repositories.by_id(id).unwrap().config.owner,
        "team"
    );

    let accepted = || h.write(Method::PUT, &path, &bob, json!({"decision":"accept"}));
    drop(accepted().await.error_for_status().unwrap());
    let result: Value = accepted()
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["state"], "accepted");
    let owner = result["to"]["owner"].as_str().unwrap();
    assert_eq!(
        owner,
        crate::catalog::personal_namespace(&h.provider.issuer, "bob-id")
    );
    let new = h.server.repositories.by_id(id).unwrap();
    assert_eq!(new.config.prefix, old.config.prefix);
    assert_eq!(new.config.name, "moved");
    let issue = h
        .json(&format!("/api/repos/{owner}/moved/issues/1"), &bob)
        .await;
    assert_eq!(issue["title"], "Follow the original issue");
    let redirect = get(&h, "/team/private/issues/1", &alice).await;
    assert_eq!(redirect.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        redirect.headers()[header::LOCATION],
        format!("/{owner}/moved/issues/1")
    );

    let deploy_path = format!("/api/repos/{owner}/moved/settings/deploy-tokens");
    let migrated = h.json(&deploy_path, &bob).await;
    assert_eq!(migrated["tokens"][0]["name"], "Deployment");
    let upload_pack = format!(
        "{}/git/{owner}/moved.git/info/refs?service=git-upload-pack",
        h.origin
    );
    assert_eq!(
        h.http
            .get(upload_pack)
            .header("git-protocol", "version=2")
            .basic_auth("crab", deploy["token"].as_str())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    let token = git_remote(&h, &bob, owner, "moved").await;
    let mut former_address = token.clone();
    former_address.set_path("/git/team/private.git");
    let refs = git::success(
        checkout.path(),
        &["ls-remote", former_address.as_str(), "refs/heads/feature"],
    )
    .await;
    assert_eq!(refs, format!("{head}\trefs/heads/feature"));
    git::success(
        checkout.path(),
        &[
            "push",
            former_address.as_str(),
            "HEAD:refs/heads/after-transfer",
        ],
    )
    .await;
    let denied = h
        .http
        .get(format!(
            "{}/git/team/private.git/info/refs?service=git-upload-pack",
            h.origin
        ))
        .header("git-protocol", "version=2")
        .basic_auth("crab", original.password())
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn organization_transfer_rechecks_source_and_destination_authority() {
    let (h, id) = fixture().await;
    let (organization, _, bob_id) = organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let bob = member(&h).await;
    let root = format!("/api/organizations/{organization}");
    h.write(
        Method::PUT,
        &format!("{root}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{root}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();

    let target = json!({"kind":"organization","id":organization});
    let (path, _) = propose(&h, &alice, id, target.clone()).await;
    assert_eq!(get(&h, &path, &bob).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        h.write(Method::PUT, &path, &bob, json!({"decision":"accept"}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let catalog = h.server.catalog().unwrap();
    catalog
        .set_members(
            "team",
            "private",
            vec![RepositoryMember {
                subject: "bob-id".into(),
                name: "Bob".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        h.write(Method::PUT, &path, &alice, json!({"decision":"accept"}))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let canceled = h
        .write(Method::PUT, &path, &bob, json!({"decision":"cancel"}))
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(canceled["state"], "cancelled");

    let (path, _) = propose(&h, &bob, id, target).await;
    let accepted: Value = h
        .write(Method::PUT, &path, &alice, json!({"decision":"accept"}))
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accepted["state"], "accepted");
    let active = h.server.repositories.by_id(id).unwrap();
    assert_eq!(active.organization, Some(organization));
    let lookup = format!("/api/repos?owner={}&name=moved", active.config.owner);
    assert_eq!(
        h.json(&lookup, &alice).await["repositories"][0]["can_admin"],
        true
    );
    assert_eq!(
        h.json(&lookup, &bob).await["repositories"][0]["can_admin"],
        true
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI with native catalog and memory object storage"]
async fn native_browser_accepts_a_repository_transfer_from_another_account() {
    let (h, id) = fixture().await;
    let alice = h.login().await;
    let bob = member(&h).await;
    let (path, _) = propose(&h, &alice, id, json!({"kind":"account","handle":"bob"})).await;
    let browser = Browser::new("repository-transfer");
    let link = format!("{}{}", h.origin, path.replacen("/api/", "/-/", 1));
    browser.run(&["open", &link]).await;
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
        ])
        .await;
    browser.run(&["wait", "input[name=confirm-transfer]"]).await;
    browser
        .run(&["fill", "input[name=confirm-transfer]", "moved"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Accept transfer",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Transfer accepted"]).await;
    browser
        .run(&["find", "role", "link", "click", "--name", "Open repository"])
        .await;
    browser.run(&["wait", "--text", "moved"]).await;
    assert_eq!(h.json(&path, &bob).await["state"], "accepted");
    browser.run(&["close"]).await;
    h.close().await;
}
