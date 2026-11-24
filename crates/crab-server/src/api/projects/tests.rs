use crate::test_support::http::Harness;
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;
const ROOT: &str = "/api/repos/team/private";

async fn project_command(h: &Harness, actor: &str, body: Value) -> Value {
    h.write(
        Method::POST,
        &format!("{ROOT}/projects/commands"),
        actor,
        body,
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn bulk_project(h: &Harness, actor: &str) -> (Uuid, [Uuid; 2]) {
    let project = Uuid::now_v7();
    assert_eq!(
        project_command(
            h,
            actor,
            json!({"request_id":Uuid::now_v7(),"command":{
                "kind":"create","id":project,"name":"Release planning","template":"table"
            }})
        )
        .await["outcome"],
        "applied"
    );
    let ids = [Uuid::now_v7(), Uuid::now_v7()];
    for (id, title) in ids
        .into_iter()
        .zip(["First deliverable", "Second deliverable"])
    {
        assert_eq!(project_command(h, actor, json!({"request_id":Uuid::now_v7(),"command":{
            "kind":"item","project":project,"version":0,"item":{
                "id":id,"subject":null,"title":title,"body":"Preserve these notes","status":"Todo",
                "priority":2,"start":null,"end":null,"values":{}
            }
        }})).await["outcome"], "applied");
    }
    (project, ids)
}

#[tokio::test]
async fn bulk_project_fields_apply_once_and_roll_back_on_a_stale_item() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (project, ids) = bulk_project(&h, &alice).await;
    let route = format!("{ROOT}/projects/{project}/items");
    let request = |versions: [u64; 2], field: Value| {
        json!({
            "request_id":Uuid::now_v7(), "command":{"kind":"bulk_fields","project":project,
            "items":ids.into_iter().zip(versions).map(|(item,version)| json!({
                "item":item,"version":version,"change":field
            })).collect::<Vec<_>>()}
        })
    };
    let stale = request([1, 2], json!({"field":"status","value":"Done"}));
    assert_eq!(
        project_command(&h, &alice, stale.clone()).await,
        json!({"outcome":"item_conflict","project":project,"item":ids[1],"version":1})
    );
    let unchanged = h.json(&route, &alice).await;
    assert!(
        unchanged["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["version"] == 1 && row["item"]["status"] == "Todo")
    );
    let batch = request([1, 1], json!({"field":"status","value":"Done"}));
    let applied = project_command(&h, &alice, batch.clone()).await;
    assert_eq!(applied["outcome"], "applied");
    assert_eq!(project_command(&h, &alice, batch).await, applied);
    assert_eq!(
        project_command(&h, &alice, stale).await["outcome"],
        "item_conflict"
    );
    let page = h.json(&route, &alice).await;
    assert!(
        page["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["version"] == 2
                && row["item"]["status"] == "Done"
                && row["item"]["priority"] == 2)
    );
    assert_eq!(
        project_command(
            &h,
            &alice,
            request([2, 1], json!({"field":"status","value":"Todo"}))
        )
        .await["outcome"],
        "item_conflict"
    );
    assert_eq!(
        h.json(&route, &alice).await["page"]["items"],
        page["page"]["items"]
    );
    let unchanged = request([2, 2], json!({"field":"status","value":"Done"}));
    assert_eq!(project_command(&h, &alice, unchanged).await, applied);
    let stale_unchanged = request([2, 1], json!({"field":"status","value":"Done"}));
    assert_eq!(
        project_command(&h, &alice, stale_unchanged).await["outcome"],
        "item_conflict"
    );
    assert_eq!(
        h.json(&route, &alice).await["page"]["items"],
        page["page"]["items"]
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires agent-browser and the built embedded UI"]
async fn browser_bulk_edits_moves_and_undoes_project_items() {
    use crate::test_support::http::browser::Browser;
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (project, _) = bulk_project(&h, &alice).await;
    let browser = Browser::new("project-bulk");
    browser
        .run(&[
            "open",
            &format!("{}/team/private/projects/{project}", h.origin),
        ])
        .await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "First deliverable"]).await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Select all Project items on this page",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "combobox",
            "click",
            "--name",
            "New value",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "find", "role", "option", "click", "--name", "Done", "--exact",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Apply to selected",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Saved 2 Project items."])
        .await;
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('[role=combobox]')].filter(el => el.getAttribute('aria-label')?.startsWith('Status for') && el.textContent.trim() === 'Done').length === 2"]).await;
    let route = format!("{ROOT}/projects/{project}/items");
    assert!(
        h.json(&route, &alice).await["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["item"]["status"] == "Done")
    );
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Undo", "--exact",
        ])
        .await;
    browser.run(&["wait", "--fn", "[...document.querySelectorAll('[role=combobox]')].filter(el => el.getAttribute('aria-label')?.startsWith('Status for') && el.textContent.trim() === 'Todo').length === 2"]).await;
    let reverted = h.json(&route, &alice).await;
    assert!(
        reverted["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["version"] == 3 && row["item"]["status"] == "Todo")
    );
    browser
        .run(&["find", "role", "tab", "click", "--name", "Board", "--exact"])
        .await;
    browser
        .run(&[
            "wait",
            "[role=checkbox][aria-label='Select all editable cards in Todo']",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Select all editable cards in Todo",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "2 selected"]).await;
    browser
        .run(&[
            "drag",
            ".project-board-column[aria-label='Todo'] .project-board-card:first-child",
            ".project-board-column[aria-label='Done']",
        ])
        .await;
    browser.run(&["wait", "--fn", "document.querySelectorAll(\".project-board-column[aria-label='Done'] .project-board-card\").length === 2"]).await;
    let moved = h.json(&route, &alice).await;
    assert!(
        moved["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["version"] == 4 && row["item"]["status"] == "Done")
    );
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Undo", "--exact",
        ])
        .await;
    browser.run(&["wait", "--fn", "document.querySelectorAll(\".project-board-column[aria-label='Todo'] .project-board-card\").length === 2"]).await;
    let restored = h.json(&route, &alice).await;
    assert!(
        restored["page"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["version"] == 5 && row["item"]["status"] == "Todo")
    );
    drop(browser);
    h.close().await;
}

#[tokio::test]
async fn projects_are_native_versioned_and_repository_authorized() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let id = Uuid::now_v7();
    let body = json!({"request_id":Uuid::now_v7(),"command":{"kind":"create","id":id,"name":"Release","template":"kanban"}});
    let path = format!("{ROOT}/projects/commands");
    let first: Value = h
        .write(Method::POST, &path, &alice, body.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["outcome"], "applied");
    let retried: Value = h
        .write(Method::POST, &path, &alice, body)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, retried);
    assert_eq!(
        h.json(&format!("{ROOT}/projects/{id}/items"), &alice).await["page"]["project"]["definition"]
            ["name"],
        "Release"
    );
    h.write(Method::POST,&format!("{ROOT}/issues"),&alice,json!({"request_id":Uuid::now_v7(),"title":"Native linked issue","body":"Private discussion body"})).await.error_for_status().unwrap();
    h.write(Method::POST,&path,&alice,json!({"request_id":Uuid::now_v7(),"command":{"kind":"item","project":id,"version":0,"item":{"id":Uuid::now_v7(),"subject":{"kind":"issue","number":1},"title":"","body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}}}})).await.error_for_status().unwrap();
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Unblock the release","body":""}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::PUT,
        &format!("{ROOT}/issues/1/dependencies/blocked_by/2"),
        &alice,
        json!({"version":0,"active":true}),
    )
    .await
    .error_for_status()
    .unwrap();
    let page = h.json(&format!("{ROOT}/projects/{id}/items"), &alice).await;
    assert_eq!(
        page["page"]["items"][0]["work"]["title"],
        "Native linked issue"
    );
    assert_eq!(page["page"]["items"][0]["blocked"], true);
    h.write(
        Method::PATCH,
        &format!("{ROOT}/issues/2"),
        &alice,
        json!({"version":1,"state":"closed"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let unblocked = h.json(&format!("{ROOT}/projects/{id}/items"), &alice).await;
    assert_ne!(unblocked["page"]["items"][0]["blocked"], true);
    assert_eq!(
        unblocked["page"]["items"][0]["version"],
        page["page"]["items"][0]["version"]
    );
    let history = h
        .json(&format!("{ROOT}/projects/{id}/history?after=0"), &alice)
        .await;
    assert_eq!(history["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        history["items"][1]["target"]["source"]["subject"]["number"],
        1
    );
    for private in ["alice-id", "Private discussion body", "issuer"] {
        assert!(!history.to_string().contains(private));
        assert!(!page.to_string().contains(private));
    }
    assert_eq!(
        h.http
            .post(format!("{}{path}", h.origin))
            .header(header::COOKIE, &alice)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for command in [
        json!({"kind":"unknown"}),
        json!({"kind":"create","id":Uuid::now_v7(),"template":"board","name":"x".repeat(100*1024)}),
    ] {
        let rejected = h
            .write(
                Method::POST,
                &path,
                &alice,
                json!({"request_id":Uuid::now_v7(),"command":command}),
            )
            .await;
        assert!(rejected.status().is_client_error());
        assert_eq!(rejected.headers()["x-crab-write-outcome"], "rejected");
    }
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(&format!("{ROOT}/projects"), &bob).await["can_manage"],
        false
    );
    assert_eq!(h.write(Method::POST,&path,&bob,json!({"request_id":Uuid::now_v7(),"command":{"kind":"create","id":Uuid::now_v7(),"name":"No","template":"board"}})).await.status(),StatusCode::FORBIDDEN);
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}/projects/{id}", h.origin))
            .header(header::COOKIE, outsider)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
