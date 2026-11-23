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
    click("Continue to sign in").await;
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
