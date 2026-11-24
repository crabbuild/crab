use super::*;
use crate::test_support::http::{Harness, organizations::bootstrap};
use reqwest::{Method, StatusCode};

mod outside;
mod repository_projects;
mod team_projects;

async fn join(h: &Harness, root: &str, alice: &str, bob: &str, member: Uuid, version: u64) {
    let offer = h
        .write(
            Method::PUT,
            &format!("{root}/invitations/{member}"),
            alice,
            json!({"version":version,"role":"member"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    h.write(
        Method::POST,
        &format!("{root}/invitation"),
        bob,
        json!({"version":offer["version"],"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();
}

async fn project_command(h: &Harness, root: &str, session: &str, command: Value) -> Value {
    h.write(
        Method::POST,
        &format!("{root}/projects/commands"),
        session,
        json!({"request_id":Uuid::now_v7(),"command":command}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn create(h: &Harness, root: &str, alice: &str) -> (Uuid, Uuid) {
    let (project, team) = (Uuid::now_v7(), Uuid::now_v7());
    assert_eq!(
        project_command(
            h,
            root,
            alice,
            json!({"kind":"create","id":project,"name":"Release triage","template":"board"})
        )
        .await["outcome"],
        "applied"
    );
    h.write(Method::POST, &format!("{root}/teams/commands"), alice, json!({"request_id":team,"command":{"kind":"create","name":"Release engineering","description":"","visibility":"closed"}})).await.error_for_status().unwrap();
    (project, team)
}

#[tokio::test]
async fn project_grants_follow_active_membership_and_hide_private_projects() {
    let h = Harness::new(false).await;
    let (organization, _, member) = bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{organization}");
    join(&h, &root, &alice, &bob, member, 0).await;
    let (project, team) = create(&h, &root, &alice).await;
    let detail = format!("{root}/projects/{project}");
    let access = |version: u64, target: Value, role: &str| json!({"kind":"set_access","project":project,"version":version,"target":target,"role":role});
    let base = json!({"kind":"base"});
    let person = json!({"kind":"member","id":member});
    let save = |command: Value| project_command(&h, &root, &alice, command);
    let post = |session: String, command: Value| {
        let (h, root) = (&h, &root);
        async move {
            h.write(
                Method::POST,
                &format!("{root}/projects/commands"),
                &session,
                json!({"request_id":Uuid::now_v7(),"command":command}),
            )
            .await
        }
    };
    let get = |path: String| {
        let (h, bob) = (&h, &bob);
        async move {
            h.http
                .get(format!("{}{path}", h.origin))
                .header("cookie", bob)
                .send()
                .await
                .unwrap()
        }
    };
    assert_eq!(h.json(&detail, &bob).await["can_manage"], false);
    assert_eq!(
        save(access(1, base.clone(), "none")).await["outcome"],
        "applied"
    );
    assert_eq!(
        h.json(&format!("{root}/projects"), &bob).await["items"],
        json!([])
    );
    for suffix in ["", "/items", "/history", "/access", "/filter-repositories"] {
        assert_eq!(
            get(format!("{detail}{suffix}")).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    assert_eq!(
        save(access(2, person.clone(), "write")).await["outcome"],
        "applied"
    );
    let current = h.json(&detail, &bob).await;
    assert_eq!(current["can_manage"], true);
    assert_eq!(current["can_admin"], false);
    assert_eq!(
        post(bob.clone(), access(3, base.clone(), "admin"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(post(bob.clone(), json!({"kind":"edit","project":project,"version":3,"definition":current["project"]["definition"],"archived":true})).await.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        get(format!("{detail}/access")).await.status(),
        StatusCode::NOT_FOUND
    );
    let draft = json!({"id":Uuid::now_v7(),"subject":null,"title":"Team release task","body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}});
    assert_eq!(
        project_command(
            &h,
            &root,
            &bob,
            json!({"kind":"item","project":project,"version":0,"repository":null,"item":draft})
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(
        save(access(3, person.clone(), "admin")).await["outcome"],
        "rejected"
    );
    assert_eq!(
        save(access(4, person.clone(), "admin")).await["outcome"],
        "applied"
    );
    assert_eq!(
        h.json(&format!("{detail}/access"), &bob).await["grants"][0]["role"],
        "admin"
    );
    assert_eq!(
        project_command(&h, &root, &bob, access(5, base, "none")).await["outcome"],
        "applied"
    );
    assert_eq!(
        save(access(6, person.clone(), "none")).await["outcome"],
        "applied"
    );
    assert_eq!(
        save(access(7, json!({"kind":"team","id":team}), "write")).await["outcome"],
        "applied"
    );

    for (version, role) in [(0, json!("member")), (1, Value::Null)] {
        h.write(Method::POST, &format!("{root}/teams/commands"), &alice, json!({"request_id":Uuid::now_v7(),"command":{"kind":"member","team":team,"member":member,"version":version,"role":role}})).await.error_for_status().unwrap();
        if version == 0 {
            assert_eq!(h.json(&detail, &bob).await["can_manage"], true);
        } else {
            assert_eq!(get(detail.clone()).await.status(), StatusCode::NOT_FOUND);
        }
    }
    assert_eq!(save(access(8, person, "read")).await["outcome"], "applied");
    assert_eq!(h.json(&detail, &bob).await["can_manage"], false);
    let remove = format!("{root}/members/{member}");
    let record = h.json(&remove, &alice).await;
    h.write(
        Method::DELETE,
        &remove,
        &alice,
        json!({"version":record["member"]["version"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    join(&h, &root, &alice, &bob, member, 2).await;
    assert_eq!(get(detail).await.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn organization_project_policy_binds_confirmation_to_the_inheriting_set() {
    let h = Harness::new(false).await;
    let (organization, _, member) = bootstrap(&h).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let root = format!("/api/organizations/{organization}");
    join(&h, &root, &alice, &bob, member, 0).await;
    let (first, second) = (Uuid::now_v7(), Uuid::now_v7());
    for (id, name) in [(first, "First plan"), (second, "Second plan")] {
        assert_eq!(
            project_command(
                &h,
                &root,
                &alice,
                json!({"kind":"create","id":id,"name":name,"template":"board"})
            )
            .await["outcome"],
            "applied"
        );
    }
    let base = |project: Uuid, version: u64, role: Value, default_version: Option<u64>| {
        json!({
            "kind":"set_access","project":project,"version":version,"target":{"kind":"base"},"role":role,"default_version":default_version
        })
    };
    assert_eq!(
        project_command(&h, &root, &alice, base(second, 1, json!("read"), None)).await["outcome"],
        "applied"
    );
    let policy = format!("{root}/project-policy");
    assert_eq!(
        h.http
            .get(format!("{}{policy}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let stale = h.json(&policy, &alice).await;
    assert_eq!(stale["affected_projects"], 1);
    assert_eq!(
        project_command(&h, &root, &alice, base(first, 1, json!("read"), None)).await["outcome"],
        "applied"
    );
    assert_eq!(
        project_command(&h, &root, &alice, base(second, 2, Value::Null, Some(0))).await["outcome"],
        "applied"
    );
    let current = h.json(&policy, &alice).await;
    assert_eq!(current["affected_projects"], stale["affected_projects"]);
    assert_ne!(current["scope"], stale["scope"]);
    let change = |displayed: &Value| json!({"kind":"set_default_access","version":displayed["version"],"role":"none","scope":displayed["scope"]});
    assert_eq!(
        project_command(&h, &root, &alice, change(&stale)).await["outcome"],
        "rejected"
    );
    let submission = json!({"request_id":Uuid::now_v7(), "command":change(&current)});
    let path = format!("{root}/projects/commands");
    assert_eq!(
        h.write(Method::POST, &path, &bob, submission.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        let receipt = h
            .write(Method::POST, &path, &alice, submission.clone())
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(
            receipt,
            json!({"outcome":"applied","project":null,"version":1})
        );
    }
    let third = Uuid::now_v7();
    assert_eq!(
        project_command(
            &h,
            &root,
            &alice,
            json!({"kind":"create","id":third,"name":"New plan","template":"table"})
        )
        .await["outcome"],
        "applied"
    );
    let listing = h.json(&format!("{root}/projects"), &bob).await;
    assert_eq!(listing["items"].as_array().unwrap().len(), 1);
    assert_eq!(listing["items"][0]["id"], json!(first));
    for id in [second, third] {
        let read = h
            .http
            .get(format!("{}{root}/projects/{id}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap();
        assert_eq!(read.status(), StatusCode::NOT_FOUND);
    }
    assert_eq!(
        project_command(&h, &root, &alice, base(first, 2, Value::Null, Some(0))).await["outcome"],
        "rejected"
    );
    assert_eq!(
        project_command(&h, &root, &alice, base(first, 2, Value::Null, Some(1))).await["outcome"],
        "applied"
    );
    assert_eq!(
        h.json(&format!("{root}/projects"), &bob).await["items"],
        json!([])
    );
    let audit = h
        .json(
            &format!("{root}/audit-log?action=project_policy_changed"),
            &alice,
        )
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1);
    assert_eq!(audit["items"][0]["action"], "project_policy_changed");
    assert!(
        audit["items"][0]["detail"]
            .as_str()
            .unwrap()
            .contains("Read → None")
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_confirms_the_organization_project_default_and_affected_count() {
    use crate::test_support::http::browser::Browser;
    let h = Harness::new(false).await;
    let (organization, _, _) = bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let (project, _) = create(&h, &root, &alice).await;
    let browser = Browser::new("project-default-access");
    browser.run(&["set", "viewport", "1280", "900"]).await;
    browser
        .run(&[
            "open",
            &format!("{}/-/organizations/{organization}?tab=settings", h.origin),
        ])
        .await;
    browser.sign_in().await;
    let selector = "[aria-label='Default Project access']";
    browser.run(&["wait", selector]).await;
    browser.choose_select(selector, "none").await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Review change",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "This affects 1 existing Project"])
        .await;
    assert_eq!(
        h.json(&format!("{root}/project-policy"), &alice).await["role"],
        "read"
    );
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Change default to No access",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--fn", "document.querySelector('[aria-label=\"Default Project access\"]')?.textContent.trim() === 'No access' && [...document.querySelectorAll('button')].some(button => button.textContent.trim() === 'Review change' && button.disabled)"]).await;
    let detail = h
        .json(&format!("{root}/projects/{project}/access"), &alice)
        .await;
    assert_eq!(detail["base"], "none");
    assert_eq!(detail["base_override"], Value::Null);
    drop(browser);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_grants_an_organization_team_project_write_access() {
    use crate::test_support::http::browser::Browser;
    let h = Harness::new(false).await;
    let (organization, _, _) = bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let (project, team) = create(&h, &root, &alice).await;
    let browser = Browser::new("project-access");
    browser.run(&["set", "viewport", "1280", "900"]).await;
    browser
        .run(&[
            "open",
            &format!(
                "{}/-/organizations/{organization}/projects/{project}",
                h.origin
            ),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Manage access"]).await;
    browser
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
    browser
        .run(&["wait", "[aria-label='Collaborator type']"])
        .await;
    browser
        .choose_select("[aria-label='Collaborator type']", "team")
        .await;
    browser
        .choose_select("[aria-label='Collaborator']", &team.to_string())
        .await;
    browser
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
    browser
        .run(&["wait", "[aria-label='Role for Release engineering']"])
        .await;
    let path = format!("{root}/projects/{project}/access");
    assert_eq!(
        h.json(&path, &alice).await["grants"],
        json!([{"target":{"kind":"team","id":team},"role":"write","active":true,"label":"Release engineering"}])
    );
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Remove Release engineering grant",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No explicit grants."])
        .await;
    assert_eq!(h.json(&path, &alice).await["grants"], json!([]));
    browser
        .run(&[
            "open",
            &format!(
                "{}/-/organizations/{organization}?tab=teams&team={team}",
                h.origin
            ),
        ])
        .await;
    browser
        .run(&["click", "nav[aria-label='Team'] a[href$='view=projects']"])
        .await;
    browser.run(&["wait", "--text", "No linked projects"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Link a project",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "fill",
            "[aria-label='Find a project to link']",
            "Release triage",
        ])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    browser
        .choose_select("[aria-label='Project']", &project.to_string())
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Link project",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--fn", "document.querySelector('a.workspace-project-link')?.textContent.includes('Release triage')"])
        .await;
    assert_eq!(h.json(&path, &alice).await["grants"][0]["role"], "read");
    browser.run(&["reload"]).await;
    browser.run(&["wait", "a.workspace-project-link"]).await;
    drop(browser);
    h.close().await;
}
