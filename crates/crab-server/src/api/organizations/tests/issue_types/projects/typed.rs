use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn typed_project_creation_commits_once_and_replays_after_type_is_disabled() {
    let (h, alice, org, repo, key) = fixture().await;
    let root = format!("/api/repos/{key}");
    let projects = format!("{root}/projects");
    let project = project_with_issue(&h, &alice, &projects, repo, false, false).await;
    let schema = h.json(&format!("{org}/issue-types"), &alice).await;
    let feature = schema["definitions"][2]["id"].as_str().unwrap();
    let bug = schema["definitions"][1]["id"].as_str().unwrap();
    let request_id = Uuid::now_v7();
    let title = "Plan authenticated releases";
    // Standalone issue creation and Project creation use independent submission scopes.
    repository_write(
        &h,
        Method::POST,
        &format!("{root}/issues"),
        &alice,
        repo,
        json!({"request_id":request_id,"title":title,"body":""}),
    )
    .await
    .error_for_status()
    .unwrap();

    let request = |id: Uuid, type_id: &str, parent: Option<u64>| {
        json!({"request_id":id,"command":{
            "kind":"create_issue","project":project,"version":0,"parent":parent,"type_id":type_id,
            "item":{"id":Uuid::now_v7(),"subject":null,"title":title,"body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}}
        }})
    };
    let submit = async |action: &str, body: Value| -> Value {
        repository_write(
            &h,
            Method::POST,
            &format!("{projects}/{action}"),
            &alice,
            repo,
            body,
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
    };
    let first = request(request_id, feature, None);
    let published = submit("commands", first.clone()).await;
    assert_eq!(published["outcome"], "applied", "{published}");
    assert!(h.json(&format!("{root}/issues/2"), &alice).await["issue_type"].is_null());
    let issue = h.json(&format!("{root}/issues/3"), &alice).await;
    assert_eq!(issue["issue_type"]["id"], feature);
    assert_eq!(issue["issue_type_version"], 1);
    let items = format!("{projects}/{project}/items");
    let grouped = h
        .json(&format!("{items}?group_by=issue_type"), &alice)
        .await;
    assert_eq!(
        grouped["page"]["group_counts"],
        json!({(feature):1,"none":1})
    );
    assert_eq!(grouped["page"]["items"][1]["work"]["subject"]["number"], 3);

    let mut definitions = schema["definitions"].clone();
    definitions[2]["enabled"] = json!(false);
    h.write(
        Method::PUT,
        &format!("{org}/issue-types"),
        &alice,
        json!({"expected_version":schema["version"],"definitions":definitions}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(submit("commands", first).await, published);
    assert_eq!(
        submit("commands", request(Uuid::now_v7(), feature, None)).await["outcome"],
        "rejected"
    );
    assert_eq!(
        submit("commands", request(Uuid::now_v7(), bug, Some(9_999))).await["outcome"],
        "rejected"
    );
    let cancelled = request(Uuid::now_v7(), bug, None);
    assert_eq!(
        submit("cancel", cancelled.clone()).await["outcome"],
        "cancelled"
    );
    assert_eq!(submit("commands", cancelled).await["outcome"], "cancelled");
    assert_eq!(h.json(&items, &alice).await["page"]["total"], 2);
    assert_eq!(
        h.http
            .get(format!("{}{root}/issues/4", h.origin))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and built embedded UI; uses real OIDC/native Cells"]
async fn native_browser_creates_issues_in_repository_project_type_groups() {
    let (h, alice, _, repo, key) = fixture().await;
    let repository = format!("/api/repos/{key}");
    let projects = format!("{repository}/projects");
    let project = project_with_issue(&h, &alice, &projects, repo, false, false).await;
    let browser = Browser::new("typed-project-creation");
    let url = format!("{}/{key}/projects/{project}?group_by=issue_type", h.origin);
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["open", &url]).await;
    browser.sign_in().await;
    browser
        .run(&["wait", ".project-table tbody[aria-label='Feature']"])
        .await;
    click("button", "Create issue in Feature").await;
    browser
        .run(&["wait", "[role=dialog] input[id$='-title']"])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Title",
            "fill",
            "Plan typed release",
            "--exact",
        ])
        .await;
    browser.run(&["press", "Escape"]).await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    browser.run(&["open", &url]).await;
    browser
        .run(&["wait", ".project-table tbody[aria-label='Feature']"])
        .await;
    click("button", "Resume item draft").await;
    browser.run(&["wait", "--fn", "document.querySelector('[role=dialog] [aria-label=\"Issue type\"]')?.textContent.includes('Feature') && document.querySelector('[role=dialog] input[id$=\"-title\"]')?.value === 'Plan typed release'"]).await;
    click("button", "Add to project").await;
    browser.run(&["wait", "--fn", "document.querySelector('.project-table tbody[aria-label=\"Feature\"]')?.textContent.includes('Plan typed release')"]).await;
    let issue = h.json(&format!("{repository}/issues/2"), &alice).await;
    assert_eq!(issue["issue_type"]["name"], "Feature");

    click("tab", "Board").await;
    browser.run(&["wait", ".project-board"]).await;
    click("combobox", "Group board by").await;
    click("option", "Issue type").await;
    browser
        .run(&["wait", ".project-board-column[aria-label='No type']"])
        .await;
    click("button", "Create issue in No type").await;
    browser
        .run(&["wait", "[role=dialog] input[id$='-title']"])
        .await;
    browser
        .run(&[
            "find",
            "label",
            "Title",
            "fill",
            "Uncategorized release note",
            "--exact",
        ])
        .await;
    click("button", "Add to project").await;
    browser.run(&["wait", "--fn", "document.querySelector('.project-board-column[aria-label=\"No type\"]')?.textContent.includes('Uncategorized release note')"]).await;
    assert!(h.json(&format!("{repository}/issues/3"), &alice).await["issue_type"].is_null());
}
