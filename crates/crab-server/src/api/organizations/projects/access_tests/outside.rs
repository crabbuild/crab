use super::*;
use crate::test_support::http::browser::Browser;

struct Fixture {
    h: Harness,
    organization: Uuid,
    guest: Uuid,
    project: Uuid,
    alice: String,
    bob: String,
    root: String,
    repository: Value,
}

impl Fixture {
    async fn new() -> Self {
        let h = Harness::new(false).await;
        let (organization, _, guest) = bootstrap(&h).await;
        let alice = h.login().await;
        let root = format!("/api/organizations/{organization}");
        let (project, _) = create(&h, &root, &alice).await;
        let repository = create_repository(&h, &root, &alice, "guest-source").await;
        *h.provider.mode.lock().await = "member".into();
        let bob = h.login().await;
        h.write(
            Method::POST,
            "/api/profile",
            &bob,
            json!({"request_id":Uuid::now_v7(),"handle":"bob-outside","name":"Bob Outside"}),
        )
        .await
        .error_for_status()
        .unwrap();
        let fixture = Self {
            h,
            organization,
            guest,
            project,
            alice,
            bob,
            root,
            repository,
        };
        fixture
            .repository_grants(json!([{"subject":"bob-id","name":"Bob Outside","access":"read"}]))
            .await;
        fixture
    }

    fn project_path(&self) -> String {
        format!("{}/projects/{}", self.root, self.project)
    }

    async fn repository_grants(&self, members: Value) {
        let root = repository_path(&self.repository);
        let path = format!("{root}/members");
        let current = self.h.json(&path, &self.alice).await;
        repository_write(
            &self.h,
            &self.alice,
            &self.repository,
            Method::PUT,
            &path,
            json!({"expected_revision":current["revision"],"members":members,"groups":[]}),
        )
        .await;
    }
}

pub(super) async fn create_repository(h: &Harness, root: &str, alice: &str, name: &str) -> Value {
    h.write(
        Method::POST,
        &format!("{root}/repositories/creation"),
        alice,
        json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

pub(super) fn repository_path(repository: &Value) -> String {
    format!(
        "/api/repos/{}/{}",
        repository["owner"].as_str().unwrap(),
        repository["name"].as_str().unwrap()
    )
}

pub(super) async fn repository_write(
    h: &Harness,
    alice: &str,
    repository: &Value,
    method: Method,
    path: &str,
    body: Value,
) {
    let session = h.json("/api/session", alice).await;
    h.http
        .request(method, format!("{}{path}", h.origin))
        .header("cookie", alice)
        .header("origin", &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repository["id"].as_str().unwrap())
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outside_project_grants_require_current_repository_access_and_redact_private_history() {
    let f = Fixture::new().await;
    let path = f.project_path();
    let commands = format!("{}/projects/commands", f.root);
    let guest_get = |path: String| {
        let f = &f;
        async move {
            f.h.http
                .get(format!("{}{path}", f.h.origin))
                .header("cookie", &f.bob)
                .send()
                .await
                .unwrap()
        }
    };
    let guest_write = |body: Value| {
        let (f, commands) = (&f, &commands);
        async move { f.h.write(Method::POST, commands, &f.bob, body).await }
    };
    assert_eq!(
        guest_get(path.clone()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        guest_get(f.root.clone()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.h.json(&format!("{}/projects/context", f.root), &f.bob)
            .await["can_create"],
        false
    );
    let candidates =
        f.h.json(&format!("{path}/collaborators?q=bob-outside"), &f.alice)
            .await;
    assert_eq!(candidates["items"][0]["id"], json!(f.guest));
    let role = |target, version, value| json!({"kind":"set_access","project":f.project,"version":version,"target":{"kind":"outside","id":target},"role":value});
    assert_eq!(
        project_command(&f.h, &f.root, &f.alice, role(Uuid::now_v7(), 1, "write")).await["outcome"],
        "rejected"
    );
    assert_eq!(
        project_command(&f.h, &f.root, &f.alice, role(f.guest, 1, "write")).await["outcome"],
        "applied"
    );
    let another = Uuid::now_v7();
    assert_eq!(
        project_command(
            &f.h,
            &f.root,
            &f.alice,
            json!({"kind":"create","id":another,"name":"Internal plan","template":"board"})
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(
        guest_get(format!("{}/projects/{another}", f.root))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let listing = f.h.json(&format!("{}/projects", f.root), &f.bob).await;
    assert_eq!(listing["items"].as_array().unwrap().len(), 1);
    assert_eq!(listing["items"][0]["id"], json!(f.project));
    assert_eq!(listing["can_create"], false);
    assert_eq!(f.h.json(&path, &f.bob).await["can_manage"], true);
    assert_eq!(
        guest_get(format!("{path}/access")).await.status(),
        StatusCode::NOT_FOUND
    );

    let private = create_repository(&f.h, &f.root, &f.alice, "confidential-source").await;
    repository_write(&f.h, &f.alice, &private, Method::POST, &format!("{}/issues", repository_path(&private)),
        json!({"request_id":Uuid::now_v7(),"title":"Private source title","body":"Private source body"})).await;
    let work = |id: Uuid, subject: Value, title: &str| json!({"id":id,"subject":subject,"title":title,"body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}});
    let mut definition = f.h.json(&path, &f.alice).await["project"]["definition"].clone();
    let notes = Uuid::now_v7().to_string();
    definition["fields"] =
        json!([{"id":notes,"name":"Planning notes","definition":{"kind":"text"}}]);
    let mut linked = work(Uuid::now_v7(), json!({"kind":"issue","number":1}), "");
    linked["values"][&notes] = json!("Private planning note");
    for command in [
        json!({"kind":"edit","project":f.project,"version":2,"definition":definition,"archived":false}),
        json!({"kind":"repository","project":f.project,"version":3,"repository":private["id"],"linked":true}),
        json!({"kind":"item","project":f.project,"version":0,"repository":private["id"],"item":linked}),
    ] {
        let outcome = project_command(&f.h, &f.root, &f.alice, command).await;
        assert_eq!(outcome["outcome"], "applied", "{outcome}");
    }
    let items = f.h.json(&format!("{path}/items"), &f.bob).await;
    assert_eq!(items["page"]["items"][0]["restricted"], true);
    assert!(!items.to_string().contains("Private planning note"));
    let grouped =
        f.h.json(&format!("{path}/items?group_by=field:{notes}"), &f.bob)
            .await;
    assert!(!grouped.to_string().contains("Private planning note"));
    let owner_history = f.h.json(&format!("{path}/history"), &f.alice).await;
    assert!(owner_history.to_string().contains("Private planning note"));
    let history = f.h.json(&format!("{path}/history"), &f.bob).await;
    assert!(
        history["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["target"]["kind"] == "item"
                && event["target"]["restricted"] == true
                && event["changes"] == json!([]))
    );
    assert!(!history.to_string().contains("Private planning note"));
    let draft = json!({"request_id":Uuid::now_v7(),"command":{"kind":"item","project":f.project,"version":0,"repository":null,"item":work(Uuid::now_v7(),Value::Null,"Guest plan")}});
    let result = guest_write(draft.clone())
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(result["outcome"], "applied");
    let version = result["version"].as_u64().unwrap();
    assert_eq!(
        project_command(&f.h, &f.root, &f.alice, role(f.guest, version, "admin")).await["outcome"],
        "applied"
    );
    assert_eq!(
        f.h.json(&format!("{path}/access"), &f.bob).await["can_manage_teams"],
        false
    );

    f.repository_grants(json!([])).await;
    for suffix in ["", "/items", "/history", "/access", "/filter-repositories"] {
        assert_eq!(
            guest_get(format!("{path}{suffix}")).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    let saved = guest_write(draft)
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(result, saved);
    assert_eq!(guest_write(json!({"request_id":Uuid::now_v7(),"command":{"kind":"create","id":Uuid::now_v7(),"name":"Unauthorized","template":"board"}})).await.status(), StatusCode::FORBIDDEN);
    let access = f.h.json(&format!("{path}/access"), &f.alice).await;
    assert_eq!(access["grants"][0]["active"], false);
    f.repository_grants(json!([{"subject":"bob-id","name":"Bob Outside","access":"read"}]))
        .await;
    assert_eq!(f.h.json(&path, &f.bob).await["can_admin"], true);
    join(&f.h, &f.root, &f.alice, &f.bob, f.guest, 0).await;
    assert_eq!(f.h.json(&path, &f.bob).await["can_admin"], false);
    let member = format!("{}/members/{}", f.root, f.guest);
    let current = f.h.json(&member, &f.alice).await;
    f.h.write(
        Method::DELETE,
        &member,
        &f.alice,
        json!({"version":current["member"]["version"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        guest_get(path.clone()).await.status(),
        StatusCode::NOT_FOUND
    );
    let version = access["version"].as_u64().unwrap();
    assert_eq!(
        project_command(&f.h, &f.root, &f.alice, role(f.guest, version, "write")).await["outcome"],
        "rejected"
    );
    f.repository_grants(json!([{"subject":"bob-id","name":"Bob Outside","access":"read"}]))
        .await;
    assert_eq!(
        guest_get(path.clone()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        project_command(&f.h, &f.root, &f.alice, role(f.guest, version, "write")).await["outcome"],
        "applied"
    );
    assert_eq!(f.h.json(&path, &f.bob).await["can_manage"], true);
    assert_eq!(
        project_command(&f.h, &f.root, &f.alice, role(f.guest, version + 1, "none")).await["outcome"],
        "applied"
    );
    assert_eq!(guest_get(path).await.status(), StatusCode::NOT_FOUND);
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_shares_an_organization_project_with_an_outside_collaborator() {
    let f = Fixture::new().await;
    let url = format!(
        "{}/-/organizations/{}/projects/{}",
        f.h.origin, f.organization, f.project
    );
    *f.h.provider.mode.lock().await = "owner".into();
    let owner = Browser::new("project-outside-owner");
    owner.run(&["open", &url]).await;
    owner.sign_in().await;
    owner.run(&["wait", "--text", "Manage access"]).await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Manage access",
            "--exact",
        ])
        .await;
    owner
        .choose_select("[aria-label='Collaborator type']", "outside")
        .await;
    owner
        .run(&["fill", "[aria-label='Find collaborator']", "bob-outside"])
        .await;
    owner
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    owner
        .choose_select("[aria-label='Collaborator']", &f.guest.to_string())
        .await;
    owner
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Grant access",
            "--exact",
        ])
        .await;
    owner
        .run(&[
            "wait",
            "--fn",
            "document.querySelector('[aria-label=\"Project grants\"]')?.innerText.includes('Bob Outside')",
        ])
        .await;
    *f.h.provider.mode.lock().await = "member".into();
    let guest = Browser::new("project-outside-guest");
    guest.run(&["open", &url]).await;
    guest.sign_in().await;
    guest.run(&["wait", "--text", "Release triage"]).await;
    guest.run(&["wait", "--text", "Add work"]).await;
    let rendered = guest.run(&["snapshot"]).await;
    assert!(!rendered.contains("link \"People\""));
    assert!(!rendered.contains("Manage access"));
    drop(guest);
    drop(owner);
    f.h.close().await;
}
