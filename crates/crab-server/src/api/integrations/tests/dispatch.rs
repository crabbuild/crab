use std::{sync::Arc, time::Duration};

use axum::{Json, Router, extract::Request, response::IntoResponse, routing::any};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Validation, decode};
use tokio::sync::mpsc;
use url::Url;

use super::*;

const APP_SECRET: &str = "test-only-github-app-webhook-secret";

async fn github_fixture() -> (
    impl FnOnce(Uuid) -> GitHubActions + Send,
    mpsc::UnboundedReceiver<Value>,
    tokio::task::JoinHandle<()>,
) {
    let directory = tempfile::tempdir().unwrap();
    let key_path = directory.path().join("fixture.pem");
    let openssl = |args: &[&str]| {
        let output = std::process::Command::new("openssl")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Could not generate GitHub App fixture key"
        );
        output.stdout
    };
    let path = key_path.to_str().unwrap();
    openssl(&["genpkey", "-algorithm", "RSA", "-out", path]);
    let signer = EncodingKey::from_rsa_pem(&std::fs::read(&key_path).unwrap()).unwrap();
    let verifier =
        Arc::new(DecodingKey::from_rsa_pem(&openssl(&["pkey", "-in", path, "-pubout"])).unwrap());
    let (sender, receiver) = mpsc::unbounded_channel();
    let handler = any(move |request: Request| {
        let sender = sender.clone();
        let verifier = Arc::clone(&verifier);
        async move {
            let (parts, body) = request.into_parts();
            assert_eq!(parts.headers["x-github-api-version"], "2026-03-10");
            let token = parts.headers["authorization"]
                .to_str()
                .unwrap()
                .strip_prefix("Bearer ")
                .unwrap();
            if parts.uri.path().starts_with("/app") {
                let claims = decode::<Value>(token, &verifier, &Validation::new(Algorithm::RS256))
                    .unwrap()
                    .claims;
                assert_eq!(claims["iss"], "7");
            } else {
                assert!(token == "scoped-fixture-token");
            }
            match (parts.method, parts.uri.path()) {
                (Method::GET, "/app") => {
                    Json(json!({"id":7,"slug":"crab-ci-fixture"})).into_response()
                }
                (Method::POST, "/app/installations/42/access_tokens") => {
                    let bytes = axum::body::to_bytes(body, 4096).await.unwrap();
                    let payload: Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(
                        payload,
                        json!({"repository_ids":[123],"permissions":{"actions":"write"}})
                    );
                    Json(json!({"token":"scoped-fixture-token"})).into_response()
                }
                (Method::GET, "/repos/example/mirror") => Json(json!({"id":123})).into_response(),
                (Method::POST, "/repos/example/mirror/actions/workflows/456/dispatches") => {
                    let bytes = axum::body::to_bytes(body, 4096).await.unwrap();
                    sender
                        .send(serde_json::from_slice(&bytes).unwrap())
                        .unwrap();
                    // GitHub accepted the workflow before its response was lost.
                    StatusCode::BAD_GATEWAY.into_response()
                }
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(handler))
            .await
            .unwrap();
    });
    let configure = move |crab_repository| {
        GitHubActions::new(
            7,
            signer,
            APP_SECRET.as_bytes().to_vec(),
            url,
            [456, 567]
                .into_iter()
                .map(|workflow| crate::GitHubActionsBinding {
                    crab_repository,
                    installation: 42,
                    github_repository: 123,
                    workflow,
                    branch: "main".into(),
                })
                .collect(),
        )
        .unwrap()
    };
    (configure, receiver, task)
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_dispatch_recovers_a_lost_reply_and_publishes_the_fetched_commit() {
    let (github, mut requests, provider) = github_fixture().await;
    let h = Harness::with_ci_dispatch("queue-ci", Some(Box::new(github))).await;
    let alice = h.login().await;
    let id = Uuid::now_v7();
    let connected: Value = h.write(
        Method::POST, SETTINGS, &alice,
        json!({"kind":"connect","id":id,"name":"ci/test","github_repository":123,"workflow":456}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    let webhook = connected["connection"]["url"].as_str().unwrap();
    let secret = APP_SECRET;
    assert_eq!(webhook, format!("{}{APP_WEBHOOK}", h.origin));
    assert_eq!(connected["connection"]["app_managed"], true);
    assert!(connected.get("secret").is_none());
    let alt = Uuid::now_v7();
    let alternate: Value = h.write(Method::POST, SETTINGS, &alice,
        json!({"kind":"connect","id":alt,"name":"ci/lint","github_repository":123,"workflow":567}),
    ).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(alternate["connection"]["url"], webhook);
    assert!(alternate.get("secret").is_none());
    let generation = Uuid::now_v7();
    let configure = json!({
        "kind":"dispatch","id":id,"expected":connected["connection"]["credential_version"],"previous":null,
        "dispatch":{"id":generation,"branch":"main","repository":"example/mirror","installation":42},
    });
    let mut unbound = configure.clone();
    unbound["dispatch"]["installation"] = json!(43);
    assert_eq!(
        h.write(Method::POST, SETTINGS, &alice, unbound)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    let configured: Value = h
        .write(Method::POST, SETTINGS, &alice, configure.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let replay: Value = h
        .write(Method::POST, SETTINGS, &alice, configure)
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        replay == configured,
        "a lost response must recover the same checkout token"
    );
    let token = configured["fetch_token"].as_str().unwrap();
    assert!(token != secret);
    assert!(!h.json(SETTINGS, &alice).await.to_string().contains(token));

    let write_token: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = Url::parse(&format!("{}/git/team/private.git", h.origin)).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(write_token["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let git = async |args: &[&str]| git::success(source.path(), args).await;
    let head = git::push_review_branches(source.path(), remote.as_str()).await;
    let base = git(&["rev-parse", "main"]).await;
    git(&["checkout", "-b", "unrelated", "main"]).await;
    git(&["commit", "--allow-empty", "-m", "Unrelated commit"]).await;
    let unrelated = git(&["rev-parse", "HEAD"]).await;
    git(&["push", remote.as_str(), "unrelated"]).await;
    h.write(
        Method::PUT,
        &format!("{ROOT}/settings/branch-protections"),
        &alice,
        json!({
            "expected_version":0,"rules":[{"branch":"main","required_approvals":0,
                "require_merge_queue":true,"required_integrations":[id]}],
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    h.write(
        Method::POST,
        &format!("{ROOT}/pulls"),
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"title":"Queue with Actions","body":"",
            "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
        }),
    )
    .await
    .error_for_status()
    .unwrap();

    let report = async |delivery: Uuid, body: &Value| {
        github_workflow_delivery(&h, webhook, secret, delivery, body, body).await
    };
    let mut run = json!({
        "action":"completed","installation":{"id":42},"repository":{"id":123,"full_name":"example/mirror"},
        "workflow_run":{"id":100,"workflow_id":456,"run_number":1,"run_attempt":1,"head_sha":head,
            "event":"push","status":"completed","conclusion":"success"},
    });
    let other = format!("{SETTINGS}/{alt}/deliveries");
    let history = format!("{SETTINGS}/{id}/deliveries");
    let before = h.json(PULL, &alice).await;
    assert!(before["merge_requirements"]["checks"][0]["state"].is_null());
    let mut signed = run.clone();
    signed["installation"]["id"] = json!(99);
    assert_eq!(
        report(Uuid::now_v7(), &signed).await,
        StatusCode::NO_CONTENT
    );
    signed["installation"]["id"] = json!(42);
    signed["workflow_run"]["workflow_id"] = json!(567);
    let delivery = Uuid::now_v7();
    for _ in 0..2 {
        assert_eq!(report(delivery, &signed).await, StatusCode::NO_CONTENT);
    }
    assert_eq!(h.json(&other, &alice).await.as_array().unwrap().len(), 1);
    assert_eq!(h.json(&history, &alice).await, json!([]));
    let forged =
        github_workflow_delivery(&h, webhook, "wrong-secret", Uuid::now_v7(), &run, &run).await;
    assert_eq!(forged, StatusCode::FORBIDDEN);

    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let old_url = format!("{}/webhooks/github-actions/{}/{id}", h.origin, repo.id);
    let legacy_secret = super::super::secret(
        super::super::shared_key(&h.server).unwrap(),
        repo.id,
        &super::super::connections(&h.server, &repo)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap(),
    );
    assert_eq!(
        github_workflow_delivery(&h, &old_url, &legacy_secret, Uuid::now_v7(), &run, &run).await,
        StatusCode::NOT_FOUND,
    );
    assert_eq!(report(Uuid::now_v7(), &run).await, StatusCode::NO_CONTENT);
    let pull = h.json(PULL, &alice).await;
    h.write(
        Method::PUT,
        &format!("{PULL}/auto-merge"),
        &alice,
        json!({
            "request_id":Uuid::now_v7(),"version":pull["version"],"base_oid":pull["base_oid"],
            "head_oid":pull["head_oid"],"method":"merge_commit","queue":true,
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    let worker = tokio::spawn(crate::api::auto_merge::run(Arc::clone(&h.server)));
    let request = tokio::time::timeout(Duration::from_secs(45), requests.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request["ref"], "main");
    let input = &request["inputs"];
    assert_eq!(input.as_object().unwrap().len(), 3);
    let tested = input["crab_commit"].as_str().unwrap();
    let source_url = input["crab_source"].as_str().unwrap();
    assert!(!request.to_string().contains(token));
    assert!(!request.to_string().contains(secret));
    assert_ne!(tested, head);
    assert_ne!(tested, base);

    // A push report cannot replace a dispatch-bound result for the same SHA.
    run["workflow_run"]["head_sha"] = json!(tested);
    assert_eq!(report(Uuid::now_v7(), &run).await, StatusCode::NOT_FOUND);

    let clone = tempfile::tempdir().unwrap();
    let git_checkout = async |args: &[&str]| git::success(clone.path(), args).await;
    git_checkout(&["init", "--object-format=sha1"]).await;
    let mut checkout = Url::parse(source_url).unwrap();
    let response = h
        .http
        .get(format!("{checkout}/info/refs?service=git-upload-pack"))
        .basic_auth("crab-ci", Some("bad"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    checkout.set_username("crab-ci").unwrap();
    checkout.set_password(Some(token)).unwrap();
    let reference = crate::git::receive::merge_candidate_ref(1);
    assert_eq!(
        git_checkout(&[
            "-c",
            "protocol.version=2",
            "ls-remote",
            "--refs",
            checkout.as_str()
        ])
        .await,
        format!("{tested}\t{reference}")
    );
    let denied = git::git(
        clone.path(),
        &[
            "-c",
            "protocol.version=2",
            "fetch",
            checkout.as_str(),
            &unrelated,
        ],
    )
    .await;
    assert!(
        !denied.status.success(),
        "An unrelated object must not be downloadable"
    );
    git_checkout(&[
        "-c",
        "protocol.version=2",
        "fetch",
        "--depth=1",
        checkout.as_str(),
        tested,
    ])
    .await;
    assert_eq!(git_checkout(&["rev-parse", "FETCH_HEAD"]).await, tested);

    let title = format!(
        "Crab queue {tested} {}",
        input["crab_dispatch"].as_str().unwrap()
    );
    run["action"] = json!("in_progress");
    run["workflow_run"] = json!({
        "id":456789,"workflow_id":456,"run_number":2,"run_attempt":1,"head_sha":unrelated,
        "event":"workflow_dispatch","status":"in_progress","display_title":title,"head_branch":"main",
        "head_repository":{"id":123,"full_name":"example/mirror"},"actor":{"login":"someone-else"},
    });
    assert_eq!(report(Uuid::now_v7(), &run).await, StatusCode::NOT_FOUND);
    run["workflow_run"]["actor"]["login"] = json!("crab-ci-fixture[bot]");
    assert_eq!(report(Uuid::now_v7(), &run).await, StatusCode::NO_CONTENT);
    let candidate = h.json(&format!("{PULL}/merge-candidate"), &alice).await;
    assert_eq!(candidate["candidate"]["commit"], tested);
    assert_eq!(
        candidate["candidate"]["dispatches"][0]["url"],
        "https://github.com/example/mirror/actions/runs/456789"
    );

    run["action"] = json!("completed");
    run["workflow_run"]["status"] = json!("completed");
    run["workflow_run"]["conclusion"] = json!("success");
    run["workflow_run"]["id"] = json!(456790);
    assert_eq!(report(Uuid::now_v7(), &run).await, StatusCode::NOT_FOUND);
    run["workflow_run"]["id"] = json!(456789);
    let delivery = Uuid::now_v7();
    assert_eq!(report(delivery, &run).await, StatusCode::NO_CONTENT);
    tokio::time::timeout(Duration::from_secs(45), async {
        while h.json(PULL, &alice).await["state"] != "merged" {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    })
    .await
    .unwrap();
    git(&["fetch", remote.as_str(), "main"]).await;
    assert_eq!(git(&["rev-parse", "FETCH_HEAD"]).await, tested);
    assert_eq!(report(delivery, &run).await, StatusCode::NO_CONTENT);
    let response = h
        .http
        .get(format!("{source_url}/info/refs?service=git-upload-pack"))
        .basic_auth("crab-ci", Some(token))
        .send()
        .await
        .unwrap();
    assert!(matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED
    ));

    worker.abort();
    provider.abort();
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn promoting_a_connection_retires_its_old_signing_key_and_required_result() {
    let (github, _, provider) = github_fixture().await;
    let h = Harness::with_ci_dispatch("promote-ci", Some(Box::new(github))).await;
    let alice = h.login().await;
    let repo = h
        .server
        .repositories
        .get(&("team".into(), "private".into()))
        .unwrap();
    let cell = app::repository_cell(&h.server, &repo).unwrap();
    let id = Uuid::now_v7();
    let stored::Changed::Saved(row) = app::command_output(
        cell.client
            .command::<stored::Edit>(
                cell.target,
                app::mutation_identity().unwrap(),
                stored::Change::Connect {
                    id,
                    name: "ci/test".into(),
                    github_repository: 123,
                    workflow: 456,
                    app_managed: false,
                },
            )
            .await,
    )
    .unwrap() else {
        panic!("expected a standalone connection");
    };
    let origin = &h.origin;
    let legacy = format!("{origin}/webhooks/github-actions/{}/{id}", repo.id);
    let secret = crate::api::integrations::secret(shared_key(&h.server).unwrap(), repo.id, &row);

    let access: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            &alice,
            json!({"owner":"team","repository":"private","access":"write"}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut remote = Url::parse(&format!("{origin}/git/team/private.git")).unwrap();
    remote.set_username("crab").unwrap();
    remote.set_password(access["token"].as_str()).unwrap();
    let source = tempfile::tempdir().unwrap();
    let head = git::push_review_branches(source.path(), remote.as_str()).await;
    h.write(Method::PUT, &format!("{ROOT}/settings/branch-protections"), &alice,
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":0,"required_integrations":[id]}]}))
        .await.error_for_status().unwrap();
    h.write(Method::POST, &format!("{ROOT}/pulls"), &alice,
        json!({"request_id":Uuid::now_v7(),"title":"Promote CI authority","body":"","base_ref":"refs/heads/main","head_ref":"refs/heads/feature"}))
        .await.error_for_status().unwrap();

    let delivery = Uuid::now_v7();
    let run = json!({
        "action":"completed","installation":{"id":42},"repository":{"id":123,"full_name":"example/mirror"},
        "workflow_run":{"id":100,"workflow_id":456,"run_number":1,"run_attempt":1,"head_sha":head,
            "event":"push","status":"completed","conclusion":"success"},
    });
    let report = async |url: &str, key: &str, id| {
        github_workflow_delivery(&h, url, key, id, &run, &run).await
    };
    assert_eq!(
        report(&legacy, &secret, delivery).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.json(PULL, &alice).await["can_merge"], true);

    let generation = Uuid::now_v7();
    let configured: Value = h.write(Method::POST, SETTINGS, &alice, json!({
        "kind":"dispatch","id":id,"expected":id,"previous":null,
        "dispatch":{"id":generation,"repository":"example/mirror","installation":42,"branch":"main"},
    })).await.error_for_status().unwrap().json().await.unwrap();
    assert_eq!(configured["connection"]["app_managed"], true);
    assert_eq!(h.json(PULL, &alice).await["can_merge"], false);
    let cleared: Value = h
        .write(
            Method::POST,
            SETTINGS,
            &alice,
            json!({
                "kind":"dispatch","id":id,"expected":id,"previous":generation,"dispatch":null,
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["connection"]["app_managed"], true);
    assert!(cleared.get("secret").is_none());
    assert_eq!(
        report(&legacy, &secret, Uuid::now_v7()).await,
        StatusCode::NOT_FOUND
    );
    let webhook = configured["connection"]["url"].as_str().unwrap();
    assert_eq!(
        report(webhook, APP_SECRET, delivery).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.json(PULL, &alice).await["can_merge"], true);

    provider.abort();
    h.close().await;
}
