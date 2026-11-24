use super::*;

async fn define_group_fields(
    h: &Harness,
    alice: &str,
    root: &str,
    project: Uuid,
    repository: Option<Uuid>,
) -> (Uuid, Uuid) {
    let detail = h.json(&format!("{root}/{project}"), alice).await;
    let mut definition = detail["project"]["definition"].clone();
    let team = Uuid::now_v7();
    let stream = Uuid::now_v7();
    definition["fields"].as_array_mut().unwrap().extend([
        json!({"id":team,"name":"Team","definition":{"kind":"select","options":["Core","Edge"]}}),
        json!({"id":stream,"name":"Stream","definition":{"kind":"text"}}),
    ]);
    submit(h, alice, root, repository, json!({
        "kind":"edit","project":project,"version":detail["project"]["version"],"definition":definition,"archived":false
    })).await;
    (team, stream)
}

async fn submit(h: &Harness, alice: &str, root: &str, repository: Option<Uuid>, command: Value) {
    let path = format!("{root}/commands");
    let input = json!({"request_id":Uuid::now_v7(),"command":command});
    let response = match repository {
        Some(repo) => repository_write(h, Method::POST, &path, alice, repo, input).await,
        None => h.write(Method::POST, &path, alice, input).await,
    };
    let reply: Value = response.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(reply["outcome"], "applied", "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn field_groups_count_only_matching_work_across_pages() {
    let (h, alice, org, repo, key) = fixture().await;
    for (root, repository, revision) in [
        (format!("{org}/projects"), None, "version"),
        (format!("/api/repos/{key}/projects"), Some(repo), "revision"),
    ] {
        let project =
            project_with_issue(&h, &alice, &root, repo, repository.is_none(), false).await;
        let (team, stream) = define_group_fields(&h, &alice, &root, project, repository).await;
        for n in 1..=52 {
            let value = if n == 52 { "Edge" } else { "Core" };
            let mut command = json!({"kind":"item","project":project,"version":0,"item":{
                "id":Uuid::now_v7(),"subject":null,"title":format!("Work {n}"),"body":"",
                "status":if n == 52 {"Done"} else {"Todo"},"priority":0,"start":null,"end":null,
                "values":{(team.to_string()):value,(stream.to_string()):value}
            }});
            if repository.is_none() {
                command["repository"] = Value::Null;
            }
            submit(&h, &alice, &root, repository, command).await;
        }
        let items = format!("{root}/{project}/items");
        let selection = h
            .json(&format!("{items}?group_by=field:{team}"), &alice)
            .await;
        assert_eq!(
            selection["page"]["group_counts"],
            json!({"":1,"Core":51,"Edge":1})
        );
        let query = format!("{items}?group_by=field:{stream}");
        let first = h.json(&query, &alice).await;
        let page = &first["page"];
        assert_eq!(page["group_counts"], json!({"":1,"Core":51}));
        let second = h
            .json(
                &format!(
                    "{query}&after={}&{revision}={}",
                    page["next"].as_str().unwrap(),
                    page["revision"]
                ),
                &alice,
            )
            .await;
        assert_eq!(second["page"]["group_counts"], json!({"Core":51,"Edge":1}));
        let done = h
            .json(
                &format!("{items}?group_by=field:{team}&status=Done"),
                &alice,
            )
            .await;
        assert_eq!(done["page"]["group_counts"], json!({"Edge":1}));
        let status = h
            .json(&format!("{items}?group_by=status&status=Done"), &alice)
            .await;
        assert_eq!(status["page"]["group_counts"], json!({"Done":1}));
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and built embedded UI; uses real OIDC/native Cells"]
async fn native_browser_moves_work_between_field_groups() {
    let (h, alice, org, repo, _) = fixture().await;
    let root = format!("{org}/projects");
    let id = project_with_issue(&h, &alice, &root, repo, true, false).await;
    let (team, _) = define_group_fields(&h, &alice, &root, id, None).await;
    let browser = Browser::new("project-field-groups");
    let organization = org.rsplit('/').next().unwrap();
    let project = format!("{}/-/organizations/{organization}/projects/{id}", h.origin);
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["open", &project]).await;
    browser.sign_in().await;
    browser.run(&["set", "viewport", "1440", "1000"]).await;
    browser.run(&["wait", "table.project-table"]).await;
    click("button", "Settings").await;
    click("tab", "Views").await;
    click("combobox", "Group by for Table").await;
    click("option", "Workflow status").await;
    click("combobox", "Board columns for Board").await;
    click("option", "Team").await;
    click("button", "Save project settings").await;
    browser.run(&["wait", "--fn", r#"!document.querySelector('[role=dialog]') && !!document.querySelector('.project-table tbody[aria-label="Todo"] tr[draggable=true]')"#]).await;
    drag_between_groups(
        &browser,
        ".project-table tbody[aria-label='Todo'] tr[draggable=true]",
        ".project-table tbody[aria-label='Done'] th",
    )
    .await;
    browser
        .run(&[
            "wait",
            ".project-table tbody[aria-label='Done'] tr[draggable=true]",
        ])
        .await;
    click("tab", "Board").await;
    browser
        .run(&[
            "wait",
            ".project-board-column[aria-label='No value'] article[draggable=true]",
        ])
        .await;
    drag_between_groups(
        &browser,
        ".project-board-column[aria-label='No value'] article[draggable=true]",
        ".project-board-column[aria-label='Core'] > header",
    )
    .await;
    browser
        .run(&["wait", ".project-board-column[aria-label='Core'] article"])
        .await;
    let items = h.json(&format!("{root}/{id}/items"), &alice).await;
    let item = &items["page"]["items"][0]["item"];
    assert_eq!(item["status"], "Done");
    assert_eq!(item["values"][team.to_string()], "Core");
    browser.run(&["open", &project]).await;
    browser
        .run(&[
            "wait",
            ".project-table tbody[aria-label='Done'] tr[draggable=true]",
        ])
        .await;
    click("tab", "Board").await;
    browser
        .run(&["wait", ".project-board-column[aria-label='Core'] article"])
        .await;
}
