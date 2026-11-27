use serde_json::json;

use crate::{
    RepositoryAccess, RepositoryMember,
    test_support::http::{Harness, browser::Browser},
};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI against the native catalog"]
async fn native_browser_maintainer_edits_topics_description_and_merge_options() {
    let store =
        crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "repository-maintainer").await;
    h.initialize_directory().await;
    let repo = h.server.repositories.values()[0].id;
    let catalog = h.server.catalog().unwrap();
    catalog.mark_cell_ready(repo).await.unwrap();
    catalog
        .set_members(
            "team",
            "private",
            vec![
                RepositoryMember {
                    subject: "alice-id".into(),
                    name: "Alice".into(),
                    access: RepositoryAccess::Admin,
                },
                RepositoryMember {
                    subject: "bob-id".into(),
                    name: "Bob".into(),
                    access: RepositoryAccess::Maintain,
                },
            ],
            true,
        )
        .await
        .unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let browser = Browser::new("repository-maintainer");
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
    click("Edit description").await;
    browser.run(&["wait", "--text", "Save description"]).await;
    browser
        .run(&[
            "find",
            "label",
            "Description",
            "fill",
            "Maintained in the browser",
            "--exact",
        ])
        .await;
    click("Save description").await;
    browser
        .run(&["wait", "--text", "Maintained in the browser"])
        .await;
    assert_eq!(
        h.json(&format!("/api/repositories/{repo}/description"), &bob)
            .await,
        json!({"description":"Maintained in the browser","version":1})
    );
    let page = browser.run(&["snapshot", "-i"]).await;
    assert!(!page.contains("Allow merge queue"));
    assert!(!page.contains("Automatically delete head branches"));
    assert_eq!(
        h.write(
            reqwest::Method::PUT,
            "/api/repos/team/private/settings/merge-methods",
            &bob,
            json!({"expected_version":0,"enabled":["merge_commit"],"delete_head_branches":true})
        )
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap()["error"]["code"],
        "branch_cleanup_settings_permission"
    );
    assert!(!page.contains("Rename repository"));
    assert!(!page.contains("Archive repository"));
    browser
        .run(&[
            "find",
            "role",
            "checkbox",
            "click",
            "--name",
            "Allow auto-merge",
        ])
        .await;
    click("Save merge methods").await;
    browser.run(&["wait", "--fn", "Array.from(document.querySelectorAll('button')).some(button => button.textContent === 'Save merge methods' && button.disabled)"]).await;
    assert_eq!(
        h.json("/api/repos/team/private/settings/merge-methods", &bob)
            .await["auto_merge"],
        true
    );
    click("Edit topics").await;
    browser.run(&["wait", "--text", "New topic"]).await;
    browser
        .run(&[
            "find",
            "label",
            "New topic",
            "fill",
            "Release-Team",
            "--exact",
        ])
        .await;
    browser.run(&["press", "Enter"]).await;
    browser
        .run(&["wait", "[aria-label='Remove topic release-team']"])
        .await;
    click("Save topics").await;
    browser
        .run(&["wait", "a[href='/?q=topic%3Arelease-team']"])
        .await;
    assert_eq!(
        h.json(&format!("/api/repositories/{repo}/topics"), &bob)
            .await,
        json!({"names":["release-team"],"version":1})
    );
    browser
        .run(&["open", &format!("{}/team/private", h.origin)])
        .await;
    browser
        .run(&["wait", "a[href='/?q=topic%3Arelease-team']"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "release-team",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--url", "**q=topic%3Arelease-team"])
        .await;
    browser
        .run(&[
            "wait",
            "[aria-label='Repository results'] a[href='/team/private']",
        ])
        .await;
    browser.run(&["close"]).await;
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repository_attributes_round_trip_validate_conflict_and_publish_to_catalog() {
    let store =
        crab_storage::Store::new(std::sync::Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "repository-attributes").await;
    h.initialize_directory().await;
    let repo = h.server.repositories.values()[0].id;
    let catalog = h.server.catalog().unwrap();
    catalog.mark_cell_ready(repo).await.unwrap();
    let alice = h.login().await;
    let endpoint = format!("/api/repositories/{repo}/attributes");
    assert_eq!(h.json(&endpoint, &alice).await, json!({"version": 0}));

    for invalid in [
        json!({"homepage": "ftp://crab.build", "license": "MIT", "is_template": false, "version": 0}),
        json!({"homepage": "https://crab.build/with space", "license": "MIT", "is_template": false, "version": 0}),
        json!({"homepage": "", "license": "MIT\n", "is_template": false, "version": 0}),
        json!({"homepage": "", "license": "MIT!", "is_template": false, "version": 0}),
    ] {
        assert_eq!(
            h.write(reqwest::Method::PUT, &endpoint, &alice, invalid)
                .await
                .status(),
            reqwest::StatusCode::BAD_REQUEST
        );
    }

    let saved: serde_json::Value = h
        .write(
            reqwest::Method::PUT,
            &endpoint,
            &alice,
            json!({
                "homepage": " https://crab.build/docs ",
                "license": " Apache-2.0 ",
                "is_template": true,
                "version": 0
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        saved,
        json!({
            "homepage": "https://crab.build/docs",
            "license": "Apache-2.0",
            "is_template": true,
            "version": 1
        })
    );

    let conflict = h
        .write(
            reqwest::Method::PUT,
            &endpoint,
            &alice,
            json!({
                "homepage": "https://crab.build",
                "license": "MIT",
                "is_template": false,
                "version": 0
            }),
        )
        .await;
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(
        conflict.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "repository_attributes_changed"
    );

    let detail = &h.json("/api/repos?owner=team&name=private", &alice).await["repositories"][0];
    assert_eq!(detail["homepage"], "https://crab.build/docs");
    assert_eq!(detail["license"], "Apache-2.0");
    assert_eq!(detail["is_template"], true);

    let browse = &h.json("/api/repos?q=private", &alice).await["repositories"][0];
    assert_eq!(browse["homepage"], "https://crab.build/docs");
    assert_eq!(browse["license"], "Apache-2.0");
    assert_eq!(browse["is_template"], true);

    h.write(
        reqwest::Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version": 0, "archived": true, "repository": "team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let archived = h
        .write(
            reqwest::Method::PUT,
            &endpoint,
            &alice,
            json!({
                "homepage": "https://crab.build/changed",
                "license": "MIT",
                "is_template": false,
                "version": 1
            }),
        )
        .await;
    assert_eq!(archived.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(
        archived.json::<serde_json::Value>().await.unwrap()["error"]["code"],
        "repository_archived"
    );
    h.close().await;
}
