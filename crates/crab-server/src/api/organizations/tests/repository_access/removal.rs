use super::*;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI; uses an isolated OIDC/native Cell fixture"]
async fn native_browser_confirms_outside_collaborator_removal() {
    let f = AccessFixture::new().await;
    f.h.write(
        Method::POST,
        &format!("{}/members/conversion", f.org),
        &f.alice,
        json!({"members":[{"actor":f.member,"version":1}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    *f.h.provider.mode.lock().await = "owner".into();
    let browser = Browser::new("outside-bulk-removal");
    browser
        .run(&[
            "open",
            &format!(
                "{}/-/organizations/{}?tab=outside-collaborators",
                f.h.origin, f.organization
            ),
        ])
        .await;
    browser.sign_in().await;
    let checkbox = "[role=checkbox][aria-label='Select Bob']";
    browser.run(&["wait", checkbox]).await;
    browser.run(&["click", checkbox]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Remove from all repositories",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "Remove 1 direct grant for 1 collaborator across 1 repository.",
        ])
        .await;
    let member_path = format!("{}/members", f.repository);
    assert_eq!(
        f.h.json(&member_path, &f.alice).await["members"][0]["name"],
        "Bob"
    );
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Remove outside collaborators",
            "--exact",
        ])
        .await;
    browser
        .run(&[
            "wait",
            "--text",
            "Removed 1 direct repository grant for 1 collaborator.",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No outside collaborators"])
        .await;
    assert_eq!(f.h.json(&member_path, &f.alice).await["members"], json!([]));
    browser
        .run(&[
            "find", "role", "link", "click", "--name", "Settings", "--exact",
        ])
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
    crate::api::organizations::tests::audit::choose_action(
        &browser,
        "outside_collaborators_removed",
    )
    .await;
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
    browser.run(&["wait", "--fn", "new URL(location.href).searchParams.get('action') === 'outside_collaborators_removed' && document.querySelectorAll('tbody tr').length === 1 && document.querySelector('tbody tr').innerText.includes('1 direct grant')"]).await;
    drop(browser);
    f.h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bulk_outside_removal_is_current_atomic_and_recoverable() {
    let f = AccessFixture::new().await;
    let listing = format!("{}/outside-collaborators", f.org);
    let preview_path = format!("{listing}/removal-preview");
    let removal_path = format!("{listing}/removals");
    let invite = format!("{}/invitations/{}", f.org, f.member);
    f.h.write(
        Method::POST,
        &format!("{}/members/conversion", f.org),
        &f.alice,
        json!({"members":[{"actor":f.member,"version":1}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let repos =
        f.h.json(
            &format!("/api/repos?organization={}", f.organization),
            &f.alice,
        )
        .await;
    let record = repos["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "private-tool")
        .unwrap();
    let second = format!(
        "/api/repos/{}/private-tool/members",
        record["owner"].as_str().unwrap()
    );
    let second_id = Uuid::parse_str(record["id"].as_str().unwrap()).unwrap();
    let state = f.h.json(&second, &f.alice).await;
    repository_write(
        &f.h,
        Method::PUT,
        &second,
        &f.alice,
        second_id,
        json!({
            "expected_revision":state["revision"],"members":[
                {"subject":"bob-id","name":"Bob guest","access":"write"},
                {"subject":"visiting-engineer","name":"Visiting engineer","access":"read"},
                {"subject":"retained-account","name":"Retained account","access":"read"}
            ],"groups":[]
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let outside = f.h.json(&listing, &f.alice).await;
    let entries = outside["items"].as_array().unwrap();
    let bob = entries.iter().find(|row| row["name"] == "Bob").unwrap();
    assert_eq!(bob["repository_count"], 2);
    let visitor = entries
        .iter()
        .find(|row| row["name"] == "Visiting engineer")
        .unwrap();
    let selection = json!({"collaborators":[bob["key"],visitor["key"]]});
    let post = |path: String, body: Value| {
        let (h, alice) = (&f.h, &f.alice);
        async move { h.write(Method::POST, &path, alice, body).await }
    };
    assert_eq!(
        f.h.write(Method::POST, &preview_path, &f.bob, selection.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    f.h.write(
        Method::PUT,
        &invite,
        &f.alice,
        json!({"version":2,"role":"member"}),
    )
    .await
    .error_for_status()
    .unwrap();
    let plan = post(preview_path.clone(), selection.clone())
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let impact = json!({"collaborators":2,"repositories":2,"grants":3});
    assert_eq!(plan["impact"], impact);
    assert!(!plan.to_string().contains("bob-id"));
    f.h.write(
        Method::POST,
        &format!("{}/invitation", f.org),
        &f.bob,
        json!({"accept":true,"version":3}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        post(removal_path.clone(), plan["plan"].clone())
            .await
            .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        f.h.json(&second, &f.alice).await["members"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    f.h.write(
        Method::POST,
        &format!("{}/members/conversion", f.org),
        &f.alice,
        json!({"members":[{"actor":f.member,"version":3}]}),
    )
    .await
    .error_for_status()
    .unwrap();
    let plan = post(preview_path, selection)
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let primary = format!("{}/members", f.repository);
    let before = f.h.json(&primary, &f.alice).await;
    for _ in 0..2 {
        assert_eq!(
            post(removal_path.clone(), plan["plan"].clone())
                .await
                .error_for_status()
                .unwrap()
                .json::<Value>()
                .await
                .unwrap(),
            impact
        );
    }
    let after = f.h.json(&primary, &f.alice).await;
    assert_eq!(
        after["revision"].as_u64(),
        before["revision"].as_u64().map(|revision| revision + 1)
    );
    assert_eq!(after["members"], json!([]));
    assert_eq!(after["groups"], before["groups"]);
    let second_after = f.h.json(&second, &f.alice).await;
    assert_eq!(second_after["members"].as_array().unwrap().len(), 1);
    assert_eq!(second_after["members"][0]["name"], "Retained account");
    let regranted = json!([{"subject":"bob-id","name":"Bob","access":"read"}]);
    repository_write(
        &f.h,
        Method::PUT,
        &primary,
        &f.alice,
        f.repository_id,
        json!({
            "expected_revision":after["revision"],"members":regranted,"groups":after["groups"]
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let recovered = post(removal_path, plan["plan"].clone())
        .await
        .error_for_status()
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(recovered, impact);
    assert_eq!(f.h.json(&primary, &f.alice).await["members"], regranted);
    let audit =
        f.h.json(
            &format!("{}/audit-log?action=outside_collaborators_removed", f.org),
            &f.alice,
        )
        .await;
    assert_eq!(audit["items"].as_array().unwrap().len(), 1);
    let detail = audit["items"][0]["detail"].as_str().unwrap();
    assert!(detail.contains("3 direct grants"));
    assert_eq!(audit["items"][0]["repository"], Value::Null);
    let event = audit["items"][0]["id"]
        .as_str()
        .unwrap()
        .strip_prefix("repository:")
        .unwrap();
    let catalog = f.h.server.catalog().unwrap();
    let reopened = crate::catalog::CatalogStore::new(catalog.root().clone());
    let (snapshot, _) = reopened.load_snapshot().await.unwrap();
    let receipt = reopened
        .outside_removal_receipt(
            &snapshot,
            f.organization,
            crate::catalog::audit::Position {
                revision: plan["plan"]["catalog_revision"].as_u64().unwrap() + 1,
                id: Uuid::parse_str(event).unwrap(),
            },
            plan["plan"]["confirmation"].as_str().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(receipt).unwrap(), impact);
    f.h.close().await;
}
