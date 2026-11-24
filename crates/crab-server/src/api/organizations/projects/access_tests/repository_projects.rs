use super::{
    outside::{create_repository, repository_path, repository_write},
    *,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repository_project_links_require_current_ownership_and_both_read_grants() {
    let h = Harness::new(false).await;
    let other = h.server.repositories.values()[0].id;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(other)
        .await
        .unwrap();
    let (organization, _, member) = bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let repository = create_repository(&h, &root, &alice, "planning").await;
    let repository_id = &repository["id"];
    let (project, _) = create(&h, &root, &alice).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    join(&h, &root, &alice, &bob, member, 0).await;
    let path = format!(
        "{root}/projects?repository={}",
        repository_id.as_str().unwrap()
    );
    let candidates = format!("{path}&linkable=true&search=release");
    let commands = format!("{root}/projects/commands");
    let get_as_bob = async |path: &str| {
        h.http
            .get(format!("{}{path}", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
    };
    let set_repository_grants = async |members: Value| {
        let members_path = format!("{}/members", repository_path(&repository));
        let state = h.json(&members_path, &alice).await;
        repository_write(
            &h,
            &alice,
            &repository,
            Method::PUT,
            &members_path,
            json!({"expected_revision":state["revision"],"members":members,"groups":[]}),
        )
        .await;
    };
    let link =
        |id: Value| json!({"kind":"link_repository","project":project,"version":1,"repository":id});
    assert_eq!(get_as_bob(&path).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(h.json(&path, &alice).await["items"], json!([]));
    assert_eq!(
        h.json(&candidates, &alice).await["items"][0]["id"],
        json!(project)
    );
    set_repository_grants(json!([{"subject":"bob-id","name":"Bob","access":"read"}])).await;
    assert_eq!(h.json(&candidates, &bob).await["items"], json!([]));
    assert_eq!(
        h.write(
            Method::POST,
            &commands,
            &bob,
            json!({"request_id":Uuid::now_v7(),"command":link(repository_id.clone())})
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let rejected = project_command(&h, &root, &alice, link(json!(other))).await;
    assert_eq!(rejected["outcome"], "rejected", "{rejected}");
    let input = json!({"request_id":Uuid::now_v7(),"command":link(repository_id.clone())});
    let save = async || {
        h.write(Method::POST, &commands, &alice, input.clone())
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let result = save().await;
    assert_eq!(result["outcome"], "applied", "{result}");
    assert_eq!(save().await, result);
    assert_eq!(h.json(&path, &bob).await["items"][0]["id"], json!(project));
    assert_eq!(h.json(&candidates, &alice).await["items"], json!([]));
    let access = |version, target, role| json!({"kind":"set_access","project":project,"version":version,"target":target,"role":role});
    assert_eq!(
        project_command(&h, &root, &alice, access(2, json!({"kind":"base"}), "none")).await["outcome"],
        "applied"
    );
    assert_eq!(h.json(&path, &bob).await["items"], json!([]));
    assert_eq!(
        project_command(
            &h,
            &root,
            &alice,
            access(3, json!({"kind":"member","id":member}), "read")
        )
        .await["outcome"],
        "applied"
    );
    assert_eq!(h.json(&path, &bob).await["items"][0]["id"], json!(project));
    set_repository_grants(json!([])).await;
    assert_eq!(get_as_bob(&path).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(project_command(&h, &root, &alice, json!({
        "kind":"repository","project":project,"version":4,"repository":repository_id,"linked":false
    })).await["outcome"], "applied");
    assert_eq!(h.json(&path, &alice).await["items"], json!([]));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_links_a_shared_project_from_a_repository() {
    use crate::test_support::http::browser::Browser;

    let h = Harness::new(false).await;
    let (organization, _, _) = bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let repo = create_repository(&h, &root, &alice, "planning").await;
    let (project, _) = create(&h, &root, &alice).await;
    let browser = Browser::new("repo-projects");
    let path = format!(
        "{}/{}/planning/projects",
        h.origin,
        repo["owner"].as_str().unwrap()
    );
    browser.run(&["open", &path]).await;
    browser.sign_in().await;
    let collection = "nav[aria-label='Project collections'] a[href$='collection=linked']";
    browser.run(&["wait", collection]).await;
    browser.run(&["click", collection]).await;
    browser.run(&["wait", "--text", "No linked projects"]).await;
    browser
        .run(&[
            "find", "role", "tab", "click", "--name", "Archived", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No archived projects"])
        .await;
    assert!(
        !browser
            .run(&["snapshot"])
            .await
            .contains("button \"Link a project\"")
    );
    browser
        .run(&[
            "find", "role", "tab", "click", "--name", "Active", "--exact",
        ])
        .await;
    browser
        .run(&["fill", "[aria-label='Search projects']", "other work"])
        .await;
    browser
        .run(&["wait", "--text", "No matching projects"])
        .await;
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
    browser.run(&["wait", "a.workspace-project-link"]).await;
    browser.run(&["reload"]).await;
    browser.run(&["wait", "a.workspace-project-link"]).await;
    browser.run(&["click", "a.workspace-project-link"]).await;
    browser
        .run(&[
            "wait",
            "--fn",
            &format!("location.pathname.endsWith('/projects/{project}')"),
        ])
        .await;
    browser.run(&["wait", "--text", "Release triage"]).await;
    assert_eq!(
        h.json(&format!("{root}/projects/{project}"), &alice).await["repositories"][0]["id"],
        repo["id"]
    );
    drop(browser);
    h.close().await;
}
