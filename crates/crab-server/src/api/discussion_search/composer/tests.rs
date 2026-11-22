use crate::{
    cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor},
    test_support::http::Harness,
};
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::json;
use uuid::Uuid;

const ROOT: &str = "/api/repos/team/private";

#[tokio::test]
async fn mentions_return_current_collaborator_profiles_and_recheck_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let profile: serde_json::Value = h
        .write(
            Method::POST,
            "/api/profile",
            &bob,
            json!({"request_id":Uuid::now_v7(),"handle":"bob","name":"Robert Example"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let repository = &h.server.repositories.values()[0];
    let path = format!(
        "{ROOT}/discussion-search/mentions?repository_id={}&search=ROBERT",
        repository.id
    );
    assert_eq!(
        h.json(&path, &alice).await["items"],
        json!([{"id":profile["id"],"handle":"bob","name":"Robert Example"}])
    );
    let mut members = repository.config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    assert_eq!(h.json(&path, &alice).await["items"], json!([]));
    let denied = h
        .http
        .get(format!("{}{path}", h.origin))
        .header(header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

#[tokio::test]
async fn references_find_titles_and_disambiguate_issue_and_pull_numbers() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let repo = h.server.repositories.values()[0].id;
    for number in 1..=10 {
        h.write(
            Method::POST,
            &format!("{ROOT}/issues"),
            &alice,
            json!({"request_id": Uuid::now_v7(), "title": format!("Issue title {number}"), "body": "Body only"}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    h.server
        .cells()
        .unwrap()
        .command::<CreatePull>(
            repo,
            crate::app::mutation_identity().unwrap(),
            CreatePullInput {
                metadata: Default::default(),
                submission_id: *Uuid::now_v7().as_bytes(),
                author: RepositoryAuthor {
                    issuer: h.provider.issuer.clone(),
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                },
                title: "Improve the README".into(),
                body: String::new(),
                draft: false,
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    let path = format!("{ROOT}/discussion-search/references?repository_id={repo}");
    assert_eq!(
        h.json(&path, &alice).await["items"]
            .as_array()
            .unwrap()
            .len(),
        11
    );
    assert_eq!(
        h.json(&format!("{path}&search=1"), &alice).await["items"],
        json!([
            {"subject":{"kind":"pull","number":1},"title":"Improve the README","state":"open"},
            {"subject":{"kind":"issue","number":1},"title":"Issue title 1","state":"open"},
            {"subject":{"kind":"issue","number":10},"title":"Issue title 10","state":"open"},
        ])
    );
    assert_eq!(
        h.json(&format!("{path}&search=readme"), &alice).await["items"][0]["subject"]["kind"],
        "pull"
    );
    assert_eq!(
        h.json(&format!("{path}&search=body"), &alice).await["items"],
        json!([])
    );
    let exact_path = format!("{ROOT}/discussion-search/references?exact=true");
    let collision = h.json(&format!("{exact_path}&search=1"), &alice).await;
    assert_eq!(collision["repository_id"], json!(repo));
    assert_eq!(
        collision["items"],
        json!([
            {"subject":{"kind":"pull","number":1},"title":"Improve the README","state":"open"},
            {"subject":{"kind":"issue","number":1},"title":"Issue title 1","state":"open"},
        ])
    );
    let unique = h.json(&format!("{exact_path}&search=2"), &alice).await;
    assert_eq!(
        unique["items"],
        json!([{"subject":{"kind":"issue","number":2},"title":"Issue title 2","state":"open"}])
    );
    let destination = format!("/api/discussion-references/{repo}/pull/1");
    let opened = h
        .http
        .get(format!("{}{destination}", h.origin))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(opened.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        opened.headers()[header::LOCATION],
        format!("/team/private/pull/1?repository_id={repo}").as_str()
    );

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(&format!("{exact_path}&search=2"), &bob).await["repository_id"],
        json!(repo)
    );
    let mut members = h.server.repositories.values()[0].config.members.clone();
    members.retain(|member| member.subject != "bob-id");
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    for path in [format!("{exact_path}&search=1"), destination] {
        let denied = h
            .http
            .get(format!("{}{path}", h.origin))
            .header(header::COOKIE, &bob)
            .send()
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let replaced = h
        .http
        .get(format!(
            "{}{ROOT}/discussion-search/references?repository_id={}",
            h.origin,
            Uuid::nil()
        ))
        .header(header::COOKIE, &alice)
        .send()
        .await
        .unwrap();
    assert_eq!(replaced.status(), StatusCode::NOT_FOUND);
    h.close().await;
}
