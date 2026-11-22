use crate::{
    cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor},
    test_support::http::Harness,
};
use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
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
async fn mentions_include_group_only_profiles_with_live_signed_grants() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let repository = &h.server.repositories.values()[0];
    replace_groups(
        &h,
        &alice,
        json!([{"group":"reviewers","access":"read"}]),
        false,
    )
    .await;

    *h.provider.mode.lock().await = "member".into();
    let bob_without_claim = h.login().await;
    let bob = enroll(&h, &bob_without_claim, "bob", "Robert Example").await;
    *h.provider.mode.lock().await = "outsider".into();
    h.provider
        .claims
        .lock()
        .await
        .insert("groups".into(), json!(["other"]));
    let outsider = h.login().await;
    enroll(&h, &outsider, "bobby", "Robert Other").await;
    let base = format!(
        "{ROOT}/discussion-search/mentions?repository_id={}",
        repository.id
    );
    let by_handle = format!("{base}&search=bo");
    let by_name = format!("{base}&search=Robert");
    assert_eq!(h.json(&by_handle, &alice).await, json!({"items":[]}));

    *h.provider.mode.lock().await = "member".into();
    h.provider
        .claims
        .lock()
        .await
        .insert("groups".into(), json!(["reviewers"]));
    let bob_signed_in = h.login().await;
    let expected = json!({"items":[{"id":bob["id"],"handle":"bob","name":"Robert Example"}]});
    for path in [&base, &by_handle, &by_name] {
        assert_eq!(h.json(path, &alice).await, expected);
    }
    assert_eq!(h.json(&by_handle, &bob_signed_in).await, expected);
    for denied in [&bob_without_claim, &outsider] {
        assert_eq!(
            h.http
                .get(format!("{}{by_handle}", h.origin))
                .header(header::COOKIE, denied)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND,
        );
    }

    replace_groups(
        &h,
        &alice,
        json!([{"group":"reviewers","access":"read"}]),
        true,
    )
    .await;
    assert_eq!(h.json(&by_handle, &alice).await, expected);
    replace_groups(
        &h,
        &alice,
        json!([{"group":"different","access":"read"}]),
        false,
    )
    .await;
    assert_eq!(h.json(&by_handle, &alice).await, json!({"items":[]}));
    replace_groups(
        &h,
        &alice,
        json!([{"group":"reviewers","access":"read"}]),
        false,
    )
    .await;
    assert_eq!(h.json(&by_handle, &alice).await, expected);
    h.server
        .auth
        .as_ref()
        .unwrap()
        .revoke_identity(&h.provider.issuer, "bob-id")
        .await
        .unwrap();
    assert_eq!(h.json(&by_handle, &alice).await, json!({"items":[]}));
    h.close().await;
}

async fn enroll(h: &Harness, cookie: &str, handle: &str, name: &str) -> Value {
    h.write(
        Method::POST,
        "/api/profile",
        cookie,
        json!({"request_id":Uuid::now_v7(),"handle":handle,"name":name}),
    )
    .await
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap()
}

async fn replace_groups(h: &Harness, admin: &str, groups: Value, include_bob: bool) {
    let path = format!("{ROOT}/members");
    let acl = h.json(&path, admin).await;
    let members: Vec<_> = h.server.repositories.values()[0]
        .config
        .members
        .iter()
        .filter(|member| include_bob || member.subject != "bob-id")
        .cloned()
        .collect();
    h.write(
        Method::PUT,
        &path,
        admin,
        json!({
            "expected_revision":acl["revision"], "members":members, "groups":groups
        }),
    )
    .await
    .error_for_status()
    .unwrap();
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
                head_repository: None,
                code_owner_requests: Default::default(),
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
