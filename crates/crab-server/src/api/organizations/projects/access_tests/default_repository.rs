use super::{
    outside::{create_repository, repository_path, repository_write},
    *,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_repository_links_atomically_and_hides_inaccessible_destinations() {
    let h = Harness::new(false).await;
    let (organization, _, member) = bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let repo = create_repository(&h, &root, &alice, "delivery").await;
    let id = &repo["id"];
    let (project, _) = create(&h, &root, &alice).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    join(&h, &root, &alice, &bob, member, 0).await;
    let detail = format!("{root}/projects/{project}");
    let scoped = format!("{root}/projects?repository={}", id.as_str().unwrap());
    let path = format!("{root}/projects/commands");
    let set = |version, repository: Value| {
        json!({
            "kind":"set_default_repository","project":project,"version":version,"repository":repository
        })
    };
    assert_eq!(
        h.write(
            Method::POST,
            &path,
            &bob,
            json!({
                "request_id":Uuid::now_v7(),"command":set(1,id.clone())
            })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let input = json!({"request_id":Uuid::now_v7(),"command":set(1,id.clone())});
    let save = async || {
        h.write(Method::POST, &path, &alice, input.clone())
            .await
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let result = save().await;
    assert_eq!(
        result,
        json!({"outcome":"applied","project":project,"version":2})
    );
    let current = h.json(&detail, &alice).await;
    assert_eq!(&current["default_repository"], id);
    assert_eq!(&current["repositories"][0]["id"], id);
    assert_eq!(
        h.json(&scoped, &alice).await["items"][0]["id"],
        json!(project)
    );

    for path in [
        detail.clone(),
        format!("{detail}/history"),
        format!("{root}/projects"),
    ] {
        let hidden = h.json(&path, &bob).await;
        assert!(
            !hidden.to_string().contains(id.as_str().unwrap()),
            "{hidden}"
        );
    }
    let hidden = h.json(&detail, &bob).await;
    assert_eq!(hidden["default_repository_configured"], true);
    assert!(hidden["default_repository"].is_null());
    let unlink = |version| {
        json!({
            "kind":"repository","project":project,"version":version,"repository":id,"linked":false
        })
    };
    assert_eq!(
        project_command(&h, &root, &alice, unlink(2)).await["outcome"],
        "rejected"
    );
    repository_write(
        &h, &alice, &repo, Method::PUT, &format!("{}/settings/archive", repository_path(&repo)),
        json!({"expected_version":0,"archived":true,"repository":format!("{}/delivery", repo["owner"].as_str().unwrap())}),
    ).await;
    let archived = h.json(&detail, &alice).await;
    assert!(archived["default_repository"].is_null());
    assert_eq!(archived["repositories"][0]["can_create_issue"], false);
    assert_eq!(
        project_command(&h, &root, &alice, set(2, id.clone())).await["outcome"],
        "rejected"
    );
    assert_eq!(save().await, result);
    assert_eq!(
        project_command(&h, &root, &alice, set(2, Value::Null)).await["outcome"],
        "applied"
    );
    assert_eq!(
        h.json(&detail, &alice).await["default_repository_configured"],
        false
    );
    assert_eq!(
        project_command(&h, &root, &alice, unlink(3)).await["outcome"],
        "applied"
    );
    assert_eq!(h.json(&scoped, &alice).await["items"], json!([]));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn native_browser_creates_project_issues_in_the_default_and_explicit_repositories() {
    use crate::test_support::http::browser::Browser;

    let h = Harness::new(false).await;
    let (organization, _, _) = bootstrap(&h).await;
    let alice = h.login().await;
    let root = format!("/api/organizations/{organization}");
    let first = create_repository(&h, &root, &alice, "planning").await;
    let default = create_repository(&h, &root, &alice, "delivery").await;
    let (project, _) = create(&h, &root, &alice).await;
    assert_eq!(project_command(&h, &root, &alice, json!({
        "kind":"repository","project":project,"version":1,"repository":first["id"],"linked":true
    })).await["outcome"], "applied");

    let browser = Browser::new("project-default");
    browser.run(&["set", "viewport", "1280", "900"]).await;
    browser
        .run(&[
            "open",
            &format!(
                "{}/-/organizations/{organization}/projects/{project}?view=overview",
                h.origin
            ),
        ])
        .await;
    browser.sign_in().await;
    let manage = "[aria-label='Linked repositories'] header button:not(:disabled)";
    browser.run(&["wait", manage]).await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Manage", "--exact",
        ])
        .await;
    browser.run(&["wait", "[role=dialog]"]).await;
    browser
        .choose_select(
            "[aria-label='Default repository']",
            default["id"].as_str().unwrap(),
        )
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Save default repository",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--fn", r#"document.querySelector('[aria-label="Default repository"]')?.textContent.includes('/delivery') && document.querySelector('[aria-label="Linked repositories"]')?.textContent.includes('Default')"#]).await;
    browser
        .run(&[
            "wait",
            "--fn",
            "Array.from(document.querySelectorAll('[role=dialog] button')).some(button => button.textContent.trim() === 'Done' && !button.disabled)",
        ])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Done", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;

    browser
        .run(&["find", "role", "tab", "click", "--name", "Board", "--exact"])
        .await;
    browser.run(&["wait", "[aria-label='Project board']"]).await;
    for (repo, title) in [
        (&default, "Routed by the project default"),
        (&first, "Routed by an explicit choice"),
    ] {
        let add_work = ".workspace-project-header > div:last-child > button:last-child";
        browser
            .run(&[
                "wait",
                "--fn",
                &format!("document.querySelector('{add_work}')?.textContent.trim() === 'Add work'"),
            ])
            .await;
        browser.run(&["click", add_work]).await;
        browser
            .run(&[
                "find",
                "role",
                "tab",
                "click",
                "--name",
                "New issue",
                "--exact",
            ])
            .await;
        browser.run(&["wait", "--fn", r#"document.querySelector('[aria-label="Issue repository"]')?.textContent.includes('/delivery')"#]).await;
        if repo["id"] != default["id"] {
            browser
                .choose_select(
                    "[aria-label='Issue repository']",
                    repo["id"].as_str().unwrap(),
                )
                .await;
        }
        browser
            .run(&["find", "label", "Title", "fill", title, "--exact"])
            .await;
        browser
            .run(&[
                "find",
                "role",
                "button",
                "click",
                "--name",
                "Create issue",
                "--exact",
            ])
            .await;
        browser.run(&["wait", "--text", title]).await;
        browser
            .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
            .await;
        assert_eq!(
            h.json(&format!("{}/issues/1", repository_path(repo)), &alice)
                .await["title"],
            title
        );
    }
    drop(browser);
    h.close().await;
}
