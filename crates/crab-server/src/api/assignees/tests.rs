use crate::test_support::http::Harness;
use axum::http::StatusCode;
use axum::http::header;
use serde_json::Value;
use serde_json::json;

const ASSIGNEES: &str = "/api/repos/team/private/assignees";
const ISSUES: &str = "/api/repos/team/private/issues";

async fn write(
    h: &Harness,
    cookie: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let session = h.json("/api/session", cookie).await;
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn repository_members_are_assignable_by_writers() {
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let catalog = h.json(ASSIGNEES, &alice).await;
    assert_eq!(
        catalog,
        json!({
            "items":[
                {"subject":"alice-id","name":"Alice"},
                {"subject":"bob-id","name":"Bob"}
            ],
            "can_manage":true
        })
    );
    let issue = write(
        &h,
        &alice,
        reqwest::Method::POST,
        ISSUES,
        json!({
            "request_id":"00000000-0000-4000-8002-000000000001",
            "title":"Assign this report",
            "body":"Details"
        }),
    )
    .await
    .1;
    let issue_path = format!("{ISSUES}/{}", issue["number"]);
    let assigned = write(
        &h,
        &alice,
        reqwest::Method::PATCH,
        &issue_path,
        json!({"version":1,"assignees":["bob-id","alice-id"]}),
    )
    .await;
    assert_eq!(assigned.0, StatusCode::OK);
    assert_eq!(
        assigned.1["assignees"],
        json!([
            {"subject":"alice-id","name":"Alice"},
            {"subject":"bob-id","name":"Bob"}
        ])
    );
    assert_eq!(
        h.json(ISSUES, &alice).await["items"][0]["assignees"],
        assigned.1["assignees"]
    );
    for selection in [json!(["bob-id", "bob-id"]), json!(["outsider-id"])] {
        assert_eq!(
            write(
                &h,
                &alice,
                reqwest::Method::PATCH,
                &issue_path,
                json!({"version":2,"assignees":selection}),
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(ASSIGNEES, &bob).await["can_manage"], false);
    assert_eq!(
        write(
            &h,
            &bob,
            reqwest::Method::PATCH,
            &issue_path,
            json!({"version":2,"assignees":[]}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    h.close().await;
}

#[tokio::test]
async fn current_membership_controls_candidates_and_issue_and_pull_assignments() {
    use crate::cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor};

    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let created = h
        .write(
            reqwest::Method::POST,
            ISSUES,
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"title":"Assign from current members","body":""}),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let repo = h.server.repositories.values()[0].clone();
    h.server
        .cells()
        .unwrap()
        .command::<CreatePull>(
            repo.id,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                draft: false,
                title: "Assign this pull".into(),
                body: String::new(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();

    let mut members = vec![repo.config.members[0].clone()];
    members.push(crate::RepositoryMember {
        subject: "charlie-id".into(),
        name: "Charlie".into(),
        access: crate::RepositoryAccess::Read,
    });
    let catalog = h.server.catalog().unwrap();
    catalog
        .set_members("team", "private", members.clone(), true)
        .await
        .unwrap();
    // Keep the old routing object: no materialization may hide stale candidates.
    assert_eq!(repo.config.members[1].subject, "bob-id");
    assert_eq!(
        h.json(ASSIGNEES, &alice).await["items"],
        json!([
            {"subject":"alice-id","name":"Alice"},
            {"subject":"charlie-id","name":"Charlie"}
        ])
    );
    for kind in ["issues", "pulls"] {
        let path = format!("/api/repos/team/private/{kind}/1");
        let before = h.json(&path, &alice).await;
        let denied = h
            .write(
                reqwest::Method::PATCH,
                &path,
                &alice,
                json!({"version":before["version"],"assignees":["bob-id"]}),
            )
            .await;
        assert_eq!(denied.status(), StatusCode::BAD_REQUEST, "{kind}");
        assert_eq!(h.json(&path, &alice).await, before, "{kind}");
        let assigned = h
            .write(
                reqwest::Method::PATCH,
                &path,
                &alice,
                json!({"version":before["version"],"assignees":["charlie-id"]}),
            )
            .await;
        assert_eq!(assigned.status(), StatusCode::OK, "{kind}");
        assert_eq!(
            assigned.json::<Value>().await.unwrap()["assignees"],
            json!([{"subject":"charlie-id","name":"Charlie"}]),
            "{kind}"
        );
    }

    members[1].name = "Charles".into();
    catalog
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    for kind in ["issues", "pulls"] {
        let root = format!("/api/repos/team/private/{kind}");
        let expected = json!([{"subject":"charlie-id","name":"Charles"}]);
        let detail = h.json(&format!("{root}/1"), &alice).await;
        assert_eq!(detail["assignees"], expected, "{kind} detail");
        let list = h.json(&root, &alice).await;
        assert!(!list["items"].as_array().unwrap().is_empty());
        for item in list["items"].as_array().unwrap() {
            assert_eq!(item["assignees"], expected, "{kind} list");
        }
    }
    h.close().await;
}
