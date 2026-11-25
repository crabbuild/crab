use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};

use super::*;
use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::{
        git,
        http::{Harness, organizations},
    },
};

#[tokio::test(flavor = "multi_thread")]
async fn public_visibility_grants_anonymous_code_and_discussion_reads_until_revoked() {
    crate::test_support::server::init_test_tracing();
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "public-visibility").await;
    h.initialize_directory().await;
    let (organization, _, bob_id) = organizations::bootstrap(&h).await;
    let catalog = h.server.catalog().unwrap();
    catalog
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let org = format!("/api/organizations/{organization}");
    h.write(
        Method::PUT,
        &format!("{org}/invitations/{bob_id}"),
        &alice,
        json!({"version":0,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{org}/invitation"),
        &bob,
        json!({"version":1,"accept":true}),
    )
    .await
    .error_for_status()
    .unwrap();

    let create = async |name: &str| {
        h.write(
            Method::POST,
            &format!("{org}/repositories/creation"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()
    };
    let source = create("library").await;
    create("private-library").await;
    let owner = source["owner"].as_str().unwrap();
    let id = source["id"].as_str().unwrap();
    let git_path = format!("{}/git/{owner}/library.git", h.origin);
    let repo_path = format!("{}/api/repos/{owner}/library", h.origin);
    let endpoint = format!("/api/repositories/{id}/visibility");
    let lookup = format!("{}/api/repos?public=true&limit=10", h.origin);
    let confirm = format!("{owner}/library");
    let submit = |version, value| json!({"version":version,"value":value,"confirmation":confirm});

    catalog
        .set_members(
            owner,
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
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":owner,"repository":"library","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let checkout = tempfile::tempdir().unwrap();
    git::success(
        checkout.path(),
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(checkout.path().join("README.md"), "base\n").unwrap();
    git::success(checkout.path(), &["add", "README.md"]).await;
    git::success(checkout.path(), &["commit", "-m", "Add README"]).await;
    assert!(
        git::with_http_token(
            checkout.path(),
            token["token"].as_str().unwrap(),
            &["push", &git_path, "main"]
        )
        .await
    );
    git::success(checkout.path(), &["switch", "-c", "feature"]).await;
    std::fs::write(checkout.path().join("README.md"), "feature\n").unwrap();
    git::success(checkout.path(), &["commit", "-am", "Update README"]).await;
    assert!(
        git::with_http_token(
            checkout.path(),
            token["token"].as_str().unwrap(),
            &["push", &git_path, "feature"]
        )
        .await
    );
    let session = h.json("/api/session", &alice).await;
    let post = async |path: &str, body: Value| {
        h.http
            .post(format!("{repo_path}/{path}"))
            .header(reqwest::header::COOKIE, &alice)
            .header(reqwest::header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header("x-crab-repository-id", id)
            .json(&body)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    let issue = post(
        "issues",
        json!({
            "request_id": Uuid::now_v7(), "title": "Published issue", "body": "Issue details",
        }),
    )
    .await;
    let issue_path = format!("issues/{}", issue["number"]);
    post(
        &format!("{issue_path}/comments"),
        json!({
            "request_id": Uuid::now_v7(), "body": "A published reply",
        }),
    )
    .await;
    let pull = post(
        "pulls",
        json!({
            "request_id": Uuid::now_v7(), "title": "Published pull", "body": "Pull details",
            "base_ref": "refs/heads/main", "head_ref": "refs/heads/feature",
        }),
    )
    .await;
    let pull_path = format!("pulls/{}", pull["number"]);
    let get = async |path: &str| h.http.get(path).send().await.unwrap();
    assert_eq!(
        get(&format!("{repo_path}/refs")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&lookup).await.json::<Value>().await.unwrap()["repositories"],
        json!([])
    );
    assert_eq!(h.json(&endpoint, &bob).await["can_change"], false);
    assert_eq!(
        h.write(Method::PUT, &endpoint, &bob, submit(0, "public"))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.write(
            Method::PUT,
            &endpoint,
            &alice,
            json!({"version":0,"value":"public","confirmation":"library"})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    h.write(Method::PUT, &endpoint, &alice, submit(0, "public"))
        .await
        .error_for_status()
        .unwrap();

    let listing = get(&lookup)
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(listing["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(listing["repositories"][0]["id"], id);
    assert_eq!(listing["repositories"][0]["visibility"], "public");
    let detail = get(&format!(
        "{}?owner={owner}&name=library",
        lookup.split('?').next().unwrap()
    ))
    .await
    .error_for_status()
    .unwrap()
    .json::<Value>()
    .await
    .unwrap();
    assert_eq!(detail["repositories"][0]["public_read_only"], true);
    assert_eq!(detail["repositories"][0]["can_admin"], false);
    assert_eq!(detail["repositories"][0]["protected_branches"], json!([]));
    for path in ["refs", "tree", "archive?rev=refs%2Fheads%2Fmain"] {
        assert_eq!(
            get(&format!("{repo_path}/{path}")).await.status(),
            StatusCode::OK
        );
    }
    for path in [
        "settings/integrations",
        "settings/webhooks",
        &format!("{pull_path}/pending-review"),
    ] {
        assert_eq!(
            get(&format!("{repo_path}/{path}")).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let read = async |path: &str| {
        get(&format!("{repo_path}/{path}"))
            .await
            .error_for_status()
            .unwrap_or_else(|error| panic!("{path}: {error}"))
            .json::<Value>()
            .await
            .unwrap()
    };
    let attachment = h
        .http
        .post(format!("{repo_path}/discussion-attachments"))
        .query(&[
            ("id", Uuid::now_v7().to_string()),
            ("name", "notes.txt".into()),
        ])
        .header(reqwest::header::COOKIE, &alice)
        .header(reqwest::header::ORIGIN, &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", id)
        .header(reqwest::header::CONTENT_TYPE, "text/plain")
        .body("Public attachment contents")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let attachment_path = attachment["url"].as_str().unwrap();
    let attachment_url = format!("{}{attachment_path}", h.origin);
    assert_eq!(get(&attachment_url).await.status(), StatusCode::NOT_FOUND);
    post(
        &format!("{issue_path}/comments"),
        json!({
            "request_id": Uuid::now_v7(), "body": format!("[Notes]({attachment_path})"),
        }),
    )
    .await;
    assert_eq!(
        get(&attachment_url)
            .await
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap(),
        "Public attachment contents"
    );
    assert_eq!(read("issues").await["items"][0]["title"], "Published issue");
    assert_eq!(read("pulls").await["items"][0]["title"], "Published pull");
    assert_eq!(read(&issue_path).await["can_edit"], false);
    assert!(
        read(&format!("{issue_path}/conversation")).await["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["kind"] == "comment"
                    && item["value"]["body"] == "A published reply"
                    && item["value"]["can_edit"] == false
            })
    );
    let detail = read(&pull_path).await;
    assert_eq!(detail["branches_available"], true);
    assert_eq!(detail["can_edit"], false);
    assert_eq!(detail["can_merge"], false);
    assert!(detail["github_actions_ref"].is_null());
    read(&format!("{pull_path}/conversation")).await;
    read("discussion-search/assignees").await;
    read("labels").await;
    read("milestones").await;
    assert_eq!(
        h.http
            .post(format!("{repo_path}/{issue_path}/comments"))
            .json(&json!({"request_id":Uuid::now_v7(),"body":"Not published"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&format!("{git_path}/info/refs?service=git-receive-pack"))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let clone = tempfile::tempdir().unwrap();
    git::success(clone.path(), &["clone", &git_path, "."]).await;
    assert_eq!(
        std::fs::read_to_string(clone.path().join("README.md")).unwrap(),
        "base\n"
    );

    let policy = format!("{org}/repository-policy");
    let mut update = h.json(&policy, &alice).await;
    update["member_visibility"] = json!(true);
    h.write(Method::PUT, &policy, &alice, update)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.write(Method::PUT, &endpoint, &bob, submit(0, "private"))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    h.write(Method::PUT, &endpoint, &bob, submit(1, "private"))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        get(&format!("{repo_path}/refs")).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&format!("{repo_path}/{issue_path}/conversation"))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(get(&attachment_url).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        get(&lookup).await.json::<Value>().await.unwrap()["repositories"],
        json!([])
    );
    assert_eq!(
        get(&format!("{git_path}/info/refs?service=git-upload-pack"))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.json(
            &format!("{org}/audit-log?action=repository_visibility_changed"),
            &alice
        )
        .await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    h.close().await;
}
