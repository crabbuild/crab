use super::{repositories::repository_write, *};

async fn fixture() -> (Harness, String, String, Uuid, String) {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let created: Value = h
        .write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":"work","description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let repo = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let key = format!("{}/work", created["owner"].as_str().unwrap());
    repository_write(
        &h,
        Method::POST,
        &format!("/api/repos/{key}/issues"),
        &alice,
        repo,
        json!({"request_id":Uuid::now_v7(),"title":"Classify this report","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    (h, alice, org, repo, key)
}

#[tokio::test(flavor = "multi_thread")]
async fn issue_types_follow_organization_policy_and_live_repository_roles() {
    let (h, alice, org, repo, key) = fixture().await;
    let root = format!("/api/repos/{key}");
    let path = format!("{root}/issues/1");
    let schema_path = format!("{org}/issue-types");
    let initial = h.json(&schema_path, &alice).await;
    let mut definitions = initial["definitions"].as_array().unwrap().to_vec();
    let type_id = definitions[1]["id"].clone();
    assert_eq!(definitions[1]["name"], "Bug");
    let role = async |role| {
        let members = format!("{root}/members");
        let acl = h.json(&members, &alice).await;
        repository_write(
            &h,
            Method::PUT,
            &members,
            &alice,
            repo,
            json!({"expected_revision":acl["revision"],"members":[{"subject":"bob-id","name":"Bob","access":role}],"groups":[]}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    role("read").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let assign = async |cookie: &str, version, id: Value| {
        repository_write(
            &h,
            Method::PUT,
            &format!("{path}/type"),
            cookie,
            repo,
            json!({"expected_version":version,"type_id":id}),
        )
        .await
    };
    assert_eq!(
        assign(&bob, 0, type_id.clone()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert!(h.json(&path, &bob).await.get("issue_types").is_none());
    let replace = async |cookie: &str, version, definitions: &[Value]| {
        h.write(
            Method::PUT,
            &schema_path,
            cookie,
            json!({"expected_version":version,"definitions":definitions}),
        )
        .await
    };
    assert_eq!(
        replace(&bob, 0, &definitions).await.status(),
        StatusCode::NOT_FOUND
    );
    role("triage").await;
    for _ in 0..2 {
        let reply: Value = assign(&bob, 0, type_id.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reply["version"], 1);
    }
    assert_eq!(
        assign(&bob, 0, Value::Null).await.status(),
        StatusCode::CONFLICT
    );

    definitions[1]["name"] = json!("Incident");
    for _ in 0..2 {
        let response: Value = replace(&alice, 0, &definitions)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["version"], 1);
    }
    let list = format!("{root}/issues");
    for (version, enabled, label) in [(1, false, Value::Null), (2, true, json!("Incident"))] {
        definitions[1]["enabled"] = json!(enabled);
        replace(&alice, version, &definitions)
            .await
            .error_for_status()
            .unwrap();
        assert_eq!(h.json(&path, &bob).await["issue_type"]["name"], label);
        assert_eq!(
            h.json(&list, &bob).await["items"][0]["issue_type"]["name"],
            label
        );
        if !enabled {
            assert_eq!(
                assign(&bob, 1, type_id.clone()).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
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
    assert_eq!(
        assign(&bob, 1, Value::Null).await.status(),
        StatusCode::FORBIDDEN
    );
    let removed = definitions.remove(1);
    replace(&alice, 3, &definitions)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(h.json(&path, &bob).await["issue_type"], Value::Null);
    definitions.push(removed);
    assert_eq!(
        replace(&alice, 4, &definitions).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.json(
            &format!("{org}/audit-log?action=issue_types_changed"),
            &alice
        )
        .await["items"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn issue_type_filters_count_sparse_results_and_bind_schema_changes() {
    let (h, alice, org, repo, key) = fixture().await;
    let root = format!("/api/repos/{key}");
    let issues = format!("{root}/issues");
    let schema_path = format!("{org}/issue-types");
    let schema = h.json(&schema_path, &alice).await;
    let mut definitions = schema["definitions"].as_array().unwrap().to_vec();
    let bug = definitions[1]["id"].clone();
    for title in ["Unclassified", "Second bug", "Newest unclassified"] {
        repository_write(
            &h,
            Method::POST,
            &issues,
            &alice,
            repo,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let assign = async |number, version, type_id: Value| {
        repository_write(
            &h,
            Method::PUT,
            &format!("{issues}/{number}/type"),
            &alice,
            repo,
            json!({"expected_version":version,"type_id":type_id}),
        )
        .await
        .error_for_status()
        .unwrap();
    };
    assign(1, 0, bug.clone()).await;
    assign(3, 0, bug).await;

    let url = |query: &str, before: Option<&str>| {
        let mut params = url::form_urlencoded::Serializer::new(String::new());
        params.append_pair("q", query).append_pair("limit", "1");
        if let Some(before) = before {
            params.append_pair("before", before);
        }
        format!("{issues}?{}", params.finish())
    };
    let query = "is:open type:bUG";
    let first = h.json(&url(query, None), &alice).await;
    assert_eq!(first["matching"], 2);
    assert_eq!(first["counts"]["open"], 2);
    assert_eq!(first["items"][0]["number"], 3);
    let cursor = first["next"].as_str().unwrap();
    let next = h.json(&url(query, Some(cursor)), &alice).await;
    assert_eq!(next["items"][0]["number"], 1);
    assert_eq!(next["next"], Value::Null);
    let mixed = h
        .json(&url("is:open (type:Bug OR no:type)", None), &alice)
        .await;
    assert_eq!(mixed["matching"], 4);
    let completion = h
        .json(
            &format!("{root}/discussion-search?query=type%3Ab&cursor=6"),
            &alice,
        )
        .await;
    assert_eq!(completion["items"][0]["value"], "type:\"Bug\"");

    definitions[1]["name"] = json!("Incident report");
    h.write(
        Method::PUT,
        &schema_path,
        &alice,
        json!({"expected_version":0,"definitions":definitions}),
    )
    .await
    .error_for_status()
    .unwrap();
    let stale = async |query: &str, cursor: &str| {
        h.http
            .get(format!("{}{}", h.origin, url(query, Some(cursor))))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(stale(query, cursor).await, StatusCode::CONFLICT);
    assert_eq!(h.json(&url(query, None), &alice).await["matching"], 0);
    let renamed = "is:open type:\"Incident report\"";
    let renamed_page = h.json(&url(renamed, None), &alice).await;
    assert_eq!(renamed_page["matching"], 2);
    assign(3, 1, Value::Null).await;
    assert_eq!(
        stale(renamed, renamed_page["next"].as_str().unwrap()).await,
        StatusCode::CONFLICT
    );

    definitions[1]["enabled"] = json!(false);
    h.write(
        Method::PUT,
        &schema_path,
        &alice,
        json!({"expected_version":1,"definitions":definitions}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.json(&url("is:open no:type", None), &alice).await["matching"],
        4
    );
    assert_eq!(h.json(&url(renamed, None), &alice).await["matching"], 0);
    let menu = h
        .json(
            &format!("{root}/discussion-search/issue-types?query=is%3Aopen"),
            &alice,
        )
        .await;
    assert_eq!(menu["items"].as_array().unwrap().len(), 2);
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{root}/discussion-search/issue-types", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND,
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and built embedded UI; uses real OIDC/native Cells"]
async fn native_browser_manages_and_assigns_issue_types() {
    use crate::test_support::http::browser::Browser;

    let (h, alice, org, repo, key) = fixture().await;
    repository_write(
        &h,
        Method::POST,
        &format!("/api/repos/{key}/issues"),
        &alice,
        repo,
        json!({"request_id":Uuid::now_v7(),"title":"Not yet classified","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    let browser = Browser::new("issue-types");
    let settings = format!(
        "{}/-/organizations/{}?tab=issue-types",
        h.origin,
        org.rsplit('/').next().unwrap()
    );
    let issue = format!("{}/{key}/issues/1", h.origin);
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.run(&["open", &settings]).await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Add issue type"]).await;
    click("button", "Add issue type").await;
    browser
        .run(&["fill", "#issue-type-name", "Reliability"])
        .await;
    click("button", "Save type").await;
    browser.run(&["wait", "--text", "Reliability"]).await;

    browser.run(&["open", &issue]).await;
    browser.run(&["wait", "--text", "Save issue type"]).await;
    click("combobox", "Issue type").await;
    click("option", "Reliability").await;
    click("button", "Save issue type").await;
    browser
        .run(&[
            "wait",
            "--fn",
            "document.querySelector('.issue-meta')?.textContent.includes('Reliability')",
        ])
        .await;
    click("link", "← Back to issues").await;
    browser.run(&["wait", "--text", "Reliability"]).await;
    browser.run(&["wait", "--text", "Not yet classified"]).await;
    click("button", "Filter by issue type").await;
    browser.run(&["wait", "[role='option'][id]"]).await;
    click("option", "Reliability").await;
    browser.run(&["wait", "--fn", "new URL(location.href).searchParams.get('q')?.includes('type:') && document.body.textContent.includes('Classify this report') && !document.body.textContent.includes('Not yet classified')"]).await;

    browser.run(&["open", &settings]).await;
    browser.run(&["wait", "--text", "Reliability"]).await;
    click("button", "Disable Reliability").await;
    browser.run(&["wait", "--text", "Disabled"]).await;
    browser.run(&["open", &issue]).await;
    browser.run(&["wait", "--text", "Save issue type"]).await;
    assert!(!browser.run(&["snapshot"]).await.contains("Reliability"));
}
