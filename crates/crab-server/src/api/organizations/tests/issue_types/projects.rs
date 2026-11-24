use super::*;
use crate::test_support::http::browser::Browser;
mod groups;

async fn project_with_issue(
    h: &Harness,
    alice: &str,
    root: &str,
    repository: Uuid,
    organization_project: bool,
    show_type: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    let submit = async |command: Value| {
        let path = format!("{root}/commands");
        let input = json!({"request_id":Uuid::now_v7(),"command":command});
        let response = if organization_project {
            h.write(Method::POST, &path, alice, input).await
        } else {
            repository_write(h, Method::POST, &path, alice, repository, input).await
        };
        let result: Value = response.error_for_status().unwrap().json().await.unwrap();
        assert_eq!(result["outcome"], "applied", "{result}");
    };
    submit(json!({"kind":"create","id":id,"name":"Release planning","template":"table"})).await;
    if organization_project {
        submit(json!({"kind":"repository","project":id,"version":1,"repository":repository,"linked":true})).await;
    }
    if show_type {
        let detail = h.json(&format!("{root}/{id}"), alice).await;
        let mut definition = detail["project"]["definition"].clone();
        definition["views"][0]["show_issue_type"] = json!(true);
        submit(json!({"kind":"edit","project":id,"version":detail["project"]["version"],"definition":definition,"archived":false})).await;
    }
    let mut command = json!({"kind":"item","project":id,"version":0,"item":{
        "id":Uuid::now_v7(),"subject":{"kind":"issue","number":1},"title":"","body":"",
        "status":"Todo","priority":0,"start":null,"end":null,"values":{}
    }});
    if organization_project {
        command["repository"] = json!(repository);
    }
    submit(command).await;
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn project_issue_type_fields_follow_source_permissions_and_assignment() {
    use crate::cells::{
        Address,
        organization::{ChangeOutcome, DecideInvitation, Decision, Invite, InviteInput, Role},
    };

    let (h, alice, org, repo, key) = fixture().await;
    let root = format!("/api/repos/{key}");
    let repositories = format!("{root}/projects");
    let workspace = format!("{org}/projects");
    let local = project_with_issue(&h, &alice, &repositories, repo, false, true).await;
    let shared = project_with_issue(&h, &alice, &workspace, repo, true, true).await;
    let local_items = format!("{repositories}/{local}/items");
    let shared_items = format!("{workspace}/{shared}/items");
    let issue = format!("{root}/issues/1");
    let schema = format!("{org}/issue-types");
    let initial = h.json(&schema, &alice).await;
    let mut types = initial["definitions"].as_array().unwrap().to_vec();
    repository_write(
        &h,
        Method::PUT,
        &format!("{issue}/type"),
        &alice,
        repo,
        json!({"expected_version":0,"type_id":types[1]["id"]}),
    )
    .await
    .error_for_status()
    .unwrap();
    for path in [&local_items, &shared_items] {
        let page = h.json(path, &alice).await;
        assert_eq!(page["page"]["items"][0]["issue_type"]["name"], "Bug");
        assert_eq!(page["page"]["items"][0]["issue_type_version"], 1);
        assert_eq!(page["page"]["issue_types"].as_array().unwrap().len(), 3);
    }

    let organization = org.rsplit('/').next().unwrap().parse().unwrap();
    let owner = h.json(&org, &alice).await["actor"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let cells = h.server.cells().unwrap();
    let bob = cells
        .command::<RegisterAccounts>(
            Address::Directory,
            app::mutation_identity().unwrap(),
            vec![IdentityKey {
                issuer: h.provider.issuer.clone(),
                subject: "bob-id".into(),
            }],
        )
        .await
        .unwrap()
        .output[0];
    let offer = cells
        .command::<Invite>(
            Address::Organization(organization),
            app::mutation_identity().unwrap(),
            InviteInput {
                actor: owner,
                recipient: bob,
                version: 0,
                role: Role::Member,
            },
        )
        .await
        .unwrap()
        .output;
    let ChangeOutcome::Applied(offer) = offer else {
        panic!("Invitation was rejected")
    };
    cells
        .command::<DecideInvitation>(
            Address::Organization(organization),
            app::mutation_identity().unwrap(),
            Decision {
                actor: bob,
                version: offer.version,
                accept: true,
            },
        )
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let reader = h.login().await;
    let restricted = h.json(&shared_items, &reader).await;
    assert_eq!(restricted["page"]["items"][0]["restricted"], true);
    assert_eq!(restricted["page"]["items"][0]["issue_type"], Value::Null);
    assert_eq!(restricted["page"]["issue_types"], json!([]));
    let filter = format!(
        "{shared_items}?issue_type={}",
        types[1]["id"].as_str().unwrap()
    );
    let hidden_matches = h.json(&filter, &reader).await;
    assert_eq!(hidden_matches["page"]["total"], 0);
    let hidden_groups = h
        .json(&format!("{shared_items}?group_by=issue_type"), &reader)
        .await;
    assert_eq!(hidden_groups["page"]["group_counts"], json!({"other":1}));
    assert_eq!(
        hidden_groups["page"]["items"][0]["issue_type_group"],
        "other"
    );
    let members = format!("{root}/members");
    for (access, version) in [("read", Value::Null), ("triage", json!(1))] {
        let acl = h.json(&members, &alice).await;
        repository_write(
            &h, Method::PUT, &members, &alice, repo,
            json!({"expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":access}],"groups":[]}),
        ).await.error_for_status().unwrap();
        let visible = h.json(&shared_items, &reader).await;
        assert_eq!(visible["can_manage"], false);
        assert_eq!(visible["page"]["items"][0]["issue_type"]["name"], "Bug");
        assert_eq!(visible["page"]["items"][0]["issue_type_version"], version);
        assert_eq!(h.json(&filter, &reader).await["page"]["total"], 1);
        let grouped = h
            .json(&format!("{shared_items}?group_by=issue_type"), &reader)
            .await;
        let type_id = types[1]["id"].as_str().unwrap();
        assert_eq!(grouped["page"]["group_counts"][type_id], 1);
        assert_eq!(grouped["page"]["items"][0]["issue_type_group"], type_id);
    }
    types[1]["name"] = json!("Incident");
    h.write(
        Method::PUT,
        &schema,
        &alice,
        json!({"expected_version":0,"definitions":types}),
    )
    .await
    .error_for_status()
    .unwrap();
    for path in [&local_items, &shared_items] {
        let page = h.json(path, &alice).await;
        assert_eq!(page["page"]["items"][0]["issue_type"]["name"], "Incident");
        assert_eq!(page["page"]["items"][0]["issue_type_version"], 1);
    }
    repository_write(
        &h,
        Method::PUT,
        &format!("{root}/settings/archive"),
        &alice,
        repo,
        json!({"expected_version":0,"archived":true,"repository":key}),
    )
    .await
    .error_for_status()
    .unwrap();
    for path in [&local_items, &shared_items] {
        let page = h.json(path, &alice).await;
        assert_eq!(page["page"]["items"][0]["issue_type"]["name"], "Incident");
        assert_eq!(page["page"]["items"][0]["issue_type_version"], Value::Null);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and built embedded UI; uses real OIDC/native Cells"]
async fn native_browser_edits_project_issue_type_field() {
    let (h, alice, _, repo, key) = fixture().await;
    let id = project_with_issue(
        &h,
        &alice,
        &format!("/api/repos/{key}/projects"),
        repo,
        false,
        false,
    )
    .await;
    let browser = Browser::new("project-types");
    let project = format!("{}/{key}/projects/{id}", h.origin);
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["open", &project]).await;
    browser.sign_in().await;
    browser.run(&["wait", "table.project-table"]).await;
    click("button", "Settings").await;
    browser
        .run(&["wait", "--text", "Save project settings"])
        .await;
    click("tab", "Views").await;
    click("checkbox", "Show issue type in Table").await;
    click("button", "Save project settings").await;
    browser.run(&["wait", "--fn", "document.querySelector('table.project-table button[aria-label^=\"Change issue type for\"]') && !document.querySelector('[role=dialog]')"]).await;
    click("button", "Change issue type for Classify this report").await;
    click("combobox", "Issue type").await;
    click("option", "Bug").await;
    click("button", "Save issue type").await;
    browser.run(&["wait", "--fn", "document.querySelector('table.project-table button[aria-label^=\"Change issue type for\"]')?.textContent.includes('Bug')"]).await;
    click("combobox", "Filter by issue type").await;
    click("option", "No type").await;
    browser
        .run(&["wait", "--text", "No work matches this view"])
        .await;
    click("combobox", "Filter by issue type").await;
    click("option", "Bug").await;
    browser.run(&["wait", "table.project-table"]).await;
    click("button", "Settings").await;
    click("tab", "Views").await;
    click("combobox", "Issue type filter for Table").await;
    click("option", "Bug").await;
    click("button", "Save project settings").await;
    browser
        .run(&["wait", "--fn", "!document.querySelector('[role=dialog]')"])
        .await;
    browser.run(&["open", &project]).await;
    browser.run(&["wait", "--fn", "document.querySelector('[aria-label=\"Filter by issue type\"]')?.textContent.includes('Bug')"]).await;
    click("link", "Classify this report").await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.querySelector('.issue-meta')?.textContent.includes('Bug')",
        ])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn project_type_filter_counts_matching_rows_across_repositories_and_pages() {
    let (h, alice, org, first, key) = fixture().await;
    let first_root = format!("/api/repos/{key}");
    let workspace = format!("{org}/projects");
    let repository_projects = format!("{first_root}/projects");
    let shared = project_with_issue(&h, &alice, &workspace, first, true, true).await;
    let local = project_with_issue(&h, &alice, &repository_projects, first, false, true).await;
    let second: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"hardware","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let second_id = second["id"].as_str().unwrap().parse().unwrap();
    let second_root = format!("/api/repos/{}/hardware", second["owner"].as_str().unwrap());
    let submit = async |command| {
        let reply: Value = h
            .write(
                Method::POST,
                &format!("{workspace}/commands"),
                &alice,
                json!({"request_id":Uuid::now_v7(),"command":command}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reply["outcome"], "applied", "{reply}");
    };
    let current = h.json(&format!("{workspace}/{shared}"), &alice).await;
    submit(json!({"kind":"repository","project":shared,"version":current["project"]["version"],"repository":second_id,"linked":true})).await;
    let schema_path = format!("{org}/issue-types");
    let schema = h.json(&schema_path, &alice).await;
    let definition = &schema["definitions"][1];
    let bug = definition["id"].as_str().unwrap();
    for (root, repo, total) in [(&first_root, first, 52), (&second_root, second_id, 1)] {
        for number in 1..=total {
            if repo != first || number != 1 {
                repository_write(
                    &h, Method::POST, &format!("{root}/issues"), &alice, repo,
                    json!({"request_id":Uuid::now_v7(),"title":format!("Filter proof {number}"),"body":""}),
                ).await.error_for_status().unwrap();
            }
            if repo == first && number == 2 {
                let result: Value = repository_write(
                    &h, Method::POST, &format!("{repository_projects}/commands"), &alice, repo,
                    json!({"request_id":Uuid::now_v7(),"command":{
                        "kind":"item","project":local,"version":0,"item":{"id":Uuid::now_v7(),"subject":{"kind":"issue","number":number},
                        "title":"","body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}}
                    }}),
                ).await.error_for_status().unwrap().json().await.unwrap();
                assert_eq!(result["outcome"], "applied");
            } else {
                repository_write(
                    &h,
                    Method::PUT,
                    &format!("{root}/issues/{number}/type"),
                    &alice,
                    repo,
                    json!({"expected_version":0,"type_id":bug}),
                )
                .await
                .error_for_status()
                .unwrap();
            }
            if repo != first || number != 1 {
                submit(json!({"kind":"item","project":shared,"version":0,"repository":repo,"item":{
                    "id":Uuid::now_v7(),"subject":{"kind":"issue","number":number},"title":"","body":"",
                    "status":if repo==second_id {"Done"} else {"Todo"},"priority":0,"start":null,"end":null,"values":{}
                }})).await;
            }
        }
    }
    let shared_items = format!("{workspace}/{shared}/items");
    let filtered = format!("{shared_items}?issue_type={bug}");
    let first_page = h.json(&filtered, &alice).await;
    let page = &first_page["page"];
    assert_eq!(page["total"], 52);
    assert_eq!(page["counts"]["Todo"], 51);
    assert_eq!(page["counts"]["Done"], 1);
    assert_eq!(page["items"].as_array().unwrap().len(), 50);
    let grouped = h
        .json(&format!("{shared_items}?group_by=issue_type"), &alice)
        .await;
    assert_eq!(grouped["page"]["group_counts"][bug], 52);
    assert_eq!(grouped["page"]["group_counts"]["none"], 1);
    assert_eq!(grouped["page"]["items"].as_array().unwrap().len(), 50);
    let done = h
        .json(
            &format!("{filtered}&group_by=issue_type&status=Done"),
            &alice,
        )
        .await;
    assert_eq!(done["page"]["group_counts"][bug], 1);
    assert_eq!(done["page"]["total"], 1);
    let continuation = format!(
        "{filtered}&after={}&version={}&type_cursor={}",
        page["next"].as_str().unwrap(),
        page["revision"],
        page["type_cursor"].as_str().unwrap()
    );
    let second_page = h.json(&continuation, &alice).await;
    assert_eq!(second_page["page"]["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        second_page["page"]["items"][1]["repository"]["id"],
        json!(second_id)
    );
    assert_eq!(
        second_page["page"]["items"][1]["work"]["subject"]["number"],
        1
    );
    assert_eq!(
        h.json(
            &format!("{filtered}&repository={second_id}&status=Done"),
            &alice
        )
        .await["page"]["total"],
        1
    );
    for path in [
        &shared_items,
        &format!("{repository_projects}/{local}/items"),
    ] {
        let untyped = h.json(&format!("{path}?issue_type=none"), &alice).await;
        assert_eq!(untyped["page"]["total"], 1);
        assert_eq!(untyped["page"]["items"][0]["work"]["subject"]["number"], 2);
    }
    assert_eq!(
        h.json(
            &format!("{repository_projects}/{local}/items?issue_type={bug}"),
            &alice
        )
        .await["page"]["total"],
        1
    );

    repository_write(
        &h,
        Method::PUT,
        &format!("{second_root}/issues/1/type"),
        &alice,
        second_id,
        json!({"expected_version":1,"type_id":null}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.http
            .get(format!("{}{continuation}", h.origin))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(h.json(&filtered, &alice).await["page"]["total"], 51);
    let draft: Value = repository_write(
        &h, Method::POST, &format!("{repository_projects}/commands"), &alice, first,
        json!({"request_id":Uuid::now_v7(),"command":{
            "kind":"item","project":local,"version":0,"item":{"id":Uuid::now_v7(),"subject":null,
            "title":"A draft","body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}}
        }}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(draft["outcome"], "applied");
    let mut definitions = schema["definitions"].as_array().unwrap().to_vec();
    definitions[1]["enabled"] = json!(false);
    h.write(
        Method::PUT,
        &schema_path,
        &alice,
        json!({"expected_version":0,"definitions":definitions}),
    )
    .await
    .error_for_status()
    .unwrap();
    let missing = h
        .json(
            &format!("{repository_projects}/{local}/items?issue_type=none"),
            &alice,
        )
        .await;
    assert_eq!(missing["page"]["total"], 2);
    assert!(
        missing["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["work"]["subject"]["kind"] == "issue")
    );
    let grouped_local = h
        .json(
            &format!("{repository_projects}/{local}/items?group_by=issue_type"),
            &alice,
        )
        .await;
    assert_eq!(
        grouped_local["page"]["group_counts"],
        json!({"none":2,"other":1})
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and built embedded UI; uses real OIDC/native Cells"]
async fn native_browser_moves_issues_between_type_columns() {
    let (h, alice, _, repo, key) = fixture().await;
    let id = project_with_issue(
        &h,
        &alice,
        &format!("/api/repos/{key}/projects"),
        repo,
        false,
        false,
    )
    .await;
    let browser = Browser::new("project-type-columns");
    let project = format!("{}/{key}/projects/{id}", h.origin);
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["open", &project]).await;
    browser.sign_in().await;
    browser.run(&["wait", "table.project-table"]).await;
    click("tab", "Board").await;
    browser.run(&["wait", ".project-board"]).await;
    click("button", "Settings").await;
    browser
        .run(&["wait", "--text", "Save project settings"])
        .await;
    click("tab", "Views").await;
    click("combobox", "Board columns for Board").await;
    click("option", "Issue type").await;
    click("button", "Save project settings").await;
    browser
        .run(&[
            "wait",
            "--fn",
            r#"!document.querySelector('[role=dialog]') && !!document.querySelector('.project-board-column[aria-label="No type"] article[draggable=true]')"#,
        ])
        .await;
    drag_between_groups(
        &browser,
        ".project-board-column[aria-label='No type'] article",
        ".project-board-column[aria-label='Feature'] > header",
    )
    .await;
    browser
        .run(&[
            "wait",
            ".project-board-column[aria-label='Feature'] article",
        ])
        .await;
    assert_eq!(
        h.json(&format!("/api/repos/{key}/issues/1"), &alice).await["issue_type"]["name"],
        "Feature"
    );
    browser.run(&["open", &project]).await;
    browser.run(&["wait", "table.project-table"]).await;
    click("tab", "Board").await;
    browser
        .run(&[
            "wait",
            ".project-board-column[aria-label='Feature'] article",
        ])
        .await;
    click("button", "Change issue type for Classify this report").await;
    click("combobox", "Issue type").await;
    click("option", "No type").await;
    click("button", "Save issue type").await;
    browser
        .run(&[
            "wait",
            ".project-board-column[aria-label='No type'] article",
        ])
        .await;
}

async fn drag_between_groups(browser: &Browser, source: &str, target: &str) {
    let selectors = serde_json::to_string(&[source, target]).unwrap();
    let expression = format!(
        r#"{selectors}.map(selector => {{
            const element = document.querySelector(selector);
            const {{ left, top, height }} = element.getBoundingClientRect();
            const x = Math.round(left + 24), y = Math.round(top + height / 2);
            if (!element.contains(document.elementFromPoint(x, y))) {{
                throw new Error("Drag point is outside its element: " + selector);
            }}
            return [x, y];
        }})"#,
    );
    let points = browser.run(&["eval", &expression]).await;
    let [[source_x, source_y], [target_x, target_y]] =
        serde_json::from_str::<[[i32; 2]; 2]>(&points).unwrap();
    let move_to = async |x: i32, y: i32| {
        browser
            .run(&["mouse", "move", &x.to_string(), &y.to_string()])
            .await;
    };
    move_to(source_x, source_y).await;
    browser.run(&["mouse", "down"]).await;
    move_to(source_x + 16, source_y).await;
    move_to(target_x, target_y).await;
    // The browser emits dragenter on arrival and needs another move for dragover.
    move_to(target_x + 1, target_y).await;
    browser.run(&["mouse", "up"]).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and built embedded UI; uses real OIDC/native Cells"]
async fn native_browser_groups_project_tables_and_roadmaps_by_issue_type() {
    let (h, alice, org, repo, key) = fixture().await;
    let root = format!("{org}/projects");
    let id = project_with_issue(&h, &alice, &root, repo, true, false).await;
    let items = format!("{root}/{id}/items");
    let row = h.json(&items, &alice).await["page"]["items"][0].clone();
    let mut scheduled = row["item"].clone();
    scheduled["start"] = json!("2026-01-15");
    scheduled["end"] = json!("2026-02-01");
    let saved: Value = h
        .write(
            Method::POST,
            &format!("{root}/commands"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"command":{
                "kind":"item","project":id,"version":row["version"],"repository":repo,"item":scheduled
            }}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["outcome"], "applied", "{saved}");

    let browser = Browser::new("project-type-groups");
    let organization = org.rsplit('/').next().unwrap();
    let project = format!("{}/-/organizations/{organization}/projects/{id}", h.origin);
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["open", &project]).await;
    browser.sign_in().await;
    browser.run(&["set", "viewport", "1280", "900"]).await;
    browser.run(&["wait", "table.project-table"]).await;
    click("button", "Settings").await;
    browser
        .run(&["wait", "--text", "Save project settings"])
        .await;
    click("tab", "Views").await;
    for name in ["Group by for Table", "Group by for Roadmap"] {
        click("combobox", name).await;
        click("option", "Issue type").await;
    }
    click("button", "Save project settings").await;
    browser
        .run(&[
            "wait",
            "--fn",
            r#"!document.querySelector('[role=dialog]') && !!document.querySelector('.project-table tbody[aria-label="No type"] tr[draggable=true]')"#,
        ])
        .await;
    drag_between_groups(
        &browser,
        ".project-table tbody[aria-label='No type'] tr[draggable=true]",
        ".project-table tbody[aria-label='Feature'] th",
    )
    .await;
    browser
        .run(&[
            "wait",
            ".project-table tbody[aria-label='Feature'] tr[draggable=true]",
        ])
        .await;
    let issue = format!("/api/repos/{key}/issues/1");
    assert_eq!(
        h.json(&issue, &alice).await["issue_type"]["name"],
        "Feature"
    );

    browser.run(&["open", &project]).await;
    browser
        .run(&[
            "wait",
            ".project-table tbody[aria-label='Feature'] tr[draggable=true]",
        ])
        .await;
    click("tab", "Roadmap").await;
    browser
        .run(&[
            "wait",
            ".project-roadmap [role=rowgroup][aria-label='Feature'] .project-roadmap-row",
        ])
        .await;
    click("button", "Change issue type for Classify this report").await;
    click("combobox", "Issue type").await;
    click("option", "No type").await;
    click("button", "Save issue type").await;
    browser
        .run(&[
            "wait",
            ".project-roadmap [role=rowgroup][aria-label='No type'] .project-roadmap-row",
        ])
        .await;
    click("combobox", "Group by").await;
    click("option", "No grouping").await;
    browser.run(&["wait", "--fn", "!document.querySelector('.project-roadmap [role=rowgroup]') && !!document.querySelector('.project-roadmap-row')"]).await;
    assert!(h.json(&issue, &alice).await["issue_type"].is_null());
    let after = h.json(&items, &alice).await;
    assert_eq!(after["page"]["items"][0]["item"], scheduled);
}
