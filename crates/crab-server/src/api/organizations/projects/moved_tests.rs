use super::*;
use crate::{RepositoryAccess, RepositoryMember, test_support::http::Harness};
use axum::http::StatusCode;
use reqwest::Method;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_home_organization_project_keeps_one_placement_through_transfer_and_replay() {
    let h = Harness::new(false).await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let csrf = h.json("/api/session", &alice).await["csrf"]
        .as_str()
        .unwrap()
        .to_owned();
    let org = format!("/api/organizations/{organization}");
    let create_repository = async |name: &str| {
        let result: Value = h
            .write(
                Method::POST,
                &format!("{org}/repositories/creation"),
                &alice,
                json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = Uuid::parse_str(result["id"].as_str().unwrap()).unwrap();
        (
            id,
            format!("/api/repos/{}/{name}", result["owner"].as_str().unwrap()),
        )
    };
    let (source, source_root) = create_repository("planning-source").await;
    let (destination, _) = create_repository("planning-target").await;
    let repository_post = async |path: &str, body: Value| {
        h.http
            .post(format!("{}{path}", h.origin))
            .header(axum::http::header::COOKIE, &alice)
            .header(axum::http::header::ORIGIN, &h.origin)
            .header("x-csrf-token", &csrf)
            .header("x-crab-repository-id", source.to_string())
            .json(&body)
            .send()
            .await
            .unwrap()
    };
    let issue: Value = repository_post(
        &format!("{source_root}/issues"),
        json!({"request_id":Uuid::now_v7(),"title":"Track the production rollout","body":""}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    let project = Uuid::now_v7();
    let project_item = Uuid::now_v7();
    let commands = format!("{org}/projects/commands");
    let send = async |input: Value| -> Value {
        h.write(Method::POST, &commands, &alice, input)
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    let placement = |id, number| {
        json!({"id":id,"subject":{"kind":"issue","number":number},
        "title":"","body":"","status":"Todo","priority":1,"start":null,"end":null,"values":{}})
    };
    for command in [
        json!({"kind":"create","id":project,"name":"Organization rollout","template":"board"}),
        json!({"kind":"repository","project":project,"version":1,"repository":source,"linked":true}),
        json!({"kind":"repository","project":project,"version":2,"repository":destination,"linked":true}),
        json!({"kind":"item","project":project,"version":0,"repository":source,"item":placement(project_item,issue["number"].as_u64().unwrap())}),
    ] {
        let reply = send(json!({"request_id":Uuid::now_v7(),"command":command})).await;
        assert_eq!(reply["outcome"], "applied", "{reply}");
    }
    let original = format!("{source_root}/issues/{}", issue["number"]);
    let transfer =
        json!({"request_id":Uuid::now_v7(),"version":issue["version"],"destination":destination});
    for _ in 0..2 {
        let reply = repository_post(&format!("{original}/transfer"), transfer.clone()).await;
        assert_eq!(
            reply.status(),
            StatusCode::ACCEPTED,
            "{}",
            reply.text().await.unwrap()
        );
    }
    let moved = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let result = h.json(&original, &alice).await;
            if result["transfer"]["state"] == "ready" {
                break result["transfer"]["number"].as_u64().unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    let duplicate = json!({"request_id":Uuid::now_v7(),"command":{
        "kind":"item","project":project,"version":0,"repository":destination,"item":placement(Uuid::now_v7(),moved),
    }});
    let first = send(duplicate.clone()).await;
    assert_eq!(first["outcome"], "issue_already_linked", "{first}");
    assert_eq!(send(duplicate).await, first);
    let items = h
        .json(
            &format!("{org}/projects/{project}/items?repository={destination}"),
            &alice,
        )
        .await;
    assert_eq!(items["page"]["total"], 1);
    let row = &items["page"]["items"][0];
    assert_eq!(row["item"]["id"], json!(project_item));
    assert_eq!(row["item"]["priority"], 1);
    assert_eq!(row["source_repository"], json!(source));
    assert_eq!(row["repository"]["id"], json!(destination));
    h.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_keeps_planning_fields_and_edits_after_the_issue_moves() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let find_repo = |name: &str| {
        h.server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap()
    };
    let original = find_repo("private");
    let destination = find_repo("library");
    let project = Uuid::now_v7();
    let item = Uuid::now_v7();
    let root = format!("/api/organizations/{organization}/projects");
    let commands = format!("{root}/commands");
    let rows = format!("{root}/{project}/items");
    let original_issues = "/api/repos/team/private/issues";
    for title in ["Other work", "Moved deployment"] {
        h.write(
            Method::POST,
            original_issues,
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":title,"body":""}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let subject = json!({"kind":"issue","number":2});
    for command in [
        json!({"kind":"create","id":project,"name":"Release plan","template":"board"}),
        json!({"kind":"repository","project":project,"version":1,"repository":original.id,"linked":true}),
        json!({"kind":"item","project":project,"version":0,"repository":original.id,"item":{"id":item,"subject":subject,"title":"","body":"","status":"Todo","priority":0,"start":null,"end":null,"values":{}}}),
    ] {
        let result: Value = h
            .write(
                Method::POST,
                &commands,
                &alice,
                json!({"request_id":Uuid::now_v7(),"command":command}),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(result["outcome"], "applied", "{result}");
    }
    let old = h.json(&rows, &alice).await["page"]["items"][0].clone();
    assert_eq!(old["repository"]["id"], json!(original.id));
    let path = format!("{original_issues}/2");
    let issue = h.json(&path, &alice).await;
    let response = h.write(Method::POST, &format!("{path}/transfer"), &alice,
        json!({"request_id":Uuid::now_v7(),"version":issue["version"],"destination":destination.id})).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let mut row = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let row = h.json(&rows, &alice).await["page"]["items"][0].clone();
            if row["repository"]["id"] == json!(destination.id) {
                break row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(row["source_repository"], json!(original.id));
    assert_eq!(row["item"], old["item"]);
    assert_eq!(row["version"], old["version"]);
    assert_eq!(row["work"]["title"], "Moved deployment");
    assert_eq!(row["work"]["subject"], json!({"kind":"issue","number":1}));

    row["item"]["status"] = json!("Done");
    let saved: Value = h.write(Method::POST, &commands, &alice, json!({
        "request_id":Uuid::now_v7(),"command":{"kind":"item","project":project,"version":row["version"],"repository":row["source_repository"],"item":row["item"]}
    })).await.json().await.unwrap();
    assert_eq!(saved["outcome"], "applied", "{saved}");
    let current = h.json(&rows, &alice).await["page"]["items"][0].clone();
    assert_eq!(current["item"]["status"], "Done");
    assert_eq!(current["repository"]["id"], json!(destination.id));
    let filters = format!("{root}/{project}/filter-repositories");
    let choices = h.json(&filters, &alice).await;
    assert!(
        choices["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|repo| repo["id"] == json!(destination.id))
    );
    let destination_filter = format!("{rows}?repository={}&status=Done", destination.id);
    let page = h.json(&destination_filter, &alice).await["page"].clone();
    assert_eq!(page["items"][0]["item"]["id"], json!(item));
    assert_eq!(page["total"], 1);
    assert_eq!(page["counts"], json!({"Done":1}));
    assert!(page["type_cursor"].is_string());
    let origin_filter = format!("{rows}?repository={}", original.id);
    assert_eq!(h.json(&origin_filter, &alice).await["page"]["total"], 0);
    let history = h
        .json(&format!("{root}/{project}/history?after=0"), &alice)
        .await;
    let events = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["target"]["id"] == json!(item))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(
            event["target"]["source"]["repository"]["id"],
            json!(original.id)
        );
        assert_eq!(event["target"]["source"]["subject"], subject);
    }

    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "library",
            vec![RepositoryMember {
                subject: "bob-id".into(),
                name: "Bob".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    let hidden = h.json(&rows, &alice).await["page"]["items"][0].clone();
    assert_eq!(hidden["restricted"], true);
    assert_eq!(hidden["item"]["status"], "Done");
    assert!(hidden["source_repository"].is_null());
    assert!(hidden["repository"].is_null());
    assert!(hidden["work"].is_null());
    let history = h
        .json(&format!("{root}/{project}/history?after=0"), &alice)
        .await;
    for event in history["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["target"]["id"] == json!(item))
    {
        assert_eq!(event["target"]["restricted"], true);
        assert!(event["target"]["source"].is_null());
        assert_eq!(event["changes"], json!([]));
    }
    let choices = h.json(&filters, &alice).await;
    assert!(
        choices["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|repo| repo["id"] != json!(destination.id))
    );
    let restricted_page = h.json(&destination_filter, &alice).await["page"].clone();
    assert_eq!(restricted_page["total"], 0);
    assert_eq!(restricted_page["counts"], json!({}));
    let stale = h
        .http
        .get(format!(
            "{}{}&after={item}&version={}&type_cursor={}",
            h.origin,
            destination_filter,
            page["revision"],
            page["type_cursor"].as_str().unwrap(),
        ))
        .header(axum::http::header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(
        stale.json::<Value>().await.unwrap()["error"]["code"],
        "stale_search_cursor"
    );
    let denied: Value = h.write(Method::POST, &commands, &alice, json!({
        "request_id":Uuid::now_v7(),"command":{"kind":"item","project":project,"version":current["version"],"repository":original.id,"item":current["item"]}
    })).await.json().await.unwrap();
    assert_eq!(denied["outcome"], "rejected", "{denied}");
    h.close().await;
}
