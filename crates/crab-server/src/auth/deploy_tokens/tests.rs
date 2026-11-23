use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::test_support::{
    git::{self, with_http_token as git_command},
    http::Harness,
};

const PATH: &str = "/api/repos/team/private/settings/deploy-tokens";

async fn seed(h: &Harness, alice: &str) -> (tempfile::TempDir, String, String) {
    let published: Value = h
        .write(
            Method::POST,
            "/api/git-token",
            alice,
            json!({
                "owner":"team","repository":"private","access":"write"
            }),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let writer = published["token"].as_str().unwrap().to_owned();
    let dir = tempfile::tempdir().unwrap();
    let path = format!("{}/git/team/private.git", h.origin);
    git::success(
        dir.path(),
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(dir.path().join("README.md"), "checkout proof\n").unwrap();
    git::success(dir.path(), &["add", "README.md"]).await;
    git::success(dir.path(), &["commit", "-m", "Seed checkout"]).await;
    assert!(git_command(dir.path(), &writer, &["push", &path, "main"]).await);
    (dir, path, writer)
}

#[tokio::test(flavor = "multi_thread")]
async fn deploy_token_survives_logout_and_revokes_git_and_lfs_checkout_individually() {
    let h = Harness::new(false).await;
    h.add_repository("sibling").await;
    let alice = h.login().await;
    let (work, remote, writer) = seed(&h, &alice).await;
    let lfs_content = b"automated checkout";
    let oid = format!("{:x}", Sha256::digest(lfs_content));
    let lfs = format!("{remote}/info/lfs/objects/{oid}?size={}", lfs_content.len());
    h.http
        .put(&lfs)
        .basic_auth("crab", Some(&writer))
        .body(&lfs_content[..])
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let create =
        json!({"expected_version":0,"edit":{"kind":"create","name":"Build service","days":7}});
    let issued: Value = h
        .write(Method::POST, PATH, &alice, create.clone())
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = issued["token"].as_str().unwrap();
    let data = h.json(PATH, &alice).await;
    assert_eq!(data["version"], 1);
    assert_eq!(data["tokens"][0]["name"], "Build service");
    assert_eq!(
        data["tokens"][0]["expires_at"].as_u64().unwrap()
            - data["tokens"][0]["created_at"].as_u64().unwrap(),
        7 * 86_400
    );
    assert!(data["tokens"][0].get("digest").is_none());
    assert!(!data.to_string().contains(token));

    let second: Value = h
        .write(
            Method::POST,
            PATH,
            &alice,
            json!({"expected_version":1,"edit":{"kind":"create","name":"Other service","days":1}}),
        )
        .await
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let other = second["token"].as_str().unwrap();
    let logout = h
        .write(Method::POST, "/auth/logout", &alice, json!({}))
        .await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);

    assert!(git_command(work.path(), token, &["clone", &remote, "ci-clone"]).await);
    assert_eq!(
        std::fs::read_to_string(work.path().join("ci-clone/README.md")).unwrap(),
        "checkout proof\n"
    );
    assert!(
        !git_command(
            work.path(),
            token,
            &["push", &remote, "HEAD:refs/heads/deploy-denied"]
        )
        .await
    );
    let sibling = format!("{}/git/team/sibling.git", h.origin);
    assert!(!git_command(work.path(), token, &["ls-remote", &sibling]).await);
    let response = h
        .http
        .get(&lfs)
        .basic_auth("crab", Some(token))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.error_for_status().unwrap().bytes().await.unwrap(),
        &lfs_content[..]
    );
    assert_eq!(
        h.http
            .put(&lfs)
            .basic_auth("crab", Some(token))
            .body(&lfs_content[..])
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.http
            .post(format!(
                "{}/api/repos/team/private/statuses/{}",
                h.origin,
                "1".repeat(40)
            ))
            .basic_auth("crab", Some(token))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    *h.provider.mode.lock().await = "member".into();
    let bob = h.login().await;
    assert_eq!(
        h.write(Method::POST, PATH, &bob, create.clone())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    *h.provider.mode.lock().await = "valid".into();
    let alice = h.login().await;
    h.write(
        Method::POST,
        PATH,
        &alice,
        json!({"expected_version":2,"edit":{"kind":"revoke","id":data["tokens"][0]["id"]}}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(Method::POST, PATH, &alice, create).await.status(),
        StatusCode::CONFLICT
    );
    assert!(!git_command(work.path(), token, &["ls-remote", &remote]).await);
    assert_eq!(
        h.http
            .get(&lfs)
            .basic_auth("crab", Some(token))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(git_command(work.path(), other, &["ls-remote", &remote]).await);
    h.write(
        Method::PUT,
        "/api/repos/team/private/settings/archive",
        &alice,
        json!({"expected_version":0,"archived":true,"repository":"team/private"}),
    )
    .await
    .error_for_status()
    .unwrap();
    assert_eq!(
        h.write(
            Method::POST,
            PATH,
            &alice,
            json!({
                "expected_version":3,"edit":{"kind":"create","name":"Too late","days":1}
            })
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let remaining = h.json(PATH, &alice).await;
    h.write(
        Method::POST,
        PATH,
        &alice,
        json!({
            "expected_version":3,"edit":{"kind":"revoke","id":remaining["tokens"][0]["id"]}
        }),
    )
    .await
    .error_for_status()
    .unwrap();
    assert!(!git_command(work.path(), other, &["ls-remote", &remote]).await);
    h.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires agent-browser and the built embedded UI with native Git and Cells"]
async fn native_browser_creates_and_revokes_ci_checkout_credentials() {
    let h = Harness::new(false).await;
    let alice = h.login().await;
    let (work, remote, _) = seed(&h, &alice).await;
    let browser = crate::test_support::http::browser::Browser::new("deploy-tokens");
    browser
        .run(&["open", &format!("{}/team/private/settings", h.origin)])
        .await;
    browser
        .run(&["wait", "--text", "Continue to sign in"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Continue to sign in",
        ])
        .await;
    browser
        .run(&["wait", "a[href$='/settings/deploy-tokens']"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "link",
            "click",
            "--name",
            "Deploy tokens",
            "--exact",
        ])
        .await;
    browser.run(&["wait", "#deploy-token-name"]).await;
    browser
        .run(&["fill", "#deploy-token-name", "Nightly checkout"])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Create deploy token",
        ])
        .await;
    browser.run(&["wait", "#deploy-token-secret"]).await;
    let credential = browser.run(&["get", "value", "#deploy-token-secret"]).await;
    assert!(
        git_command(
            work.path(),
            credential.trim(),
            &["clone", &remote, "browser-clone"]
        )
        .await
    );
    browser
        .run(&[
            "find", "role", "button", "click", "--name", "Done", "--exact",
        ])
        .await;
    browser.run(&["wait", "--text", "Nightly checkout"]).await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Revoke Nightly checkout",
        ])
        .await;
    browser
        .run(&[
            "find",
            "role",
            "button",
            "click",
            "--name",
            "Confirm revoke",
        ])
        .await;
    browser
        .run(&["wait", "--text", "No active deploy tokens."])
        .await;
    assert!(!git_command(work.path(), credential.trim(), &["ls-remote", &remote]).await);
    drop(browser);
    h.close().await;
}
