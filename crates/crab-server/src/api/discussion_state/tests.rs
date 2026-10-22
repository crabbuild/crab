use crate::{
    cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor},
    test_support::http::Harness,
};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
const ROOT: &str = "/api/repos/team/private";
async fn seed(h: &Harness, cookie: &str) {
    h.write(
        Method::POST,
        &format!("{ROOT}/issues"),
        cookie,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"State action","body":"Description"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let cell = h
        .server
        .cells()
        .unwrap()
        .repository(h.server.repositories.values()[0].id)
        .unwrap();
    cell.client
        .command::<CreatePull>(
            &cell.target,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                submission_id: *uuid::Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                draft: false,
                title: "State action".into(),
                body: "Description".into(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
}
#[tokio::test]
async fn combined_issue_and_pull_actions_commit_one_comment_and_recover_the_same_submission() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h, &alice).await;
    for kind in ["issues", "pulls"] {
        let path = format!("{ROOT}/{kind}/1");
        let mut input = json!({"request_id":uuid::Uuid::now_v7(),"version":1,"state":"closed","body":"Closing with context"});
        if kind == "issues" {
            input["reason"] = json!("not_planned");
        }
        let result: Value = h
            .write(
                Method::POST,
                &format!("{path}/state"),
                &alice,
                input.clone(),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(result["comment"]["body"], "Closing with context");
        assert!(!result.to_string().contains("alice-id"));
        let replay: Value = h
            .write(
                Method::POST,
                &format!("{path}/state"),
                &alice,
                input.clone(),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(replay["comment"]["number"], result["comment"]["number"]);
        assert_eq!(
            h.json(&format!("{path}/comments"), &alice).await["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let activity = h.json(&format!("{path}/activity"), &alice).await;
        assert_eq!(activity["items"].as_array().unwrap().len(), 3);
        input["body"] = json!("Different request");
        assert_eq!(
            h.write(Method::POST, &format!("{path}/state"), &alice, input)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        let stale = json!({"request_id":uuid::Uuid::now_v7(),"version":1,"state":"open","body":"Must roll back"});
        assert_eq!(
            h.write(Method::POST, &format!("{path}/state"), &alice, stale)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            h.json(&format!("{path}/comments"), &alice).await["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let opened = json!({"request_id":uuid::Uuid::now_v7(),"version":2,"state":"open","body":"Reopening with context"});
        h.write(Method::POST, &format!("{path}/state"), &alice, opened)
            .await
            .error_for_status()
            .unwrap();
        assert_eq!(
            h.json(&format!("{path}/comments"), &alice).await["items"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let conversation = h.json(&format!("{path}/conversation"), &alice).await;
        assert_eq!(
            conversation["items"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["kind"] == "comment")
                .count(),
            2
        );
    }
    h.close().await;
}
#[tokio::test]
async fn denied_state_actions_cannot_publish_orphan_comments() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h, &alice).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    for kind in ["issues", "pulls"] {
        let path = format!("{ROOT}/{kind}/1");
        let input = json!({"request_id":uuid::Uuid::now_v7(),"version":1,"state":"closed","body":"Unauthorized comment"});
        let denied = h
            .write(Method::POST, &format!("{path}/state"), &bob, input)
            .await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(denied.headers()["x-crab-write-outcome"], "rejected");
        assert_eq!(
            h.json(&format!("{path}/comments"), &alice).await["items"],
            json!([])
        );
    }
    h.close().await;
}

#[tokio::test]
async fn state_routes_require_current_membership_and_csrf_even_for_recovery() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    seed(&h, &alice).await;
    let input = json!({"request_id":uuid::Uuid::now_v7(),"version":1,"state":"closed","body":"Private action"});
    for kind in ["issues", "pulls"] {
        let path = format!("{ROOT}/{kind}/1/state");
        assert_eq!(
            h.http
                .post(format!("{}{path}", h.origin))
                .header("cookie", &alice)
                .json(&input)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    h.write(
        Method::POST,
        &format!("{ROOT}/issues/1/state"),
        &alice,
        input.clone(),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    assert_eq!(
        h.write(
            Method::POST,
            &format!("{ROOT}/issues/1/state"),
            &outsider,
            input
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
