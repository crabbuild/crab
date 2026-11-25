use super::{repositories::repository_write, *};
use crate::test_support::http::{browser::Browser, organizations::bootstrap};

mod conversion;
mod removal;

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
async fn owner_finds_outside_collaborators_and_revokes_a_repository_grant() {
    let f = AccessFixture::new().await;
    let people = format!("{}/outside-collaborators", f.org);
    let member = format!("{}/members/{}", f.org, f.member);
    assert_eq!(f.get(&people, &f.bob).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(f.h.json(&people, &f.alice).await["items"], json!([]));

    let repo = f.repository.strip_prefix("/api/repos/").unwrap();
    let (owner, _) = repo.split_once('/').unwrap();
    let other = format!("/api/repos/{owner}/private-tool");
    let other_members = format!("{other}/members");
    let snapshot =
        f.h.server
            .catalog()
            .unwrap()
            .membership(owner, "private-tool")
            .await
            .unwrap();
    let write_other = async |revision: Value, members: Value| {
        repository_write(
            &f.h,
            Method::PUT,
            &other_members,
            &f.alice,
            snapshot.repository_id,
            json!({
                "expected_revision": revision, "members": members, "groups": [],
            }),
        )
        .await
        .error_for_status()
        .unwrap()
    };
    let unregistered =
        json!({"subject":"contractor-id","name":"External Contractor","access":"read"});
    write_other(
        json!(snapshot.revision),
        json!([
            {"subject":"bob-id","name":"Bob","access":"write"}, unregistered
        ]),
    )
    .await;
    let membership = f.h.json(&member, &f.alice).await;
    f.h.write(
        Method::DELETE,
        &member,
        &f.alice,
        json!({"version":membership["member"]["version"]}),
    )
    .await
    .error_for_status()
    .unwrap();

    let first = f.h.json(&format!("{people}?limit=1"), &f.alice).await;
    assert_eq!(first["items"][0]["name"], "Bob");
    assert_eq!(first["items"][0]["repository_count"], 2);
    assert!(!first.to_string().contains("bob-id"));
    let people_next = first["next"].as_str().unwrap();
    let following =
        f.h.json(&format!("{people}?after={people_next}"), &f.alice)
            .await;
    assert_eq!(following["items"][0]["name"], "External Contractor");
    assert_eq!(following["items"][0]["profile"], Value::Null);
    let searched = f.h.json(&format!("{people}?q=contractor"), &f.alice).await;
    assert_eq!(searched["items"], following["items"]);

    let access = format!(
        "{people}/{}/repositories",
        first["items"][0]["key"].as_str().unwrap()
    );
    assert_eq!(f.get(&access, &f.bob).await.status(), StatusCode::NOT_FOUND);
    let grants = f.h.json(&format!("{access}?limit=1"), &f.alice).await;
    assert_eq!(grants["items"][0]["name"], "library");
    assert_eq!(grants["items"][0]["access"], "read");
    let grant_page = format!("{access}?after={}", grants["next"].as_str().unwrap());
    assert_eq!(
        f.h.json(&grant_page, &f.alice).await["items"][0]["access"],
        "write"
    );
    assert_eq!(
        f.get(&format!("{other}/refs"), &f.bob).await.status(),
        StatusCode::OK
    );

    write_other(
        f.h.json(&other_members, &f.alice).await["revision"].clone(),
        json!([unregistered]),
    )
    .await;
    assert_eq!(
        f.get(&grant_page, &f.alice).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.get(&format!("{other}/refs"), &f.bob).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(f.h.json(&access, &f.alice).await["items"], grants["items"]);
    assert_eq!(
        f.get(&format!("{}/refs", f.repository), &f.bob)
            .await
            .status(),
        StatusCode::OK
    );

    let current_page = f.h.json(&format!("{people}?limit=1"), &f.alice).await;
    let after = current_page["next"].as_str().unwrap();
    let invite = format!("{}/invitations/{}", f.org, f.member);
    let previous = f.h.json(&invite, &f.alice).await;
    let pending =
        f.h.write(
            Method::PUT,
            &invite,
            &f.alice,
            json!({
                "role":"member", "version":previous["invitation"]["version"],
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    f.h.write(
        Method::POST,
        &format!("{}/invitation", f.org),
        &f.bob,
        json!({
            "accept":true, "version":pending["version"],
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        f.get(&format!("{people}?after={after}"), &f.alice)
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.get(&access, &f.alice).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.h.write(
            Method::POST,
            &format!("{people}/removal-preview"),
            &f.alice,
            json!({"collaborators":[current_page["items"][0]["key"]]}),
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.h.json(&people, &f.alice).await["items"],
        following["items"]
    );
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bulk_outside_removal_revokes_all_selected_sources_and_survives_restore() {
    use crate::{
        RepositoryAccess, RepositoryMember,
        catalog::{
            MembershipActor,
            deletion::{Delete, Restore},
        },
    };

    let f = AccessFixture::new().await;
    let catalog = f.h.server.catalog().unwrap();
    let repo = f.repository.strip_prefix("/api/repos/").unwrap();
    let (owner, _) = repo.split_once('/').unwrap();
    let members = [
        ("bob-id", "Bob"),
        ("vendor-id", "Vendor"),
        ("contractor-id", "Contractor"),
    ]
    .map(|(subject, name)| RepositoryMember {
        subject: subject.into(),
        name: name.into(),
        access: RepositoryAccess::Read,
    });
    for name in ["library", "private-tool"] {
        catalog
            .set_members(owner, name, members.to_vec(), true)
            .await
            .unwrap();
    }
    let retired: Value =
        f.h.write(
            Method::POST,
            &format!("{}/repositories/creation", f.org),
            &f.alice,
            json!({"request_id":Uuid::now_v7(),"name":"retired","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let repository = Uuid::parse_str(retired["id"].as_str().unwrap()).unwrap();
    catalog
        .set_members(owner, "retired", members.to_vec(), true)
        .await
        .unwrap();
    let (document, _) = catalog.load().await.unwrap();
    let record = document.repository_by_id(repository).unwrap();
    let actor = MembershipActor {
        issuer: "urn:crab:local".into(),
        subject: "operator".into(),
    };
    let operation = Uuid::now_v7();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    catalog
        .delete_repository(
            document.version,
            Delete {
                repository,
                organization: f.organization,
                actor: actor.clone(),
                operation,
                expected_version: record.name_version(),
                now,
            },
        )
        .await
        .unwrap();

    let root = format!("{}/outside-collaborators", f.org);
    let preview = format!("{root}/removal-preview");
    let remove = format!("{root}/removals");
    let prior = f.h.json(&root, &f.alice).await;
    assert_eq!(prior["items"].as_array().unwrap().len(), 2);
    let denied =
        f.h.write(
            Method::POST,
            &preview,
            &f.bob,
            json!({
                "collaborators":[prior["items"][0]["key"]],
            }),
        )
        .await;
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    let member = format!("{}/members/{}", f.org, f.member);
    let membership = f.h.json(&member, &f.alice).await;
    f.h.write(
        Method::DELETE,
        &member,
        &f.alice,
        json!({
            "version":membership["member"]["version"],
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    let listed = f.h.json(&root, &f.alice).await;
    let keys: Vec<_> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] != "Contractor")
        .map(|row| row["key"].clone())
        .collect();
    assert_eq!(keys.len(), 2);
    let review = async |collaborators: Vec<Value>| {
        f.h.write(
            Method::POST,
            &preview,
            &f.alice,
            json!({ "collaborators":collaborators }),
        )
        .await
    };
    let revoke = async |plan: Value| f.h.write(Method::POST, &remove, &f.alice, plan).await;
    assert_eq!(
        review(vec![keys[0].clone(), json!("0".repeat(64))])
            .await
            .status(),
        StatusCode::CONFLICT,
    );
    assert_eq!(f.h.json(&root, &f.alice).await["items"], listed["items"]);
    let token: Value =
        f.h.write(
            Method::POST,
            "/api/git-token",
            &f.bob,
            json!({
                "owner":owner, "repository":"library","access":"read",
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let git_advertisement = format!(
        "{}/git/{owner}/library.git/info/refs?service=git-upload-pack",
        f.h.origin
    );
    let fetch = async || {
        f.h.http
            .get(&git_advertisement)
            .header("git-protocol", "version=2")
            .basic_auth("crab", token["token"].as_str())
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(fetch().await, StatusCode::OK);
    let reviewed: Value = review(keys)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reviewed["impact"],
        json!({"collaborators":2, "repositories":3, "grants":6})
    );
    let removed: Value = revoke(reviewed["plan"].clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        removed,
        json!({"collaborators":2, "repositories":3, "grants":6})
    );
    assert_eq!(
        revoke(reviewed["plan"].clone())
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap(),
        removed
    );
    assert_eq!(
        f.get(&format!("{}/refs", f.repository), &f.bob)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(fetch().await, StatusCode::NOT_FOUND);

    catalog
        .restore_repository(Restore {
            repository,
            organization: f.organization,
            operation,
            actor,
            now: now + 1,
        })
        .await
        .unwrap();
    for name in ["library", "private-tool", "retired"] {
        assert_eq!(
            catalog.membership(owner, name).await.unwrap().members,
            vec![members[2].clone()]
        );
    }
    assert_eq!(
        catalog.membership(owner, "library").await.unwrap().groups[0].group,
        "release-admins"
    );
    let audit =
        f.h.json(
            &format!("{}/audit-log?action=outside_collaborators_removed", f.org),
            &f.alice,
        )
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        audit["items"][0]["detail"],
        "2 collaborators · 6 direct grants · 3 repositories"
    );
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
