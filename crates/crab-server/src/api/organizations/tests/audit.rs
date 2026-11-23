use super::{repository_access::AccessFixture, *};
use crate::test_support::http::browser::Browser;

#[tokio::test(flavor = "multi_thread")]
async fn organization_audit_filters_and_exports_committed_events_for_owners() {
    let f = AccessFixture::new().await;
    let path = format!("{}/audit-log", f.org);
    for path in [path.clone(), format!("{path}?export=json")] {
        assert_eq!(f.get(&path, &f.bob).await.status(), StatusCode::NOT_FOUND);
    }
    let all = f.h.json(&path, &f.alice).await;
    let items = all["items"].as_array().unwrap();
    assert!(items.iter().any(|e| e["action"] == "team_membership"));
    assert!(
        items
            .iter()
            .any(|e| e["action"] == "organization_invitation")
    );
    let expected_id = items[1]["id"].clone();

    let first = f.h.json(&format!("{path}?limit=1"), &f.alice).await;
    let next = first["next"].as_str().unwrap();
    let current = f.h.json(&f.org, &f.alice).await;
    f.h.write(
        Method::PATCH,
        &f.org,
        &f.alice,
        json!({
            "version":current["organization"]["version"],
            "name":"Audit qualification",
            "description":"",
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let following =
        f.h.json(&format!("{path}?limit=1&after={next}"), &f.alice)
            .await;
    assert_eq!(following["items"][0]["id"], expected_id);
    assert_eq!(
        f.get(
            &format!("{path}?action=team_created&after={next}"),
            &f.alice
        )
        .await
        .status(),
        StatusCode::CONFLICT,
    );

    let filter = format!(
        "action=repository_access_changed&repository=library&actor={}&since_ms=1",
        f.owner_id
    );
    let filtered = f.h.json(&format!("{path}?{filter}"), &f.alice).await;
    let filtered = filtered["items"].as_array().unwrap();
    assert_eq!(filtered.len(), 2);
    assert!(
        filtered
            .iter()
            .all(|event| event["action"] == "repository_access_changed"
                && event["repository"]["name"] == "library")
    );
    let exported = f
        .get(&format!("{path}?{filter}&export=json"), &f.alice)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        exported.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(
        exported.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=crab-audit-log.json"
    );
    assert_eq!(exported.headers()["x-crab-audit-count"], "2");
    assert!(!exported.headers().contains_key("x-crab-audit-next"));
    let body = exported.text().await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!(filtered)
    );
    assert!(!body.contains("alice-id"));
    assert!(!body.contains("release-admins"));
    let csv = f
        .get(&format!("{path}?{filter}&export=csv"), &f.alice)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        csv.headers()[header::CONTENT_TYPE],
        "text/csv; charset=utf-8"
    );
    let csv = csv.text().await.unwrap();
    assert!(csv.starts_with("event,occurred_at_ms,action,actor_id,"));
    assert_eq!(csv.lines().count(), 3);
    assert!(
        csv.lines()
            .skip(1)
            .all(|line| line.contains("\"repository_access_changed\""))
    );
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and a built embedded UI with isolated native OIDC and Cell state"]
async fn native_browser_filters_and_downloads_the_organization_audit_log() {
    let f = AccessFixture::new().await;
    *f.h.provider.mode.lock().await = String::new();
    let browser = Browser::new("audit-log");
    let url = format!("{}/-/organizations/{}", f.h.origin, f.organization);
    browser.run(&["open", &url]).await;
    browser.sign_in().await;
    browser
        .run(&["wait", "[aria-label='Organization'] a[href$='tab=audit']"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Audit log",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "[aria-label='Audit events']"]).await;
    browser
        .run(&["select", "#audit-action", "repository_access_changed"])
        .await;
    browser.run(&["fill", "#audit-repository", "library"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Apply filters",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "--fn", "location.search.includes('repository_access_changed') && document.querySelectorAll('tbody tr').length === 2 && [...document.querySelectorAll('tbody tr')].every(row => row.innerText.includes('Repository access changed') && row.innerText.includes('library'))"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Export JSON",
            "--exact",
        ])
        .await;
    browser
        .run(&["wait", "--text", "Audit export complete."])
        .await;
    browser.run(&["close"]).await;
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_export_follows_the_original_snapshot_beyond_its_record_limit() {
    let f = AccessFixture::new().await;
    let cells = f.h.server.cells().unwrap();
    let address = Address::Organization(f.organization);
    let own = f.h.json(&f.org, &f.alice).await;
    let revision = own["organization"]["version"].as_u64().unwrap();
    for index in 0..501 {
        let outcome = cells
            .command::<Edit>(
                address,
                app::mutation_identity().unwrap(),
                EditInput {
                    actor: f.owner_id,
                    version: revision + index,
                    name: format!("Organization revision {index}"),
                    description: String::new(),
                },
            )
            .await
            .unwrap()
            .output;
        assert!(matches!(outcome, ChangeOutcome::Applied(_)));
    }

    let path = format!("{}/audit-log?action=organization_edited&export=json", f.org);
    let first = f.get(&path, &f.alice).await.error_for_status().unwrap();
    assert_eq!(first.headers()["x-crab-audit-count"], "500");
    let cursor = first.headers()["x-crab-audit-next"]
        .to_str()
        .unwrap()
        .to_owned();
    let first: Vec<Value> = first.json().await.unwrap();
    assert_eq!(first.len(), 500);

    let next = f
        .get(&format!("{path}&after={cursor}"), &f.alice)
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(next.headers()["x-crab-audit-count"], "1");
    assert!(!next.headers().contains_key("x-crab-audit-next"));
    let next: Vec<Value> = next.json().await.unwrap();
    assert!(first.iter().all(|event| event["id"] != next[0]["id"]));
    f.h.close().await;
}
