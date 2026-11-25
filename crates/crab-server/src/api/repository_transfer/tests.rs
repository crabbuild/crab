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

const INCOMING: &str = "/api/repository-transfers/incoming";
const UNREAD: &str = "/api/notifications/unread";

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

async fn propose(
    h: &Harness,
    cookie: &str,
    repo: Uuid,
    recipient: Value,
    name: &str,
) -> (String, Value) {
    let request =
        json!({"request_id":Uuid::now_v7(),"recipient":recipient,"version":0,"name":name});
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

    let (path, request) = propose(
        &h,
        &alice,
        id,
        json!({"kind":"account","handle":"bob"}),
        "moved",
    )
    .await;
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
    let incoming = h.json(INCOMING, &bob).await;
    assert_eq!(incoming["items"].as_array().unwrap().len(), 1);
    assert_eq!(incoming["items"][0]["repository"], json!(id));
    assert_eq!(incoming["items"][0]["can_accept"], true);
    assert_eq!(incoming["next"], Value::Null);
    assert_eq!(h.json(UNREAD, &bob).await["pending_transfers"], true);
    assert_eq!(h.json(INCOMING, &alice).await["items"], json!([]));
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
    let property = Uuid::now_v7().to_string();
    h.write(
        Method::PUT,
        &format!("{root}/repository-properties"),
        &alice,
        json!({"expected_version":0,"definitions":[{
            "id":property,"name":"regulated","description":"","kind":"boolean","options":[],"default":false,
            "required":true,"require_explicit_values":true
        }]}),
    )
    .await
    .error_for_status()
    .unwrap();
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
    let (path, _) = propose(&h, &alice, id, target.clone(), "moved").await;
    assert_eq!(h.json(&path, &alice).await["recipient"], "Crab engineering");
    assert_eq!(h.json(&path, &alice).await["properties"][0]["id"], property);
    assert_eq!(get(&h, &path, &bob).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.json(INCOMING, &bob).await["items"], json!([]));
    assert_eq!(
        h.json(INCOMING, &alice).await["items"][0]["repository"],
        json!(id)
    );
    assert_eq!(h.json(UNREAD, &alice).await["pending_transfers"], true);
    assert_eq!(
        h.write(Method::PUT, &path, &bob, json!({"decision":"accept"}))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let catalog = h.server.catalog().unwrap();
    h.write(
        Method::POST,
        &format!("{root}/repositories/creation"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"name":"moved","description":"",
            "properties":{(&property):false}}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(
            Method::PUT,
            &path,
            &alice,
            json!({"decision":"accept","properties":{(&property):false}})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
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
    assert_eq!(h.json(INCOMING, &alice).await["items"], json!([]));

    let submit = format!("/api/repositories/{id}/transfer");
    let policy = format!("{root}/repository-policy");
    for recipient in [
        target.clone(),
        json!({"kind":"organization","id":Uuid::now_v7()}),
    ] {
        let rejected = h
            .write(
                Method::POST,
                &submit,
                &bob,
                json!({
                    "request_id":Uuid::now_v7(),"recipient":recipient,"version":0,"name":"private"
                }),
            )
            .await;
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            rejected.json::<Value>().await.unwrap()["error"]["code"],
            "transfer_destination_creation_required"
        );
    }

    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({
            "version":0,"member_creation":true,"member_transfer":false,"baseline":null
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let renamed = h
        .write(
            Method::POST,
            &submit,
            &bob,
            json!({
                "request_id":Uuid::now_v7(),"recipient":target,"version":0,"name":"moved"
            }),
        )
        .await;
    assert_eq!(renamed.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        renamed.json::<Value>().await.unwrap()["error"]["code"],
        "transfer_destination_owner_required"
    );

    let (path, _) = propose(&h, &bob, id, target, "private").await;
    assert!(h.json(&path, &bob).await.get("properties").is_none());
    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({
            "version":1,"member_creation":false,"member_transfer":false,"baseline":null
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let blocked = h
        .write(Method::PUT, &path, &alice, json!({"decision":"accept"}))
        .await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        blocked.json::<Value>().await.unwrap()["error"]["code"],
        "transfer_destination_creation_required"
    );
    assert_eq!(h.server.repositories.by_id(id).unwrap().organization, None);
    assert_eq!(h.json(&path, &alice).await["state"], "pending");
    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({
            "version":2,"member_creation":true,"member_transfer":false,"baseline":null
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(Method::PUT, &path, &alice, json!({"decision":"accept"}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.write(
            Method::PUT,
            &path,
            &alice,
            json!({"decision":"accept","properties":{(&property):"true"}})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let choice = json!({"decision":"accept","properties":{(&property):true}});
    let accepted: Value = h
        .write(Method::PUT, &path, &alice, choice.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accepted["state"], "accepted");
    assert_eq!(h.json(INCOMING, &alice).await["items"], json!([]));
    assert_eq!(h.json(UNREAD, &alice).await, json!({"unread":false}));
    let active = h.server.repositories.by_id(id).unwrap();
    assert_eq!(active.organization, Some(organization));
    let lookup = format!("/api/repos?owner={}&name=private", active.config.owner);
    assert_eq!(
        h.json(&lookup, &alice).await["repositories"][0]["can_admin"],
        true
    );
    assert_eq!(
        h.json(&lookup, &bob).await["repositories"][0]["can_admin"],
        true
    );
    let values = format!("/api/repos/{}/private/properties", active.config.owner);
    assert_eq!(
        h.json(&values, &alice).await["values"]["overrides"][&property],
        true
    );
    let filtered = h
        .json(
            &format!("/api/repos?organization={organization}&q=props.regulated%3Atrue"),
            &alice,
        )
        .await;
    assert_eq!(filtered["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(filtered["repositories"][0]["id"], json!(id));
    let ruleset = Uuid::now_v7();
    h.write(Method::PUT, &format!("{root}/rulesets"), &alice, json!({
        "expected_version":0,"rulesets":[{
            "ruleset":{"id":ruleset,"name":"Protect regulated branches","target":"branch",
                "enforcement":"active","include":["~ALL"],"exclude":[],"restrict_creations":true,
                "restrict_deletions":false,"require_pull_request":false,"required_approvals":0,
                "required_code_owner_reviews":false,"required_checks":[],"required_integrations":[]},
            "repositories":{"kind":"properties","conditions":[{"property":property,"value":true}]}
        }]
    })).await.error_for_status().unwrap();
    let rules = format!("/api/repos/{}/private/rulesets", active.config.owner);
    assert_eq!(
        h.json(&rules, &alice).await["inherited"]["rulesets"][0]["id"],
        json!(ruleset)
    );
    assert_eq!(
        h.write(Method::PUT, &path, &alice, choice)
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["state"],
        "accepted"
    );
    assert_eq!(
        h.write(
            Method::PUT,
            &path,
            &alice,
            json!({"decision":"accept","properties":{(&property):false}})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn organization_transfer_policy_fences_team_admin_offers() {
    let (h, _) = fixture().await;
    let (organization, _, bob_id) = organizations::bootstrap(&h).await;
    let alice = h.login().await;
    h.write(
        Method::POST,
        "/api/profile",
        &alice,
        json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let bob = member(&h).await;
    let org = format!("/api/organizations/{organization}");
    h.write(
        Method::PUT,
        &format!("{org}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{org}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let repo: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = Uuid::parse_str(repo["id"].as_str().unwrap()).unwrap();
    let owner = repo["owner"].as_str().unwrap();
    let team = Uuid::now_v7();
    let teams = format!("{org}/teams/commands");
    for (request_id, command) in [
        (
            team,
            json!({"kind":"create","name":"Maintainers","description":"","visibility":"secret"}),
        ),
        (
            Uuid::now_v7(),
            json!({"kind":"member","team":team,"member":bob_id,"version":0,"role":"member"}),
        ),
    ] {
        h.write(
            Method::POST,
            &teams,
            &alice,
            json!({"request_id":request_id,"command":command}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let team_access = format!("/api/repos/{owner}/library/team-access");
    let revision = h.json(&team_access, &alice).await["revision"].clone();
    let session = h.json("/api/session", &alice).await;
    h.http
        .put(format!("{}{team_access}", h.origin))
        .header(header::COOKIE, &alice)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", id.to_string())
        .json(&json!({"expected_revision":revision,"teams":[{"team":team,"access":"admin"}]}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let policy = format!("{org}/repository-policy");
    let transfer = format!("/api/repositories/{id}/transfer");
    let to_alice = json!({"kind":"account","handle":"alice"});
    assert_eq!(h.json(&transfer, &alice).await["can_transfer"], true);
    assert_eq!(h.json(&transfer, &bob).await["can_transfer"], false);
    let rejected = h
        .write(
            Method::POST,
            &transfer,
            &bob,
            json!({"request_id":Uuid::now_v7(),"recipient":to_alice,"version":0,"name":"moved"}),
        )
        .await;
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        rejected.json::<Value>().await.unwrap()["error"]["code"],
        "repository_transfer_restricted"
    );

    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({
            "version":0,"member_creation":false,"member_transfer":true,"baseline":null
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(h.json(&transfer, &bob).await["can_transfer"], true);
    let (offer, _) = propose(&h, &bob, id, to_alice, "moved").await;
    h.write(
        Method::PUT,
        &policy,
        &alice,
        json!({
            "version":1,"member_creation":false,"member_transfer":false,"baseline":null
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let blocked = h
        .write(Method::PUT, &offer, &alice, json!({"decision":"accept"}))
        .await;
    assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        h.server.repositories.by_id(id).unwrap().organization,
        Some(organization)
    );
    assert_eq!(h.json(&offer, &bob).await["can_cancel"], true);
    h.write(Method::PUT, &offer, &bob, json!({"decision":"cancel"}))
        .await
        .error_for_status()
        .unwrap();
    let (offer, _) = propose(
        &h,
        &alice,
        id,
        json!({"kind":"account","handle":"bob"}),
        "moved",
    )
    .await;
    h.write(Method::PUT, &offer, &bob, json!({"decision":"accept"}))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(h.server.repositories.by_id(id).unwrap().organization, None);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI with native catalog and memory object storage"]
async fn native_browser_accepts_a_repository_transfer_from_another_account() {
    let (h, id) = fixture().await;
    let alice = h.login().await;
    let bob = member(&h).await;
    let (path, _) = propose(
        &h,
        &alice,
        id,
        json!({"kind":"account","handle":"bob"}),
        "moved",
    )
    .await;
    let browser = Browser::new("repository-transfer");
    let link = format!("{}/-/notifications", h.origin);
    browser.run(&["open", &link]).await;
    browser.sign_in().await;
    browser
        .run(&[
            "wait",
            r#"[aria-label="Notifications, pending repository transfers"]"#,
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Notifications, pending repository transfers",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Incoming repository transfers"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Review transfer of team/private",
            "--exact",
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
    assert_eq!(h.json(INCOMING, &bob).await["items"], json!([]));
    browser.run(&["close"]).await;
    h.close().await;
}
