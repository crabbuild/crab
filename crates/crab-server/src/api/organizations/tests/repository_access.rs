use super::{repositories::repository_write, *};
use crate::test_support::http::{browser::Browser, organizations::bootstrap};

pub(super) struct AccessFixture {
    pub(super) h: Harness,
    pub(super) organization: Uuid,
    pub(super) owner_id: Uuid,
    member: Uuid,
    team: Uuid,
    pub(super) alice: String,
    pub(super) bob: String,
    pub(super) org: String,
    repository: String,
    repository_id: Uuid,
}

impl AccessFixture {
    pub(super) async fn new() -> Self {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let h =
            Harness::new_with_store_and_prefix(false, false, store, "organization-access").await;
        h.initialize_directory().await;
        let (organization, owner_id, member) = bootstrap(&h).await;
        let alice = h.login().await;
        *h.provider.mode.lock().await = "member".into();
        let bob = h.login().await;
        let org = format!("/api/organizations/{organization}");
        h.write(
            Method::PUT,
            &format!("{org}/invitations/{member}"),
            &alice,
            json!({"role":"member","version":0}),
        )
        .await
        .error_for_status()
        .unwrap();
        h.write(
            Method::POST,
            &format!("{org}/invitation"),
            &bob,
            json!({"accept":true,"version":1}),
        )
        .await
        .error_for_status()
        .unwrap();
        let team = Uuid::now_v7();
        h.write(Method::POST, &format!("{org}/teams/commands"), &alice, json!({
            "request_id":team,
            "command":{"kind":"create","name":"Release engineering","description":"","visibility":"secret"},
        })).await.error_for_status().unwrap();
        h.write(
            Method::POST,
            &format!("{org}/teams/commands"),
            &alice,
            json!({
                "request_id":Uuid::now_v7(), "command":{
                    "kind":"member","team":team,"member":member,"version":0,"role":"member",
                },
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        let path = format!("{org}/repositories/creation");
        let created: Value = h
            .write(
                Method::POST,
                &path,
                &alice,
                json!({
                    "request_id":Uuid::now_v7(),"name":"library","description":"",
                }),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        h.write(
            Method::POST,
            &path,
            &alice,
            json!({
                "request_id":Uuid::now_v7(),"name":"private-tool","description":"",
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        let repository = format!("/api/repos/{}/library", created["owner"].as_str().unwrap());
        let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
        let direct = format!("{repository}/members");
        let page = h.json(&direct, &alice).await;
        repository_write(
            &h,
            Method::PUT,
            &direct,
            &alice,
            id,
            json!({
                "expected_revision":page["revision"],
                "members":[{"subject":"bob-id","name":"Bob","access":"read"}],
                "groups":[{"group":"release-admins","access":"admin"}],
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        let team_access = format!("{repository}/team-access");
        let page = h.json(&team_access, &alice).await;
        repository_write(
            &h,
            Method::PUT,
            &team_access,
            &alice,
            id,
            json!({
                "expected_revision":page["revision"],"teams":[{"team":team,"access":"write"}],
            }),
        )
        .await
        .error_for_status()
        .unwrap();
        Self {
            h,
            organization,
            owner_id,
            member,
            team,
            alice,
            bob,
            org,
            repository,
            repository_id: id,
        }
    }

    fn path(&self, actor: Uuid) -> String {
        format!("{}/members/{actor}/repository-access", self.org)
    }

    async fn remove_team_member(&self) {
        self.h.write(Method::POST, &format!("{}/teams/commands",self.org), &self.alice, json!({
            "request_id":Uuid::now_v7(), "command":{
                "kind":"member","team":self.team,"member":self.member,"version":1,"role":null,
            },
        })).await.error_for_status().unwrap();
    }

    pub(super) async fn get(&self, path: &str, cookie: &str) -> reqwest::Response {
        self.h
            .http
            .get(format!("{}{path}", self.h.origin))
            .header(header::COOKIE, cookie)
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_inspects_configured_roles_and_a_team_revocation_invalidates_the_page() {
    let f = AccessFixture::new().await;
    let path = f.path(f.member);
    assert_eq!(f.get(&path, &f.bob).await.status(), StatusCode::NOT_FOUND);
    let first = f.h.json(&format!("{path}?limit=1"), &f.alice).await;
    let next = first["next"].as_str().unwrap();
    let row = &first["items"][0];
    assert_eq!(row["name"], "library");
    assert_eq!(row["access"], "write");
    assert_eq!(row["direct"], "read");
    assert_eq!(
        row["teams"],
        json!([{
            "id":f.team,"name":"Release engineering","access":"write",
        }])
    );
    assert_eq!(row["sso_group_ceiling"], "admin");
    assert!(!first.to_string().contains("bob-id"));
    assert!(!first.to_string().contains("release-admins"));
    let following =
        f.h.json(&format!("{path}?limit=1&after={next}"), &f.alice)
            .await;
    assert_eq!(following["items"][0]["name"], "private-tool");
    assert_eq!(following["items"][0]["access"], Value::Null);
    assert_eq!(following["next"], Value::Null);
    assert_eq!(
        f.get(&format!("{}?after={next}", f.path(f.owner_id)), &f.alice)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let owner = f.h.json(&f.path(f.owner_id), &f.alice).await;
    assert_eq!(owner["items"][0]["access"], "admin");
    assert_eq!(owner["items"][0]["organization_owner"], true);

    let repo = f.repository.strip_prefix("/api/repos/").unwrap();
    let (owner, name) = repo.split_once('/').unwrap();
    let request_token = || {
        f.h.write(
            Method::POST,
            "/api/git-token",
            &f.bob,
            json!({"owner":owner,"repository":name,"access":"write"}),
        )
    };
    assert!(request_token().await.status().is_success());
    f.remove_team_member().await;
    assert_eq!(
        f.get(&format!("{path}?limit=1&after={next}"), &f.alice)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let current = f.h.json(&path, &f.alice).await;
    assert_eq!(current["items"][0]["access"], "read");
    assert_eq!(current["items"][0]["teams"], json!([]));
    assert_eq!(request_token().await.status(), StatusCode::FORBIDDEN);

    f.h.server
        .auth
        .as_ref()
        .unwrap()
        .set_account_suspended("bob-id", 0, true)
        .await
        .unwrap();
    let blocked = f.h.json(&path, &f.alice).await;
    assert_eq!(blocked["account"], "suspended");
    assert_eq!(blocked["items"][0]["access"], "read");
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_collaborators_follow_current_membership_and_repository_grants() {
    let f = AccessFixture::new().await;
    let path = format!("{}/outside-collaborators", f.org);
    assert_eq!(f.get(&path, &f.bob).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(f.h.json(&path, &f.alice).await["items"], json!([]));

    f.h.write(
        Method::DELETE,
        &format!("{}/members/{}", f.org, f.member),
        &f.alice,
        json!({"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    let direct = format!("{}/members", f.repository);
    let page = f.h.json(&direct, &f.alice).await;
    let repository = f.repository_id;
    let update = |revision: Value, members: Value| json!({"expected_revision":revision,"members":members,"groups":page["groups"]});
    let existing_bob = page["members"][0].clone();
    let outside =
        json!({"name":"Visiting engineer","subject":"unlisted-subject","access":"triage"});
    repository_write(
        &f.h,
        Method::PUT,
        &direct,
        &f.alice,
        repository,
        update(page["revision"].clone(), json!([existing_bob, outside])),
    )
    .await
    .error_for_status()
    .unwrap();

    let listing =
        f.h.json(&format!("{path}?limit=1&q=library"), &f.alice)
            .await;
    let cursor = listing["next"].as_str().unwrap();
    let following =
        f.h.json(
            &format!("{path}?limit=1&q=library&after={cursor}"),
            &f.alice,
        )
        .await;
    let names = [&listing["items"][0], &following["items"][0]];
    assert!(
        names
            .iter()
            .any(|row| row["name"] == "Bob" && row["access"] == "read")
    );
    assert!(
        names
            .iter()
            .any(|row| row["name"] == "Visiting engineer" && row["access"] == "triage")
    );
    assert!(
        names
            .iter()
            .all(|row| row["repository_id"] == json!(repository))
    );
    assert_eq!(following["next"], Value::Null);
    for page in [listing.clone(), following] {
        let wire = page.to_string();
        assert!(!wire.contains("bob-id"));
        assert!(!wire.contains("unlisted-subject"));
        assert!(!wire.contains("release-admins"));
    }
    assert_eq!(
        f.get(&format!("{path}?after={cursor}&q=visiting"), &f.alice)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(f.get(&path, &f.bob).await.status(), StatusCode::NOT_FOUND);
    let repo = f.repository.strip_prefix("/api/repos/").unwrap();
    let (owner, name) = repo.split_once('/').unwrap();
    assert!(
        f.h.write(
            Method::POST,
            "/api/git-token",
            &f.bob,
            json!({"owner":owner,"repository":name,"access":"read"})
        )
        .await
        .status()
        .is_success()
    );

    let current = f.h.json(&direct, &f.alice).await;
    repository_write(
        &f.h,
        Method::PUT,
        &direct,
        &f.alice,
        repository,
        update(current["revision"].clone(), json!([outside])),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        f.get(&format!("{path}?after={cursor}&q=library"), &f.alice)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.h.json(&path, &f.alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses an isolated OIDC/native Cell fixture"]
async fn native_browser_removes_a_repository_grant_from_the_outside_collaborator_view() {
    let f = AccessFixture::new().await;
    f.h.write(
        Method::DELETE,
        &format!("{}/members/{}", f.org, f.member),
        &f.alice,
        json!({"version":1}),
    )
    .await
    .error_for_status()
    .unwrap();
    *f.h.provider.mode.lock().await = "owner".into();
    let browser = Browser::new("outside-collaborators");
    let root = format!("{}/-/organizations/{}", f.h.origin, f.organization);
    browser.run(&["open", &root]).await;
    browser.sign_in().await;
    for (ready, role, label) in [
        (
            "nav[aria-label='People'] a[href*='people=outside']",
            "link",
            "Outside collaborators",
        ),
        (
            "a[aria-label='Manage library access for Bob']",
            "link",
            "Manage library access for Bob",
        ),
        ("button[aria-label='Remove Bob']", "button", "Remove Bob"),
        (
            "[role=region][aria-label='Remove member']",
            "button",
            "Remove member",
        ),
        (".member-save-bar button", "button", "Save access"),
    ] {
        browser.run(&["wait", ready]).await;
        browser
            .run(&["find", "role", role, "click", "--name", label, "--exact"])
            .await;
    }
    browser.run(&["wait", "--text", "Access updated."]).await;
    browser
        .run(&["open", &format!("{root}?tab=people&people=outside")])
        .await;
    browser
        .run(&["wait", "--text", "No matching outside collaborator grants."])
        .await;
    assert_eq!(
        f.h.json(&format!("{}/members", f.repository), &f.alice)
            .await["members"],
        json!([])
    );
    drop(browser);
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses an isolated OIDC/native Cell fixture"]
async fn native_browser_inspects_the_member_and_refreshes_after_a_team_revocation() {
    let f = AccessFixture::new().await;
    *f.h.provider.mode.lock().await = "owner".into();
    let browser = Browser::new("member-access");
    let root = format!("{}/-/organizations/{}", f.h.origin, f.organization);
    browser.run(&["open", &root]).await;
    browser.sign_in().await;
    let action = format!("View repository access for {}", f.member);
    browser
        .run(&["wait", "--text", "Organization members"])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", &action, "--exact",
        ])
        .await;
    browser.run(&["wait", "[role=dialog]"]).await;
    browser
        .run(&["wait", "--text", "Release engineering"])
        .await;
    browser
        .run(&["wait", "--text", "Direct grant · Read"])
        .await;
    f.remove_team_member().await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Refresh access",
            "--exact",
        ])
        .await;
    browser.run(&["wait","--fn", "document.querySelector('[role=dialog] li')?.innerText.includes('Direct grant · Read') && !document.querySelector('[role=dialog]').innerText.includes('Release engineering')"]).await;
    browser.run(&["close"]).await;
    f.h.close().await;
}
