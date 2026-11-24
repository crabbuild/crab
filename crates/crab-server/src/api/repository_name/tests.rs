use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    catalog::{Rename, RepositoryApplicationState},
    test_support::{
        git,
        http::{Harness, browser::Browser},
    },
};

const OLD: &str = "/api/repos/team/private";
const NEW: &str = "/api/repos/team/renamed";

async fn ready_harness() -> Harness {
    crate::test_support::server::init_test_tracing();
    let store =
        crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "repository-rename").await;
    h.initialize_directory().await;
    let id = h.server.repositories.values()[0].id;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(id)
        .await
        .unwrap();
    h
}

fn change(name: &str, version: u32) -> Value {
    json!({"name":name,"version":version,"request_id":Uuid::now_v7()})
}

async fn get(h: &Harness, path: &str, cookie: &str) -> reqwest::Response {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
}

async fn remote(h: &Harness, cookie: &str) -> url::Url {
    let token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            cookie,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = url::Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(token["token"].as_str()).unwrap();
    remote
}

#[tokio::test]
async fn rename_keeps_native_git_and_issues_and_replays_the_same_request() {
    let h = ready_harness().await;
    let alice = h.login().await;
    let old = h.server.repositories.values()[0].clone();
    let catalog = h.server.catalog().unwrap();
    let (before, _) = catalog.load().await.unwrap();
    let placement = crate::repository::catalog::materialize_record(
        &catalog,
        before.repository_by_id(old.id).unwrap(),
    )
    .await
    .unwrap();
    let settings = format!("/api/repositories/{}/name", old.id);
    let url = remote(&h, &alice).await;
    let checkout = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(checkout.path(), url.as_str()).await;
    h.write(
        Method::POST,
        &format!("{OLD}/issues"),
        &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Keep this issue","body":"Same repository"}),
    )
    .await
    .error_for_status()
    .unwrap();

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.write(Method::PUT, &settings, &bob, change("renamed", 0))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(h.json(&settings, &alice).await["version"], 0);
    let submission = change("renamed", 0);
    let lost = h
        .write(Method::PUT, &settings, &alice, submission.clone())
        .await;
    assert_eq!(lost.status(), StatusCode::OK);
    drop(lost);
    let response: Value = h
        .write(Method::PUT, &settings, &alice, submission)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        response,
        json!({"id":old.id,"owner":"team","name":"renamed","version":1})
    );
    let current = h.server.repositories.by_id(old.id).unwrap();
    assert_eq!(current.identity, placement.identity);
    assert_eq!(current.config.prefix, placement.config.prefix);

    let new = h.json(&format!("{NEW}/issues/1"), &bob).await;
    assert_eq!(new["title"], "Keep this issue");
    let old_api = get(&h, &format!("{OLD}/issues/1"), &bob).await;
    assert_eq!(old_api.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        old_api.headers()[header::LOCATION],
        format!("{NEW}/issues/1")
    );
    let old_web = get(&h, "/team/private/issues/1?mode=review", &bob).await;
    assert_eq!(old_web.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        old_web.headers()[header::LOCATION],
        "/team/renamed/issues/1?mode=review"
    );
    let exact = h.json("/api/repos?owner=team&name=private", &bob).await;
    assert_eq!(
        exact["redirect"],
        json!({"id":old.id,"owner":"team","name":"renamed"})
    );
    let stale_write = h
        .write(
            Method::POST,
            &format!("{OLD}/issues"),
            &alice,
            json!({"request_id":Uuid::now_v7(),"title":"Stale write","body":""}),
        )
        .await;
    assert_eq!(stale_write.status(), StatusCode::NOT_FOUND);

    let refs = git::success(
        checkout.path(),
        &["ls-remote", url.as_str(), "refs/heads/feature"],
    )
    .await;
    assert_eq!(refs, format!("{head}\trefs/heads/feature"));
    let clone = tempfile::tempdir().unwrap();
    git::success(clone.path(), &["clone", url.as_str(), "."]).await;
    git::success(
        checkout.path(),
        &["push", url.as_str(), "HEAD:refs/heads/after-rename"],
    )
    .await;
    let mut canonical = url.clone();
    canonical.set_path("/git/team/renamed.git");
    let refs = git::success(
        checkout.path(),
        &["ls-remote", canonical.as_str(), "refs/heads/after-rename"],
    )
    .await;
    assert_eq!(refs, format!("{head}\trefs/heads/after-rename"));

    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "renamed",
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    assert_eq!(
        get(&h, &format!("{OLD}/issues/1"), &bob).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.json("/api/repos?owner=team&name=private", &bob).await,
        json!({"repositories":[]})
    );
    h.close().await;
}

#[tokio::test]
async fn stale_peer_recovers_the_canonical_route_and_name_reuse_releases_the_alias() {
    let h = ready_harness().await;
    let alice = h.login().await;
    let old_remote = remote(&h, &alice).await;
    let catalog = h.server.catalog().unwrap();
    let original = h.server.repositories.values()[0].id;
    let (document, _) = catalog.load().await.unwrap();
    let renamed = catalog
        .rename_repository(
            document.version,
            Rename {
                repository: original,
                expected_version: 0,
                request: Uuid::now_v7(),
                name: "renamed".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        h.server
            .repositories
            .get(&("team".into(), "renamed".into()))
            .is_none()
    );
    let alias = get(&h, &format!("{OLD}/refs"), &alice).await;
    assert_eq!(alias.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(h.server.repositories.version(), renamed.version);
    assert_eq!(
        h.json("/api/repos?owner=team&name=renamed", &alice).await["repositories"][0]["id"],
        json!(original)
    );

    let settings = format!("/api/repositories/{original}/name");
    add_repository(&h, &catalog, "occupied", "new-placement").await;
    assert_eq!(
        h.write(Method::PUT, &settings, &alice, change("occupied", 1))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.write(Method::PUT, &settings, &alice, change("stale", 0))
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let replacement = add_repository(&h, &catalog, "private", "replacement-placement").await;
    let exact = h.json("/api/repos?owner=team&name=private", &alice).await;
    assert_eq!(exact["repositories"][0]["id"], json!(replacement));
    assert!(exact.get("redirect").is_none());
    let replacement_url = format!("{OLD}/refs");
    assert_eq!(
        get(&h, &replacement_url, &alice).await.status(),
        StatusCode::OK
    );
    let rebound = h
        .http
        .get(format!(
            "{}/git/team/private.git/info/refs?service=git-upload-pack",
            h.origin
        ))
        .header("git-protocol", "version=2")
        .basic_auth("crab", old_remote.password())
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::NOT_FOUND);
    h.close().await;
}

async fn add_repository(
    h: &Harness,
    catalog: &crate::catalog::CatalogStore,
    name: &str,
    prefix: &str,
) -> Uuid {
    let record = catalog
        .create_repository(
            "team".into(),
            name.into(),
            prefix.into(),
            "main".into(),
            String::new(),
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
        )
        .await
        .unwrap();
    h.server
        .initialize_catalog_repository(record.id, RepositoryApplicationState::EmptyCellPending)
        .await
        .unwrap();
    let (document, _) = catalog.load().await.unwrap();
    h.server.install_catalog(document).await.unwrap();
    record.id
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn native_browser_renames_a_repository_from_general_settings() {
    let h = ready_harness().await;
    let browser = Browser::new("repository-rename");
    browser
        .run(&["open", &format!("{}/team/private/settings", h.origin)])
        .await;
    let click = async |name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    browser.sign_in().await;
    click("Rename repository").await;
    browser
        .run(&["wait", "--text", "New repository name"])
        .await;
    browser
        .run(&["fill", "input[name=repository-name]", "renamed"])
        .await;
    click("Save name").await;
    browser
        .run(&["wait", "--url", "**/team/renamed/settings"])
        .await;
    browser.run(&["wait", "--text", "Repository name"]).await;
    browser
        .run(&["open", &format!("{}/team/private/settings", h.origin)])
        .await;
    browser
        .run(&["wait", "--url", "**/team/renamed/settings"])
        .await;
    browser.run(&["close"]).await;
    h.close().await;
}
