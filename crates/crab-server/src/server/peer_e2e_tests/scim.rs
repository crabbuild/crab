use axum::http::{StatusCode, header};
use reqwest::{Client, Method};
use serde_json::{Value, json};

use super::process::{
    ProcessFixture, get_session, git, git_authenticated, login, provision, run, stop,
};
use crate::test_support::http::TEST_SCIM_TOKEN;

const REPO: &str = "/api/repos/team/private/refs";

async fn set_active(client: &Client, origin: &str, user: &str, active: bool) {
    let response: Value = client
        .patch(format!("{origin}/scim/v2/Users/{user}"))
        .bearer_auth(TEST_SCIM_TOKEN)
        .header(header::CONTENT_TYPE, "application/scim+json")
        .json(&json!({
            "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"],
            "Operations": [{"op":"replace", "path":"active", "value":active}],
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["active"], active);
}

async fn sign_in(client: &Client, origin: &str, access: &str) -> (String, String) {
    let cookie = login(client, origin, "/team/private").await;
    let session = get_session(client, origin, &cookie).await;
    let token: Value = client
        .request(Method::POST, format!("{origin}/api/git-token"))
        .header(header::ORIGIN, origin)
        .header(header::COOKIE, &cookie)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .json(&json!({"owner":"team", "repository":"private", "access":access}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    (cookie, token["token"].as_str().unwrap().to_owned())
}

async fn check_access(
    client: &Client,
    origin: &str,
    credentials: &(String, String),
    expected_head: Option<&str>,
) {
    let (cookie, token) = credentials;
    assert_eq!(
        get_session(client, origin, cookie).await["authenticated"],
        expected_head.is_some()
    );
    let status = client
        .get(format!("{origin}{REPO}"))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
        .status();
    let remote = format!("{origin}/git/team/private.git");
    let git = git_authenticated(token)
        .args(["ls-remote", &remote, "refs/heads/main"])
        .output()
        .await
        .unwrap();
    match expected_head {
        Some(head) => {
            assert_eq!(status, StatusCode::OK);
            assert!(git.status.success());
            assert_eq!(
                String::from_utf8(git.stdout).unwrap().trim_end(),
                format!("{head}\trefs/heads/main")
            );
        }
        None => {
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert!(!git.status.success() && git.stdout.is_empty());
            assert_eq!(
                client
                    .get(format!("{remote}/info/refs?service=git-upload-pack"))
                    .basic_auth("crab", Some(token))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a built crab-server and an isolated RustFS bucket"]
async fn rustfs_scim_revocation_reaches_both_serving_processes_and_survives_restart() {
    let fixture = ProcessFixture::new("scim").await;
    let client = &fixture.client;
    let origin_a = &fixture.first.origin;
    let origin_b = &fixture.second.origin;
    let first_process = fixture.start(&fixture.first).await;
    run(fixture
        .command(&fixture.first)
        .args(["cells", "initialize-directory"]))
    .await;
    let second_process = fixture.start(&fixture.second).await;
    provision(client, origin_a, "alice-id").await;
    let bob_id = provision(client, origin_a, "bob-id").await;
    let alice = sign_in(client, origin_a, "write").await;
    let source = fixture.local.path().join("git");
    run(git()
        .args(["init", "--initial-branch=main", "--object-format=sha1"])
        .arg(&source))
    .await;
    run(git().current_dir(&source).args([
        "-c",
        "user.name=SCIM Fixture",
        "-c",
        "user.email=scim@example.test",
        "commit",
        "--allow-empty",
        "-m",
        "Seed private repository",
    ]))
    .await;
    let head = run(git().current_dir(&source).args(["rev-parse", "HEAD"])).await;
    run(git_authenticated(&alice.1).current_dir(&source).args([
        "push",
        &format!("{origin_a}/git/team/private.git"),
        "main",
    ]))
    .await;
    *fixture.provider.mode.lock().await = "member".into();
    let bob = sign_in(client, origin_a, "read").await;
    let replicas = [origin_a, origin_b];
    for origin in replicas {
        check_access(client, origin, &bob, Some(&head)).await;
    }

    // Deactivate through the other replica after both have served these credentials.
    set_active(client, origin_b, &bob_id, false).await;
    for origin in replicas {
        check_access(client, origin, &bob, None).await;
        check_access(client, origin, &alice, Some(&head)).await;
    }
    stop(second_process).await;
    let second_process = fixture.start(&fixture.second).await;
    check_access(client, origin_b, &bob, None).await;

    set_active(client, origin_a, &bob_id, true).await;
    let fresh = sign_in(client, origin_b, "read").await;
    for origin in replicas {
        check_access(client, origin, &bob, None).await;
        check_access(client, origin, &fresh, Some(&head)).await;
    }
    stop(second_process).await;
    stop(first_process).await;
}
