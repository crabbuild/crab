use axum::http::StatusCode;
use reqwest::Method;
use serde_json::{Value, json};

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::http::{Harness, browser::Browser},
};

const WATCHED: &str = "/api/notifications/watched";
const PRIVATE: &str = "/api/repos/team/private/subscription";

async fn set(h: &Harness, cookie: &str, name: &str, preference: Value) {
    let repo = h
        .server
        .repositories
        .get(&("team".into(), name.into()))
        .unwrap();
    let session = h.json("/api/session", cookie).await;
    h.http
        .put(format!("{}/api/repos/team/{name}/subscription", h.origin))
        .header("cookie", cookie)
        .header("origin", &h.origin)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .header("x-crab-repository-id", repo.id.to_string())
        .json(&preference)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

#[tokio::test]
async fn watched_repositories_include_release_only_and_exclude_other_accounts_and_revoked_access() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    h.add_repository("muted").await;
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let empty = json!({"items":[],"next":null});

    set(&h, &alice, "library", json!({"mode":"issues","version":0})).await;
    set(&h, &bob, "muted", json!({"mode":"muted","version":0})).await;
    let preference = json!({"mode":"direct","version":1,"releases":true});
    set(
        &h,
        &bob,
        "private",
        json!({"mode":"direct","version":0,"releases":true}),
    )
    .await;

    assert_eq!(
        h.json(&format!("{WATCHED}?limit=1"), &bob).await,
        json!({"items":[],"next":"team/library"})
    );
    let after = format!("{WATCHED}?after=team%2Flibrary&limit=1");
    assert_eq!(
        h.json(&after, &bob).await,
        json!({"items":[],"next":"team/muted"})
    );
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let release_watch = json!({
        "items":[{"repository":{"id":repo.id,"owner":"team","name":"private"},
            "preference":preference}],
        "next":null
    });
    assert_eq!(h.json(WATCHED, &bob).await, release_watch);
    assert_eq!(
        h.json(&format!("{WATCHED}?after=team%2Fmuted&limit=1"), &bob)
            .await,
        release_watch
    );
    assert_eq!(
        h.json(WATCHED, &alice).await["items"][0]["repository"]["name"],
        "library"
    );
    assert_eq!(
        h.json(&format!("{WATCHED}?after=team%2Flibrary"), &alice)
            .await,
        empty
    );
    assert_eq!(
        h.http
            .get(format!("{}{WATCHED}?after=team/private/extra", h.origin))
            .header("cookie", &bob)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );

    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "private",
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    assert_eq!(h.json(WATCHED, &bob).await, empty);
    assert_eq!(h.json(&after, &bob).await, empty);
    h.close().await;
}

#[tokio::test]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_edits_and_stops_watching_from_notifications() {
    let h = Harness::new(false).await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    h.write(
        Method::PUT,
        PRIVATE,
        &bob,
        json!({"mode":"direct","version":0,"releases":true}),
    )
    .await
    .error_for_status()
    .unwrap();

    let browser = Browser::new("watched-repositories");
    browser
        .run(&["open", &format!("{}/-/notifications", h.origin)])
        .await;
    let click = async |role: &str, name: &str| {
        browser
            .run(&["find", "role", role, "click", "--name", name, "--exact"])
            .await;
    };
    browser.sign_in().await;
    browser
        .run(&["wait", "--text", "Watched repositories"])
        .await;
    click("link", "Watched repositories").await;
    browser.run(&["wait", "--text", "New releases"]).await;
    click("button", "Watch team/private").await;
    browser.run(&["wait", "[role=dialog] select"]).await;
    browser
        .run(&["select", "[role=dialog] select", "issues"])
        .await;
    click("button", "Save preference").await;
    browser
        .run(&["wait", "--text", "Issue updates · New releases"])
        .await;
    assert_eq!(
        h.json(PRIVATE, &bob).await,
        json!({"mode":"issues","version":2,"releases":true})
    );

    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "Issue updates · New releases"])
        .await;
    click("button", "Watch team/private").await;
    browser.run(&["wait", "[role=dialog] select"]).await;
    browser
        .run(&["select", "[role=dialog] select", "direct"])
        .await;
    click("checkbox", "New releases").await;
    click("button", "Save preference").await;
    browser
        .run(&["wait", "--text", "You aren’t watching any repositories."])
        .await;
    assert_eq!(
        h.json(PRIVATE, &bob).await,
        json!({"mode":"direct","version":3})
    );
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "You aren’t watching any repositories."])
        .await;
    drop(browser);
    h.close().await;
}

const UNWATCH: &str = "/api/notifications/watched/unwatch";

#[tokio::test]
async fn bulk_unwatch_preserves_newer_choices_and_recovers_committed_versions() {
    let h = Harness::new(false).await;
    for name in ["library", "changed", "muted"] {
        h.add_repository(name).await;
    }
    let alice = h.login().await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let id = |name: &str| {
        h.server
            .repositories
            .get(&("team".into(), name.into()))
            .unwrap()
            .id
    };
    set(&h, &alice, "library", json!({"mode":"issues","version":0})).await;
    for name in ["library", "private", "changed", "muted"] {
        set(
            &h,
            &bob,
            name,
            json!({"mode":"issues","version":0,"releases":true}),
        )
        .await;
    }
    let after = h.json(&format!("{WATCHED}?owner=team&limit=1"), &bob).await;
    assert_eq!(after["next"], "team/changed");
    assert_eq!(after["items"][0]["repository"]["name"], "changed");
    assert_eq!(
        h.json(&format!("{WATCHED}?owner=another"), &bob).await,
        json!({"items":[],"next":null})
    );
    set(
        &h,
        &bob,
        "changed",
        json!({"mode":"pull_requests","version":1}),
    )
    .await;
    set(&h, &bob, "muted", json!({"mode":"muted","version":1})).await;
    h.server
        .catalog()
        .unwrap()
        .set_members(
            "team",
            "private",
            vec![RepositoryMember {
                subject: "alice-id".into(),
                name: "Alice".into(),
                access: RepositoryAccess::Admin,
            }],
            true,
        )
        .await
        .unwrap();
    let item = |name: &str| json!({"id":id(name),"version":1});
    let batch = json!({
        "owner":"team",
        "items":[item("library"),item("changed"),item("private"),item("muted")]
    });
    let submit = async |body: Value| h.write(Method::POST, UNWATCH, &bob, body).await;

    assert_eq!(
        h.http
            .post(format!("{}{UNWATCH}", h.origin))
            .header("cookie", &bob)
            .header("origin", &h.origin)
            .json(&batch)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        submit(json!({"owner":"team","items":[item("library"),item("library")]}))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let wrong_owner: Value = submit(json!({"owner":"another","items":[item("library")]}))
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        wrong_owner,
        json!({"items":[{"id":id("library"),"status":"unavailable"}]})
    );

    let expected = json!({"items":[
        {"id":id("library"),"status":"unwatched"},
        {"id":id("changed"),"status":"changed"},
        {"id":id("private"),"status":"unavailable"},
        {"id":id("muted"),"status":"changed"}
    ]});
    for _ in 0..2 {
        let response: Value = submit(batch.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response, expected);
    }
    const LIBRARY: &str = "/api/repos/team/library/subscription";
    assert_eq!(
        h.json(LIBRARY, &bob).await,
        json!({"mode":"direct","version":2})
    );
    assert_eq!(
        h.json(LIBRARY, &alice).await,
        json!({"mode":"issues","version":1})
    );
    set(&h, &bob, "library", json!({"mode":"issues","version":2})).await;
    let response: Value = submit(batch)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["items"][0]["status"], "changed");
    assert_eq!(
        h.json(LIBRARY, &bob).await,
        json!({"mode":"issues","version":3})
    );
    h.close().await;
}

#[tokio::test]
#[ignore = "requires agent-browser and the built embedded UI; uses isolated real OIDC/native Cells"]
async fn native_browser_confirms_bulk_unwatch_for_an_owner() {
    let h = Harness::new(false).await;
    h.add_repository("library").await;
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    set(
        &h,
        &bob,
        "private",
        json!({"mode":"direct","version":0,"releases":true}),
    )
    .await;
    set(
        &h,
        &bob,
        "library",
        json!({"mode":"issues_and_pull_requests","version":0}),
    )
    .await;

    let browser = Browser::new("bulk-unwatch");
    browser
        .run(&["open", &format!("{}/-/notifications/watched", h.origin)])
        .await;
    let click = async |name: &str| {
        browser.run(&["wait", "--text", name]).await;
        browser
            .run(&["find", "role", "button", "click", "--name", name, "--exact"])
            .await;
    };
    browser.sign_in().await;
    browser.run(&["wait", "--text", "team/library"]).await;
    click("Unwatch all").await;
    browser
        .run(&["wait", "--text", "2 watched repositories owned by team."])
        .await;
    assert_eq!(
        h.json(WATCHED, &bob).await["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    click("Unwatch 2 repositories").await;
    browser
        .run(&["wait", "--text", "Stopped watching 2 repositories."])
        .await;
    for name in ["private", "library"] {
        assert_eq!(
            h.json(&format!("/api/repos/team/{name}/subscription"), &bob)
                .await,
            json!({"mode":"direct","version":2})
        );
    }
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Close", "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "You aren’t watching any repositories."])
        .await;
    browser.run(&["reload"]).await;
    browser
        .run(&["wait", "--text", "You aren’t watching any repositories."])
        .await;
    drop(browser);
    h.close().await;
}
