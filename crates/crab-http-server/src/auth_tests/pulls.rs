use super::*;

const ROOT: &str = "/api/repos/team/private/pulls";

async fn mutate(
    h: &Harness,
    cookie: &str,
    csrf: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .header(header::COOKIE, cookie)
        .header(header::ORIGIN, &h.origin)
        .header("x-csrf-token", csrf)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

async fn report_status(h: &Harness, token: &str, oid: &str, body: Value) -> (StatusCode, Value) {
    let response = h
        .http
        .post(format!(
            "{}/api/repos/team/private/statuses/{oid}",
            h.origin
        ))
        .basic_auth("crab", Some(token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

async fn report_check(
    h: &Harness,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let response = h
        .http
        .request(method, format!("{}{path}", h.origin))
        .basic_auth("crab", Some(token))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    (status, body)
}

#[tokio::test]
async fn pull_routes_require_membership_and_csrf_before_repository_access() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    assert_eq!(
        h.http
            .get(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let input = json!({
        "request_id":"00000000-0000-4000-8000-000000000001",
        "title":"Proposed change",
        "body":"Please review",
        "base_ref":"refs/heads/main",
        "head_ref":"refs/heads/feature"
    });
    assert_eq!(
        h.http
            .post(format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &alice)
            .header(header::CONTENT_TYPE, "application/json")
            .body(input.to_string())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    *h.provider.mode.lock().await = "outsider".into();
    let outsider = h.login().await;
    for method in [reqwest::Method::GET, reqwest::Method::POST] {
        let session = h.json("/api/session", &outsider).await;
        let response = h
            .http
            .request(method, format!("{}{ROOT}", h.origin))
            .header(header::COOKIE, &outsider)
            .header(header::ORIGIN, &h.origin)
            .header("x-csrf-token", session["csrf"].as_str().unwrap())
            .header(header::CONTENT_TYPE, "application/json")
            .body(input.to_string())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    h.close().await;
}

#[tokio::test]
async fn pull_review_decisions_require_another_member_and_follow_the_exact_head() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let alice_session = h.json("/api/session", &alice).await;
    let alice_csrf = alice_session["csrf"].as_str().unwrap();
    let token = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        "/api/git-token",
        json!({"owner":"team","repository":"private","access":"write"}),
    )
    .await;
    assert_eq!(token.0, StatusCode::OK);
    let git_token = token.1["token"].as_str().unwrap().to_owned();
    let mut git_url = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    git_url.set_username("crab").unwrap();
    git_url.set_password(Some(&git_token)).unwrap();
    let source = tempfile::tempdir().unwrap();
    let path = source.path();
    crate::server::receive_tests::success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "base\n").unwrap();
    crate::server::receive_tests::success(path, &["add", "README.md"]).await;
    crate::server::receive_tests::success(path, &["commit", "-m", "base"]).await;
    crate::server::receive_tests::success(path, &["push", git_url.as_str(), "main"]).await;
    crate::server::receive_tests::success(path, &["checkout", "-b", "feature"]).await;
    std::fs::write(path.join("README.md"), "base\nfeature\n").unwrap();
    crate::server::receive_tests::success(path, &["commit", "-am", "feature"]).await;
    let first_head = crate::server::receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(path, &["push", git_url.as_str(), "feature"]).await;
    let created = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        ROOT,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000010",
            "title":"Proposed change",
            "body":"Please review",
            "base_ref":"refs/heads/main",
            "head_ref":"refs/heads/feature"
        }),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED);
    assert_eq!(created.1["can_decide"], false);
    assert_eq!(created.1["can_merge"], false);
    let blocked = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/merge"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000014",
            "version":created.1["version"],
            "method":"fast_forward",
            "base_oid":created.1["base_oid"],
            "head_oid":created.1["head_oid"]
        }),
    )
    .await;
    assert_eq!(blocked.0, StatusCode::CONFLICT);
    assert_eq!(blocked.1["error"]["code"], "merge_blocked");

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    let bob_session = h.json("/api/session", &bob).await;
    let bob_csrf = bob_session["csrf"].as_str().unwrap();
    assert_eq!(h.json(&format!("{ROOT}/1"), &bob).await["can_decide"], true);
    let approved = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000011",
            "body":"",
            "state":"approved"
        }),
    )
    .await;
    assert_eq!(approved.0, StatusCode::CREATED);
    assert_eq!(approved.1["commit_oid"], first_head);
    assert_eq!(approved.1["current"], true);
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["version"], 2);
    assert_eq!(ready["merge_requirements"]["approvals"], 1);
    assert_eq!(
        ready["merge_requirements"]["checks"][0]["state"],
        Value::Null
    );
    assert_eq!(ready["merge_requirements"]["checks_satisfied"], false);
    assert_eq!(ready["merge_requirements"]["satisfied"], false);
    assert_eq!(ready["can_merge"], false);
    let blocked = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/merge"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000021",
            "version":ready["version"],
            "method":"fast_forward",
            "base_oid":ready["base_oid"],
            "head_oid":ready["head_oid"]
        }),
    )
    .await;
    assert_eq!(blocked.0, StatusCode::CONFLICT);
    assert_eq!(blocked.1["error"]["code"], "merge_blocked");
    let pending = report_status(
        &h,
        &git_token,
        &first_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000017",
            "context":"ci/test",
            "state":"pending",
            "description":"Tests are running",
            "target_url":"https://ci.example.test/build/17"
        }),
    )
    .await;
    assert_eq!(pending.0, StatusCode::CREATED, "{}", pending.1);
    assert_eq!(pending.1["state"], "pending");
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::POST,
            &format!("/api/repos/team/private/statuses/{first_head}"),
            json!({
                "request_id":"00000000-0000-4000-8000-000000000018",
                "context":"ci/test",
                "state":"success"
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let success = report_status(
        &h,
        &git_token,
        &first_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000019",
            "context":"CI/Test",
            "state":"success",
            "description":"Tests passed",
            "target_url":"https://ci.example.test/build/19"
        }),
    )
    .await;
    assert_eq!(success.0, StatusCode::CREATED);
    assert_eq!(success.1["state"], "success");
    let replay = report_status(
        &h,
        &git_token,
        &first_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000017",
            "context":"ci/test",
            "state":"pending",
            "description":"Tests are running",
            "target_url":"https://ci.example.test/build/17"
        }),
    )
    .await;
    assert_eq!(replay.0, StatusCode::CREATED);
    assert_eq!(replay.1["state"], "pending");
    let statuses = h
        .json(
            &format!("/api/repos/team/private/commits/{first_head}/status"),
            &bob,
        )
        .await;
    assert_eq!(statuses["state"], "success");
    assert_eq!(statuses["statuses"].as_array().unwrap().len(), 1);
    assert_eq!(statuses["statuses"][0]["context"], "CI/Test");
    let created_check = report_check(
        &h,
        &git_token,
        reqwest::Method::POST,
        "/api/repos/team/private/check-runs",
        json!({
            "request_id":"00000000-0000-4000-8000-000000000023",
            "head_sha":first_head,
            "name":"ci/test",
            "status":"queued",
            "conclusion":null,
            "details_url":"https://ci.example.test/runs/23",
            "output":{
                "title":"Tests are queued",
                "summary":"Waiting for a runner.",
                "text":null,
                "steps":[]
            }
        }),
    )
    .await;
    assert_eq!(created_check.0, StatusCode::CREATED, "{}", created_check.1);
    let run_id = created_check.1["id"].as_u64().unwrap();
    let run_path = format!("/api/repos/team/private/commits/{first_head}/check-runs/{run_id}");
    let waiting = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(
        waiting["merge_requirements"]["checks"][0]["state"],
        "pending"
    );
    assert_eq!(waiting["merge_requirements"]["checks"][0]["run_id"], run_id);
    let listed = h
        .json(
            &format!("/api/repos/team/private/commits/{first_head}/check-runs"),
            &bob,
        )
        .await;
    assert_eq!(listed["items"][0]["name"], "ci/test");
    assert_eq!(listed["next"], Value::Null);
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::PATCH,
            &run_path,
            json!({
                "request_id":"00000000-0000-4000-8000-000000000024",
                "version":1,
                "status":"in_progress",
                "conclusion":null,
                "details_url":null,
                "output":{"title":"Denied","summary":"Denied","text":null,"steps":[]}
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let running_input = json!({
        "request_id":"00000000-0000-4000-8000-000000000025",
        "version":1,
        "status":"in_progress",
        "conclusion":null,
        "details_url":"https://ci.example.test/runs/23",
        "output":{
            "title":"Tests are running",
            "summary":"One step has completed.",
            "text":"Live output is available below.",
            "steps":[{
                "name":"Build",
                "status":"completed",
                "conclusion":"success",
                "log":"Compiling crab-http-server\nFinished release build\n"
            }]
        }
    });
    let running = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        running_input.clone(),
    )
    .await;
    assert_eq!(running.0, StatusCode::OK, "{}", running.1);
    assert_eq!(running.1["version"], 2);
    let completed = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000026",
            "version":2,
            "status":"completed",
            "conclusion":"success",
            "details_url":"https://ci.example.test/runs/23",
            "output":{
                "title":"All tests passed",
                "summary":"The required test suite passed.",
                "text":"No failures were reported.",
                "steps":[{
                    "name":"Build",
                    "status":"completed",
                    "conclusion":"success",
                    "log":"Compiling crab-http-server\nFinished release build\n"
                },{
                    "name":"Test",
                    "status":"completed",
                    "conclusion":"success",
                    "log":"44 passed; 0 failed\n"
                }],
                "annotations":[{
                    "path":"src/lib.rs",
                    "start_line":42,
                    "end_line":44,
                    "level":"warning",
                    "title":"Slow assertion",
                    "message":"This assertion took longer than expected."
                }]
            }
        }),
    )
    .await;
    assert_eq!(completed.0, StatusCode::OK, "{}", completed.1);
    assert_eq!(completed.1["version"], 3);
    assert_eq!(
        completed.1["output"]["steps"][1]["log"],
        "44 passed; 0 failed\n"
    );
    assert_eq!(completed.1["output"]["annotations"][0]["start_line"], 42);
    let replay = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        running_input,
    )
    .await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["version"], 2);
    assert_eq!(h.json(&run_path, &bob).await["version"], 3);
    let immutable = report_check(
        &h,
        &git_token,
        reqwest::Method::PATCH,
        &run_path,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000027",
            "version":3,
            "status":"in_progress",
            "conclusion":null,
            "details_url":null,
            "output":{"title":"Restarted","summary":"Restarted","text":null}
        }),
    )
    .await;
    assert_eq!(immutable.0, StatusCode::BAD_REQUEST);
    let invalid_annotation = report_check(
        &h,
        &git_token,
        reqwest::Method::POST,
        "/api/repos/team/private/check-runs",
        json!({
            "request_id":"00000000-0000-4000-8000-000000000028",
            "head_sha":first_head,
            "name":"ci/lint",
            "status":"completed",
            "conclusion":"success",
            "details_url":null,
            "output":{
                "title":"Lint passed",
                "summary":"No findings.",
                "text":null,
                "annotations":[{
                    "path":"src/lib.rs",
                    "start_line":2,
                    "end_line":1,
                    "level":"failure",
                    "title":null,
                    "message":"Invalid range"
                }]
            }
        }),
    )
    .await;
    assert_eq!(invalid_annotation.0, StatusCode::BAD_REQUEST);
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["merge_requirements"]["checks"][0]["state"], "success");
    assert_eq!(ready["merge_requirements"]["checks_satisfied"], true);
    assert_eq!(ready["merge_requirements"]["satisfied"], true);
    assert_eq!(ready["can_merge"], true);
    let edited = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1/reviews/1"),
        json!({"version":1,"body":"Approved after exact comparison."}),
    )
    .await;
    assert_eq!(edited.0, StatusCode::OK);
    assert_eq!(edited.1["state"], "approved");
    assert_eq!(edited.1["version"], 2);
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::POST,
            &format!("{ROOT}/1/merge"),
            json!({
                "request_id":"00000000-0000-4000-8000-000000000015",
                "version":1,
                "method":"fast_forward",
                "base_oid":created.1["base_oid"],
                "head_oid":created.1["head_oid"]
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        mutate(
            &h,
            &alice,
            alice_csrf,
            reqwest::Method::POST,
            &format!("{ROOT}/1/reviews"),
            json!({
                "request_id":"00000000-0000-4000-8000-000000000012",
                "body":"",
                "state":"approved"
            }),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    std::fs::write(path.join("README.md"), "base\nfeature\nupdated\n").unwrap();
    crate::server::receive_tests::success(path, &["commit", "-am", "updated"]).await;
    let second_head = crate::server::receive_tests::success(path, &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(path, &["push", git_url.as_str(), "feature"]).await;
    let stale = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(stale["merge_requirements"]["approvals"], 0);
    assert_eq!(
        stale["merge_requirements"]["checks"][0]["state"],
        Value::Null
    );
    assert_eq!(stale["merge_requirements"]["satisfied"], false);
    assert_eq!(stale["can_merge"], false);
    let reviews = h.json(&format!("{ROOT}/1/reviews"), &bob).await;
    assert_eq!(reviews["items"][0]["current"], false);
    let requested = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000013",
            "body":"Please address the remaining concern.",
            "state":"changes_requested"
        }),
    )
    .await;
    assert_eq!(requested.0, StatusCode::CREATED);
    assert_eq!(requested.1["commit_oid"], second_head);
    assert_eq!(requested.1["current"], true);
    let blocked = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(blocked["version"], 3);
    assert_eq!(blocked["merge_requirements"]["changes_requested"], 1);
    assert_eq!(blocked["merge_requirements"]["satisfied"], false);
    let approved = mutate(
        &h,
        &bob,
        bob_csrf,
        reqwest::Method::POST,
        &format!("{ROOT}/1/reviews"),
        json!({
            "request_id":"00000000-0000-4000-8000-000000000016",
            "body":"",
            "state":"approved"
        }),
    )
    .await;
    assert_eq!(approved.0, StatusCode::CREATED);
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["version"], 4);
    assert_eq!(ready["merge_requirements"]["approvals"], 1);
    assert_eq!(ready["merge_requirements"]["changes_requested"], 0);
    assert_eq!(ready["merge_requirements"]["checks_satisfied"], false);
    assert_eq!(ready["merge_requirements"]["satisfied"], false);
    let success = report_status(
        &h,
        &git_token,
        &second_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000020",
            "context":"ci/test",
            "state":"success"
        }),
    )
    .await;
    assert_eq!(success.0, StatusCode::CREATED);
    let ready = h.json(&format!("{ROOT}/1"), &alice).await;
    assert_eq!(ready["merge_requirements"]["satisfied"], true);
    let label = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::POST,
        "/api/repos/team/private/labels",
        json!({
            "request_id":"00000000-0000-4000-8000-000000000022",
            "name":"ready for merge",
            "color":"1f883d",
            "description":"All required checks passed"
        }),
    )
    .await;
    assert_eq!(label.0, StatusCode::CREATED);
    let labeled = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1"),
        json!({"version":ready["version"],"label_ids":[label.1["id"]]}),
    )
    .await;
    assert_eq!(labeled.0, StatusCode::OK);
    assert_eq!(labeled.1["labels"][0]["name"], "ready for merge");
    let assigned = mutate(
        &h,
        &alice,
        alice_csrf,
        reqwest::Method::PATCH,
        &format!("{ROOT}/1"),
        json!({"version":labeled.1["version"],"assignees":["bob-id"]}),
    )
    .await;
    assert_eq!(assigned.0, StatusCode::OK);
    assert_eq!(
        assigned.1["assignees"],
        json!([{"subject":"bob-id","name":"Bob"}])
    );
    assert_eq!(
        h.json(ROOT, &bob).await["items"][0],
        json!({
            "number":1,
            "title":"Proposed change",
            "state":"open",
            "author":"Alice",
            "base_ref":"refs/heads/main",
            "head_ref":"refs/heads/feature",
            "created_at":assigned.1["created_at"],
            "updated_at":assigned.1["updated_at"],
            "labels":[label.1],
            "assignees":[{"subject":"bob-id","name":"Bob"}]
        })
    );
    assert_eq!(
        mutate(
            &h,
            &bob,
            bob_csrf,
            reqwest::Method::PATCH,
            &format!("{ROOT}/1"),
            json!({"version":assigned.1["version"],"assignees":[]}),
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    crate::server::receive_tests::success(path, &["push", git_url.as_str(), ":feature"]).await;
    let replay = report_status(
        &h,
        &git_token,
        &second_head,
        json!({
            "request_id":"00000000-0000-4000-8000-000000000020",
            "context":"ci/test",
            "state":"success"
        }),
    )
    .await;
    assert_eq!(replay.0, StatusCode::CREATED);
    source.close().unwrap();
    h.close().await;
}
