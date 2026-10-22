use crate::repository::catalog::RepositoryIndex;
use crate::test_support::http::Harness;
use axum::http::StatusCode;
use axum::http::header;
use crab_storage::Store;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn catalog_installation_preserves_revocation_before_http_access() {
    let h = Harness::new(false).await;
    *h.provider.mode.lock().await = "member".into();
    let cookie = h.login().await;
    h.json("/api/repos/team/private/labels", &cookie).await;
    let catalog = h.server.catalog().unwrap();
    let repository = h.server.repositories.values()[0].clone();
    catalog.mark_cell_ready(repository.id).await.unwrap();
    let members = repository
        .config
        .members
        .iter()
        .filter(|member| member.subject != "bob-id")
        .cloned()
        .collect();
    catalog
        .set_members("team", "private", members, false)
        .await
        .unwrap();
    // Installing the current durable membership must precede HTTP access.
    h.server
        .repositories
        .replace(RepositoryIndex::new(1, BTreeMap::new()));
    let (document, _) = catalog.load().await.unwrap();
    h.server.install_catalog(document).await.unwrap();
    let response = h
        .http
        .get(format!("{}/api/repos/team/private/labels", h.origin))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap();
    let status = response.status();
    h.close().await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

async fn replace(h: &Harness, cookie: &str, csrf: &str, body: Value) -> reqwest::Response {
    h.http
        .put(format!("{}/api/repos/team/private/members", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn delayed_catalog_materialization_cannot_restore_revoked_http_access() {
    delayed_catalog_materialization_preserves_http_denial(Harness::new(false).await).await;
}

#[tokio::test]
#[ignore = "requires a pre-created RustFS bucket, prefix and test credentials"]
async fn rustfs_delayed_catalog_materialization_cannot_restore_revoked_http_access() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let store = crab_storage::build_explicit_store(
        &required("CRAB_HTTP_CELL_TEST_BUCKET"),
        crab_storage::ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_HTTP_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    let prefix = format!(
        "{}/catalog-ordering-{}",
        required("CRAB_HTTP_CELL_TEST_PREFIX"),
        uuid::Uuid::now_v7(),
    );
    let scoped = object_store::prefix::PrefixStore::new(Arc::clone(store.inner()), prefix);
    let h = Harness::new_with_store(false, false, Store::new(Arc::new(scoped))).await;
    delayed_catalog_materialization_preserves_http_denial(h).await;
}

async fn delayed_catalog_materialization_preserves_http_denial(h: Harness) {
    let admin_cookie = h.login().await;
    let session = h.json("/api/session", &admin_cookie).await;
    *h.provider.mode.lock().await = "member".into();
    let member_cookie = h.login().await;
    h.json("/api/repos/team/private/labels", &member_cookie)
        .await;
    let catalog = h.server.catalog().unwrap();
    let repository = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    catalog.mark_cell_ready(repository.id).await.unwrap();
    let (delayed, _) = catalog.load().await.unwrap();
    let members = delayed.repositories[0]
        .members
        .iter()
        .filter(|member| member.subject != "bob-id")
        .cloned()
        .collect::<Vec<_>>();
    let response = replace(
        &h,
        &admin_cookie,
        session["csrf"].as_str().unwrap(),
        json!({"expected_revision":delayed.version, "members":members}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let (current, _) = catalog.load().await.unwrap();
    h.server.install_catalog(current).await.unwrap();
    let denied = h
        .http
        .get(format!("{}/api/repos/team/private/labels", h.origin))
        .header(header::COOKIE, &member_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    // Resume an import holding the pre-revocation snapshot after refresh has
    // already installed the membership mutation accepted through public HTTP.
    h.server.repositories.replace(
        crate::repository::catalog::materialize(&catalog, delayed)
            .await
            .unwrap(),
    );
    let response = h
        .http
        .get(format!("{}/api/repos/team/private/labels", h.origin))
        .header(header::COOKIE, member_cookie)
        .send()
        .await
        .unwrap();
    let status = response.status();
    h.close().await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn catalog_installation_keeps_current_revision_when_a_ready_cell_has_no_root() {
    let h = Harness::new(false).await;
    let catalog = h.server.catalog().unwrap();
    let original = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    catalog.mark_cell_ready(original.id).await.unwrap();
    let (current, _) = catalog.load().await.unwrap();
    let version = current.version;
    h.server.install_catalog(current).await.unwrap();
    let invalid = catalog
        .create_repository(
            "team".into(),
            "rootless".into(),
            "rootless".into(),
            "main".into(),
            String::new(),
            Vec::new(),
        )
        .await
        .unwrap();
    catalog.mark_cell_ready(invalid.id).await.unwrap();
    let (candidate, _) = catalog.load().await.unwrap();
    let result = h.server.install_catalog(candidate).await;
    let observed = (
        result.is_err(),
        h.server.repositories.version(),
        h.server.repositories.by_id(invalid.id).is_none(),
    );
    h.close().await;
    assert_eq!(observed, (true, version, true));
}

#[tokio::test]
async fn membership_cas_audit_and_current_authorization() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let csrf = session["csrf"].as_str().unwrap();
    let before = h.json("/api/repos/TEAM/PRIVATE/members", &cookie).await;
    let no_op = replace(
        &h,
        &cookie,
        csrf,
        json!({"expected_revision":before["revision"], "members":before["members"]}),
    )
    .await;
    assert_eq!(no_op.status(), StatusCode::OK);
    let no_op: Value = serde_json::from_slice(&no_op.bytes().await.unwrap()).unwrap();
    assert_eq!(no_op, before);
    let final_admin = replace(
        &h,
        &cookie,
        csrf,
        json!({"expected_revision":before["revision"], "members":[]}),
    )
    .await;
    assert_eq!(final_admin.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let mut members = before["members"].clone();
    members[1]["access"] = json!("admin");
    let changed = replace(
        &h,
        &cookie,
        csrf,
        json!({"expected_revision":before["revision"], "members":members}),
    )
    .await;
    assert_eq!(changed.status(), StatusCode::OK);
    let changed: Value = serde_json::from_slice(&changed.bytes().await.unwrap()).unwrap();
    assert_eq!(
        changed["revision"].as_u64(),
        Some(before["revision"].as_u64().unwrap() + 1)
    );
    assert_eq!(
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"], "members":members})
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    let catalog = h.server.catalog.as_ref().unwrap();
    let (document, _) = catalog.load().await.unwrap();
    let path = catalog
        .root()
        .path(&document.membership_audit_head.unwrap());
    let (event, _) = catalog
        .root()
        .store
        .get_with_etag_bounded(&path, 8192)
        .await
        .unwrap();
    let event: Value = serde_json::from_slice(&event).unwrap();
    assert_eq!(
        event["actor"],
        json!({"issuer":h.provider.issuer,"subject":"alice-id"})
    );
    members[0]["access"] = json!("read");
    assert_eq!(
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":changed["revision"], "members":members})
        )
        .await
        .status(),
        StatusCode::OK
    );
    // The routing snapshot still grants Alice admin; the catalog decision must not.
    assert_eq!(
        h.http
            .get(format!("{}/api/repos/team/private/members", h.origin))
            .header(header::COOKIE, &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test]
async fn membership_hides_non_admins_and_enforces_browser_mutation_protection() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let before = h.json("/api/repos/team/private/members", &cookie).await;
    let body = json!({"expected_revision":before["revision"],"members":before["members"]});
    for (origin, csrf) in [
        (None, None),
        (
            Some("https://wrong.invalid"),
            Some(session["csrf"].as_str().unwrap()),
        ),
        (Some(h.origin.as_str()), None),
        (Some(h.origin.as_str()), Some("wrong")),
    ] {
        let mut request = h
            .http
            .put(format!("{}/api/repos/team/private/members", h.origin))
            .header(header::COOKIE, &cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    for identity in ["member", "outsider"] {
        *h.provider.mode.lock().await = identity.into();
        let cookie = h.login().await;
        for name in ["private", "missing"] {
            assert_eq!(
                h.http
                    .get(format!("{}/api/repos/team/{name}/members", h.origin))
                    .header(header::COOKIE, &cookie)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
    h.close().await;
}

#[tokio::test]
async fn anonymous_membership_requests_are_hidden_without_changing_other_api_authentication() {
    let h = Harness::new(false).await;
    for name in ["private", "missing"] {
        for method in [reqwest::Method::GET, reqwest::Method::PUT] {
            let response = h
                .http
                .request(
                    method,
                    format!("{}/api/repos/team/{name}/members", h.origin),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
            assert_eq!(
                body,
                json!({"error":{"code":"repository_not_found","message":"Repository not found."}})
            );
        }
    }
    assert_eq!(
        h.http
            .get(format!("{}/api/repos", h.origin))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    h.close().await;
}

#[tokio::test]
async fn malformed_membership_is_rejected_and_two_writers_cannot_overwrite_each_other() {
    let h = Harness::new(false).await;
    let cookie = h.login().await;
    let session = h.json("/api/session", &cookie).await;
    let csrf = session["csrf"].as_str().unwrap();
    let before = h.json("/api/repos/team/private/members", &cookie).await;
    for mutation in [
        "empty-subject",
        "duplicate-subject",
        "duplicate-name",
        "unknown-access",
        "unknown-field",
        "oversized-subject",
    ] {
        let mut members = before["members"].clone();
        match mutation {
            "empty-subject" => members[0]["subject"] = json!(""),
            "duplicate-subject" => members[1]["subject"] = members[0]["subject"].clone(),
            "duplicate-name" => members[1]["name"] = json!("alice"),
            "unknown-access" => members[1]["access"] = json!("owner"),
            "unknown-field" => members[1]["extra"] = json!(true),
            _ => members[0]["subject"] = json!("s".repeat(513)),
        }
        let response = replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"],"members":members}),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{mutation}"
        );
        let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(body["error"]["code"], "invalid_membership");
    }
    let mut first = before["members"].clone();
    first[1]["name"] = json!("Bob First");
    let mut second = before["members"].clone();
    second[1]["name"] = json!("Bob Second");
    let (first, second) = tokio::join!(
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"],"members":first})
        ),
        replace(
            &h,
            &cookie,
            csrf,
            json!({"expected_revision":before["revision"],"members":second})
        )
    );
    let mut statuses = [first.status(), second.status()];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    h.close().await;
}

#[tokio::test]
async fn triage_uses_durable_grants_for_issue_and_pull_management_without_git_write() {
    use crate::cells::repository::{CreatePull, CreatePullInput, RepositoryAuthor};
    use reqwest::Method;
    let h = Harness::new(false).await;
    h.initialize_directory().await;
    let alice = h.login().await;
    let root = "/api/repos/team/private";
    let issue: Value = h.write(Method::POST, &format!("{root}/issues"), &alice,
        json!({"request_id":uuid::Uuid::now_v7(),"title":"Triage report","body":"Original body"}))
        .await.error_for_status().unwrap().json().await.unwrap();
    let label: Value = h
        .write(
            Method::POST,
            &format!("{root}/labels"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"name":"Investigate","color":"abcdef"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let milestone: Value = h
        .write(
            Method::POST,
            &format!("{root}/milestones"),
            &alice,
            json!({"request_id":uuid::Uuid::now_v7(),"title":"Triage milestone"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
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
                title: "Triage pull".into(),
                body: "Original pull".into(),
                base_ref: "refs/heads/main".into(),
                head_ref: "refs/heads/feature".into(),
                base_oid: "a".repeat(40),
                head_oid: "b".repeat(40),
            },
        )
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let mut members = repo.config.members.clone();
    members[1].access = crate::RepositoryAccess::Triage;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members.clone(), true)
        .await
        .unwrap();
    // Routing still contains Bob's old read role. Every capability below must
    // come from the catalog decision rather than delayed materialization.
    assert_eq!(repo.config.members[1].access, crate::RepositoryAccess::Read);
    let bob_id = h.assignee_id(&bob, "Bob").await;
    for kind in ["issues", "pulls"] {
        let path = format!("{root}/{kind}/1");
        let before = h.json(&path, &bob).await;
        assert_eq!(before["can_edit"], false);
        assert_eq!(before["can_label"], true);
        assert_eq!(before["can_assign"], true);
        if kind == "issues" {
            assert_eq!(before["can_close"], true);
        } else {
            assert_eq!(before["can_manage"], true);
            assert_eq!(before["can_merge"], false);
        }
        let denied = h.write(Method::PATCH, &path, &bob, json!({"version":before["version"],"title":"Cannot edit another author's title","state":"closed"})).await;
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        assert_eq!(h.json(&path, &bob).await["version"], before["version"]);
        let closed: Value = h.write(Method::PATCH, &path, &bob, json!({"version":before["version"],"state":"closed","label_ids":[label["id"]],"assignee_ids":[bob_id]})).await.error_for_status().unwrap().json().await.unwrap();
        assert_eq!(closed["state"], "closed");
        assert_eq!(closed["assignees"][0]["id"], bob_id);
        let reopened: Value = h
            .write(
                Method::PATCH,
                &path,
                &bob,
                json!({"version":closed["version"],"state":"open"}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(reopened["state"], "open");
        let assigned: Value = h
            .write(
                Method::PUT,
                &format!("{path}/milestone"),
                &bob,
                json!({"version":0,"milestone":milestone["number"]}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(assigned["milestone"]["number"], milestone["number"]);
    }
    let requests = format!("{root}/pulls/1/review-requests");
    let reviews = h.json(&requests, &bob).await;
    assert_eq!(reviews["can_manage"], true);
    let requested: Value = h
        .write(
            Method::PUT,
            &requests,
            &bob,
            json!({"version":reviews["version"],"change":{"kind":"request","subject":"bob-id"}}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(requested["state"]["kind"], "requested");
    for (path, body) in [
        (
            format!("{root}/labels"),
            json!({"request_id":uuid::Uuid::now_v7(),"name":"Not allowed","color":"abcdef"}),
        ),
        (
            format!("{root}/milestones"),
            json!({"request_id":uuid::Uuid::now_v7(),"title":"Not allowed"}),
        ),
        (
            "/api/git-token".to_string(),
            json!({"owner":"team","repository":"private","access":"write"}),
        ),
        (
            format!("{root}/pulls/1/merge"),
            json!({"request_id":uuid::Uuid::now_v7(),"version":1,"method":"fast_forward","base_oid":"a".repeat(40),"head_oid":"b".repeat(40)}),
        ),
    ] {
        assert_eq!(
            h.write(Method::POST, &path, &bob, body).await.status(),
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    members[1].access = crate::RepositoryAccess::Read;
    h.server
        .catalog()
        .unwrap()
        .set_members("team", "private", members, true)
        .await
        .unwrap();
    for kind in ["issues", "pulls"] {
        let path = format!("{root}/{kind}/1");
        let current = h.json(&path, &bob).await;
        assert_eq!(current["can_label"], false);
        assert_eq!(
            h.write(
                Method::PATCH,
                &path,
                &bob,
                json!({"version":current["version"],"state":"closed"})
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        h.json(&format!("{root}/issues/1"), &alice).await["title"],
        issue["title"]
    );
    h.close().await;
}

mod authorization;
