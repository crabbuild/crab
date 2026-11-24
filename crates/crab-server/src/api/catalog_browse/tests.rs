use axum::http::{StatusCode, header};
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::http::{Harness, browser::Browser},
};

async fn get(h: &Harness, cookie: &str, path: &str) -> reqwest::Response {
    h.http
        .get(format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn personal_repository_browse_uses_stable_ownership_and_current_viewer_access() {
    let h = Harness::new(false).await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &alice,
            json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let actor = profile["id"].as_str().unwrap();
    let public = format!("/api/actors/{actor}/repositories");
    let organization_creation = format!("/api/organizations/{organization}/repositories/creation");
    let mut owner = String::new();
    for (path, name) in [
        ("/api/repos/creation", "personal-a"),
        ("/api/repos/creation", "personal-b"),
        (&organization_creation, "shared"),
    ] {
        let repo: Value = h
            .write(
                Method::POST,
                path,
                &alice,
                json!({"request_id":Uuid::now_v7(),"name":name,"description":""}),
            )
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        if name == "personal-a" {
            owner = repo["owner"].as_str().unwrap().to_owned();
        }
    }
    let personal = h.json("/api/repos/personal?limit=1", &alice).await;
    assert_eq!(personal["repositories"][0]["name"], "personal-a");
    let cursor = personal["next"].as_str().unwrap();
    assert_eq!(
        h.json(&format!("/api/repos/personal?after={cursor}"), &alice)
            .await["repositories"][0]["name"],
        "personal-b"
    );
    assert_eq!(
        get(&h, &alice, &format!("/api/repos?after={cursor}"))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.json(&public, &alice).await["repositories"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    h.write(
        Method::PATCH,
        "/api/profile",
        &alice,
        json!({"version":profile["version"],"handle":"alice-new","name":"Alice"}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(h.json(&public, &bob).await["repositories"], json!([]));
    let catalog = h.server.catalog().unwrap();
    let repo = catalog.membership(&owner, "personal-a").await.unwrap();
    let mut members = repo.members;
    members.push(RepositoryMember {
        subject: "bob-id".into(),
        name: "Bob".into(),
        access: RepositoryAccess::Read,
    });
    catalog
        .set_members(&owner, "personal-a", members, true)
        .await
        .unwrap();
    assert_eq!(
        h.json(&public, &bob).await["repositories"][0]["name"],
        "personal-a"
    );
    assert_eq!(
        h.json("/api/repos/personal", &bob).await["repositories"],
        json!([])
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_creates_in_personal_scope_and_opens_the_profile_repository() {
    let h = Harness::new(false).await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let alice = h.login().await;
    let profile: Value = h
        .write(
            Method::POST,
            "/api/profile",
            &alice,
            json!({"request_id":Uuid::now_v7(),"handle":"alice","name":"Alice"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let profile_url = format!("{}/-/users/{}", h.origin, profile["id"].as_str().unwrap());
    let browser = Browser::new("personal-repositories");
    browser.run(&["open", &h.origin]).await;
    browser.sign_in().await;
    browser
        .run(&["wait", "a[href='/-/repositories/personal']"])
        .await;
    browser
        .run(&[
            "find", "role", "link", "click", "--name", "Personal", "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "You don’t have any personal repositories yet.",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "New repository",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "#new-repository-name"]).await;
    browser
        .run(&["find", "label", "Repository name", "fill", "alice-notes"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create repository",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "This repository is empty"])
        .await;
    let personal = h.json("/api/repos/personal", &alice).await;
    assert_eq!(personal["repositories"][0]["name"], "alice-notes");

    browser.run(&["open", &profile_url]).await;
    browser
        .run(&[
            "wait",
            "nav[aria-label='User profile'] a[href$='/repositories']",
        ])
        .await;
    browser
        .run(&[
            "click",
            "nav[aria-label='User profile'] a[href$='/repositories']",
        ])
        .await;
    browser
        .run(&["wait", "--fn", "document.querySelector('ul[aria-label=\"Repository results\"]')?.textContent.trim() === 'alice-notes'"])
        .await;
    drop(browser);
    h.close().await;
}

async fn revoke_bob(h: &Harness, name: &str) {
    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            name,
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn topic_search_binds_the_cursor_to_current_authorized_matches() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    h.add_repository("project-notes").await;
    h.add_repository("project-secret").await;
    revoke_bob(&h, "project-secret").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let id = |name: &str| {
        h.server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap()
            .id
    };
    let topics = |name: &str| format!("/api/repositories/{}/topics", id(name));
    for (name, names) in [
        ("private", json!(["Release-Team", "docs", "docs"])),
        ("project-notes", json!(["release-team"])),
        ("project-secret", json!(["release-team", "secret-only"])),
    ] {
        h.write(
            Method::PUT,
            &topics(name),
            &alice,
            json!({"version":0,"names":names}),
        )
        .await
        .error_for_status()
        .unwrap();
    }
    let canonical = json!({"version":1,"names":["docs","release-team"]});
    let recovered = h
        .write(
            Method::PUT,
            &topics("private"),
            &alice,
            json!({"version":0,"names":["RELEASE-TEAM","docs"]}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(recovered, canonical);
    assert_eq!(h.json(&topics("private"), &bob).await, canonical);
    assert_eq!(
        get(&h, &bob, &topics("project-secret")).await.status(),
        StatusCode::NOT_FOUND
    );
    for (account, body, expected) in [
        (
            &bob,
            json!({"version":1,"names":["forbidden"]}),
            StatusCode::NOT_FOUND,
        ),
        (
            &alice,
            json!({"version":0,"names":["changed"]}),
            StatusCode::CONFLICT,
        ),
        (
            &alice,
            json!({"version":1,"names":["not valid"]}),
            StatusCode::BAD_REQUEST,
        ),
        (
            &alice,
            json!({"version":1,"names":vec!["topic"; 21]}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        assert_eq!(
            h.write(Method::PUT, &topics("private"), account, body)
                .await
                .status(),
            expected
        );
    }
    assert_eq!(
        h.json("/api/repos?owner=team&name=private", &bob).await["repositories"][0]["topics"],
        canonical["names"]
    );
    let first_path = "/api/repos?q=topic%3ARELEASE-TEAM%20topics%3A1..2&limit=1";
    let first = h.json(first_path, &bob).await;
    assert_eq!(first["repositories"][0]["name"], "private");
    let cursor = first["next"].as_str().unwrap();
    let path = format!("{first_path}&after={cursor}");
    let next = h.json(&path, &bob).await;
    assert_eq!(next["repositories"][0]["name"], "project-notes");
    assert_eq!(next["next"], Value::Null);
    assert_eq!(
        h.json(
            "/api/repos?q=topic%3Arelease-team%20-topic%3Adocs%20topics%3A%3C2",
            &bob
        )
        .await["repositories"][0]["name"],
        "project-notes"
    );
    assert_eq!(
        h.json("/api/repos?q=topic%3Asecret-only", &bob).await["repositories"],
        json!([])
    );
    assert_eq!(
        get(
            &h,
            &bob,
            &format!("/api/repos?q=topic%3Adocs&after={cursor}")
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    // Use the shared catalog without installing it into this serving peer.
    // A continuation must still observe a change to the eligible topic set.
    let catalog = h.server.catalog().unwrap();
    let (before, _) = catalog.load().await.unwrap();
    let published = catalog
        .change_topics(
            before.version,
            id("project-notes"),
            crate::catalog::topics::Topics {
                names: Vec::new(),
                version: 1,
            },
        )
        .await
        .unwrap();
    assert!(h.server.repositories.version() < published.version);
    assert_eq!(get(&h, &bob, &path).await.status(), StatusCode::CONFLICT);
    let empty = h.json("/api/repos?q=topics%3A0", &bob).await;
    assert_eq!(empty["repositories"][0]["name"], "project-notes");
    assert!(empty["repositories"][0].get("topics").is_none());
    h.close().await;
}

#[tokio::test]
async fn pages_rank_matches_and_bind_continuations_to_account_query_and_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    h.add_repository("project-notes").await;
    h.add_repository("project-secret").await;
    revoke_bob(&h, "project-secret").await;
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version": 0, "archived": true, "repository": "team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;

    let first = h.json("/api/repos?q=PROJECT&limit=1", &bob).await;
    assert_eq!(first["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(first["repositories"][0]["name"], "project-notes");
    assert!(first["repositories"][0].get("protected_branches").is_none());
    let cursor = first["next"].as_str().unwrap();
    let path = format!("/api/repos?q=project&limit=1&after={cursor}");
    let second = h.json(&path, &bob).await;
    assert_eq!(second["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(second["repositories"][0]["name"], "private");
    assert_eq!(second["repositories"][0]["archived"], true);
    assert_eq!(second["next"], Value::Null);
    for (path, account) in [
        (path.clone(), &alice),
        (format!("/api/repos?q=notes&limit=1&after={cursor}"), &bob),
        (
            format!(
                "/api/repos?organization={}&q=project&after={cursor}",
                Uuid::now_v7()
            ),
            &bob,
        ),
        (format!("/api/repos?q=project&after={cursor}x"), &bob),
        ("/api/repos?q=project&limit=0".to_owned(), &bob),
    ] {
        assert_eq!(
            get(&h, account, &path).await.status(),
            StatusCode::BAD_REQUEST
        );
    }

    let absent = h.json("/api/repos?q=secret", &bob).await;
    assert_eq!(absent, json!({"repositories":[],"next":null}));
    let qualified = h.json("/api/repos?q=TEAM%2FPRIVATE", &bob).await;
    assert_eq!(qualified["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(qualified["repositories"][0]["name"], "private");
    revoke_bob(&h, "project-notes").await;
    assert_eq!(get(&h, &bob, &path).await.status(), StatusCode::CONFLICT);
    let refreshed = h.json("/api/repos?q=project&limit=1", &bob).await;
    assert_eq!(refreshed["repositories"][0]["name"], "private");
    assert_eq!(refreshed["next"], Value::Null);

    let exact = h.json("/api/repos?owner=team&name=private", &bob).await;
    assert_eq!(
        exact["repositories"][0]["protected_branches"],
        json!([{"branch":"main","required_approvals":1,"required_checks":["ci/test"]}])
    );
    assert!(exact.get("next").is_none());
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_filter_checks_current_state_after_the_cursor_is_issued() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    h.add_repository("project-notes").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let start = "/api/repos?q=archived:false&limit=1";
    let first = h.json(start, &bob).await;
    assert_eq!(first["repositories"][0]["name"], "private");
    let cursor = first["next"].as_str().unwrap();
    let next = format!("{start}&after={cursor}");
    let second = h.json(&next, &bob).await;
    assert_eq!(second["repositories"][0]["name"], "project-notes");
    assert_eq!(second["next"], Value::Null);

    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(get(&h, &bob, &next).await.status(), StatusCode::CONFLICT);
    let archived = h.json("/api/repos?q=private%20ARCHIVED:TRUE", &bob).await;
    assert_eq!(archived["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(archived["repositories"][0]["archived"], true);
    let active = h.json("/api/repos?q=-archived:true", &bob).await;
    assert_eq!(active["repositories"].as_array().unwrap().len(), 1);
    assert_eq!(active["repositories"][0]["name"], "project-notes");
    for invalid in [
        "archived:yes",
        "archived:true%20archived:false",
        "fork:unknown",
    ] {
        assert_eq!(
            get(&h, &bob, &format!("/api/repos?q={invalid}"))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built React UI with the native catalog"]
async fn native_browser_searches_and_opens_an_authorized_repository() {
    let h = Harness::new(false).await;
    h.add_repository("project-notes").await;
    h.add_repository("project-secret").await;
    revoke_bob(&h, "project-secret").await;
    let alice = h.login().await;
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let browser = Browser::new("repository-search");
    browser.run(&["open", &h.origin]).await;
    browser.sign_in().await;
    browser.run(&["wait", "--text", "Your repositories"]).await;
    browser
        .run(&["wait", "select[aria-label='Archive status']"])
        .await;
    browser
        .run(&["select", "select[aria-label='Archive status']", "true"])
        .await;
    browser
        .run(&["wait", "--url", "**/?q=archived%3Atrue"])
        .await;
    browser
        .run(&[
            "wait",
            "[aria-label='Repository results'] a[href='/team/private']",
        ])
        .await;
    browser
        .run(&["fill", "input[name=q]", "TEAM/project"])
        .await;
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Search", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/?q=TEAM%2Fproject"])
        .await;
    browser.run(&["wait", "--text", "project-notes"]).await;
    let result = browser.run(&["snapshot"]).await;
    assert!(!result.contains("project-secret"));
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "team / project-notes",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**/team/project-notes"])
        .await;
    browser.run(&["wait", "--text", "Code"]).await;
    browser.run(&["close"]).await;
    h.close().await;
}
