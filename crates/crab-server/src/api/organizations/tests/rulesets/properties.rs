use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn bulk_property_edits_validate_every_repository_and_recover_the_receipt() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "org-property-batch").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let schema = format!("{org}/repository-properties");
    let bulk = format!("{schema}/values");
    let (first, owner) = create(&h, &org, &alice, "app").await;
    let (second, _) = create(&h, &org, &alice, "sandbox").await;
    let selection = format!("{bulk}?repositories={first},{second}");
    let first_values = format!("/api/repos/{owner}/app/properties");
    let second_values = format!("/api/repos/{owner}/sandbox/properties");
    let regulated = Uuid::now_v7().to_string();
    let region = Uuid::now_v7().to_string();
    h.write(
        Method::PUT,
        &schema,
        &alice,
        json!({"expected_version":0,"definitions":[
            {"id":regulated,"name":"regulated","description":"","kind":"boolean","options":[],"default":true},
            {"id":region,"name":"region","description":"","kind":"text","options":[],"default":null}
        ]}),
    )
    .await
    .error_for_status()
    .unwrap();

    let initial = h.json(&selection, &alice).await;
    assert_eq!(
        initial["effective"][&regulated],
        json!({"kind":"shared","value":true})
    );
    let command = |view: &serde_json::Value, value| {
        json!({
            "request_id":Uuid::now_v7(), "issued_at_ms":crate::cells::unix_now_ms().unwrap(),
            "schema_version":view["schema"]["version"], "repositories":view["repositories"],
            "changes":{(&regulated):value}
        })
    };
    repository_write(
        &h,
        Method::PUT,
        &second_values,
        &alice,
        second,
        json!({"expected_version":0,"schema_version":1,"overrides":{(&region):"us"}}),
    )
    .await
    .error_for_status()
    .unwrap();
    let stale = h
        .write(Method::PUT, &bulk, &alice, command(&initial, json!(false)))
        .await;
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    assert_eq!(h.json(&first_values, &alice).await["values"]["version"], 0);

    let current = h.json(&selection, &alice).await;
    assert_eq!(current["effective"][&region], json!({"kind":"mixed"}));
    let mut moved = command(&current, json!(false));
    let epoch = &mut moved["repositories"][1]["repository"]["ownership_epoch"];
    *epoch = json!(epoch.as_u64().unwrap() + 1);
    assert_eq!(
        h.write(Method::PUT, &bulk, &alice, moved).await.status(),
        StatusCode::CONFLICT
    );
    assert_eq!(h.json(&first_values, &alice).await["values"]["version"], 0);

    let submission = command(&current, json!(false));
    for _ in 0..2 {
        let receipt: serde_json::Value = h
            .write(Method::PUT, &bulk, &alice, submission.clone())
            .await
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(receipt, json!({"updated":2}));
    }
    let current = h.json(&selection, &alice).await;
    assert_eq!(current["repositories"][0]["version"], 1);
    assert_eq!(current["repositories"][1]["version"], 2);
    assert_eq!(
        current["effective"][&regulated],
        json!({"kind":"shared","value":false})
    );
    h.write(Method::PUT, &bulk, &alice, command(&current, json!(null)))
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        h.json(&selection, &alice).await["effective"][&regulated],
        json!({"kind":"shared","value":true})
    );
    assert_eq!(
        h.json(&second_values, &alice).await["values"]["overrides"],
        json!({(&region):"us"})
    );

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let read = h
        .http
        .get(format!("{}{selection}", h.origin))
        .header(reqwest::header::COOKIE, &bob)
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        h.write(Method::PUT, &bulk, &bob, submission).await.status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn property_ruleset_tracks_defaults_overrides_and_current_organization_authority() {
    let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
    let h = Harness::new_with_store_and_prefix(false, false, store, "org-property-rules").await;
    h.initialize_directory().await;
    h.server
        .catalog()
        .unwrap()
        .mark_cell_ready(h.server.repositories.values()[0].id)
        .await
        .unwrap();
    let (organization, _, _) = crate::test_support::http::organizations::bootstrap(&h).await;
    let alice = h.login().await;
    let org = format!("/api/organizations/{organization}");
    let schema = format!("{org}/repository-properties");
    let (release_id, owner) = create(&h, &org, &alice, "release-app").await;
    let (sandbox_id, _) = create(&h, &org, &alice, "release-sandbox").await;
    let release_url = remote(&h, &alice, &owner, "release-app").await;
    let sandbox_url = remote(&h, &alice, &owner, "release-sandbox").await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    git::push_review_branches(path, release_url.as_str()).await;
    git::success(path, &["push", sandbox_url.as_str(), "main"]).await;

    let regulated = Uuid::now_v7();
    let regions = Uuid::now_v7();
    let mut definitions = json!([
        {"id":regulated,"name":"regulated","description":"","kind":"boolean","options":[],"default":true},
        {"id":regions,"name":"regions","description":"","kind":"multi_select","options":["us","eu"],"default":["us","eu"]}
    ]);
    definitions[0]["default"] = json!("true");
    assert_eq!(
        h.write(
            Method::PUT,
            &schema,
            &alice,
            json!({"expected_version":0,"definitions":definitions})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    definitions[0]["default"] = json!(true);
    h.write(
        Method::PUT,
        &schema,
        &alice,
        json!({"expected_version":0,"definitions":definitions}),
    )
    .await
    .error_for_status()
    .unwrap();
    let release = format!("/api/repos/{owner}/release-app");
    let sandbox = format!("/api/repos/{owner}/release-sandbox");
    let save_values = async |repo: &str, id, version, overrides| {
        repository_write(
            &h,
            Method::PUT,
            &format!("{repo}/properties"),
            &alice,
            id,
            json!({"expected_version":version,"schema_version":1,"overrides":overrides}),
        )
        .await
    };
    let sandbox_override = json!({(regions.to_string()):["eu"]});
    save_values(&sandbox, sandbox_id, 0, sandbox_override.clone())
        .await
        .error_for_status()
        .unwrap();
    assert_eq!(
        save_values(&sandbox, sandbox_id, 0, sandbox_override)
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let rule_id = Uuid::now_v7();
    let org_rules = format!("{org}/rulesets");
    let entry = json!({
        "ruleset":rule(rule_id,"Regulated US repository","branch",json!(["~DEFAULT_BRANCH"])),
        "repositories":{"kind":"properties","conditions":[
            {"property":regulated,"value":true},{"property":regions,"value":"us"}
        ]}
    });
    let mut invalid = entry.clone();
    invalid["repositories"]["conditions"][1]["value"] = json!("undefined");
    assert_eq!(
        h.write(
            Method::PUT,
            &org_rules,
            &alice,
            json!({"expected_version":0,"rulesets":[invalid]})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    h.write(
        Method::PUT,
        &org_rules,
        &alice,
        json!({"expected_version":0,"rulesets":[entry]}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(
            Method::PUT,
            &schema,
            &alice,
            json!({"expected_version":1,"definitions":[definitions[0]]})
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let inherited = async |repo: &str| {
        h.json(&format!("{repo}/rulesets"), &alice).await["inherited"]["rulesets"].clone()
    };
    let assert_denied = async |remote: &url::Url| {
        let result = git::git(path, &["push", remote.as_str(), "HEAD:main"]).await;
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("requires a pull request"));
    };
    assert_eq!(inherited(&release).await[0]["id"], json!(rule_id));
    assert_eq!(inherited(&sandbox).await, json!([]));
    assert_denied(&release_url).await;
    git::success(path, &["push", sandbox_url.as_str(), "HEAD:main"]).await;

    // Unset resumes the default. An explicit false is an override, even
    // when the new value makes a previously protected repository ineligible.
    save_values(&sandbox, sandbox_id, 1, json!({}))
        .await
        .error_for_status()
        .unwrap();
    save_values(
        &release,
        release_id,
        0,
        json!({(regulated.to_string()):false}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(inherited(&sandbox).await[0]["id"], json!(rule_id));
    assert_eq!(inherited(&release).await, json!([]));
    git::success(
        path,
        &[
            "commit",
            "--allow-empty",
            "-m",
            "Change after targeting update",
        ],
    )
    .await;
    assert_denied(&sandbox_url).await;
    git::success(path, &["push", release_url.as_str(), "HEAD:main"]).await;

    let members = format!("{release}/members");
    let revision = h.json(&members, &alice).await["revision"].clone();
    repository_write(&h, Method::PUT, &members, &alice, release_id, json!({
        "expected_revision":revision,"groups":[],"members":[{"subject":"bob-id","name":"Bob","access":"admin"}]
    })).await.error_for_status().unwrap();
    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.json(&format!("{release}/properties"), &bob).await["values"]["overrides"],
        json!({(regulated.to_string()):false})
    );
    assert_eq!(
        repository_write(
            &h,
            Method::PUT,
            &format!("{release}/properties"),
            &bob,
            release_id,
            json!({"expected_version":1,"schema_version":1,"overrides":{}})
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    h.close().await;
}
