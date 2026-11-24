use std::{path::Path, time::Duration};

use axum::http::{StatusCode, header};
use futures_util::{StreamExt, stream};
use reqwest::{Body, Client};
use serde_json::{Value, json};
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use super::process::{
    ProcessFixture, get_session, git, git_authenticated, login, provision, run, stop,
};

async fn write(client: &Client, origin: &str, path: &str, cookie: &str, body: Value) -> Value {
    let session = get_session(client, origin, cookie).await;
    client
        .post(format!("{origin}{path}"))
        .header(header::ORIGIN, origin)
        .header(header::COOKIE, cookie)
        .header("x-csrf-token", session["csrf"].as_str().unwrap())
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn assert_head(remote: &str, token: &str, head: &str) {
    let observed =
        run(git_authenticated(token).args(["ls-remote", remote, "refs/heads/main"])).await;
    assert_eq!(observed, format!("{head}\trefs/heads/main"));
}

async fn assert_revoked(client: &Client, remote: &str, token: &str) {
    for service in ["git-upload-pack", "git-receive-pack"] {
        assert_eq!(
            client
                .get(format!("{remote}/info/refs?service={service}"))
                .basic_auth("crab", Some(token))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}

async fn wait_for_receive(data: &Path, prefix: &[u8]) {
    // A file containing this request proves the replica authorized it and
    // consumed its first chunk before the other replica revokes the token.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for session in std::fs::read_dir(data.join("sessions")).unwrap() {
                let transfers = session.unwrap().path().join("transfers");
                let Ok(transfers) = std::fs::read_dir(transfers) else {
                    continue;
                };
                for transfer in transfers {
                    let file = transfer.unwrap().path().join("receive");
                    if std::fs::read(file).is_ok_and(|body| body == prefix) {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("server must accept and stage a receive body before revocation");
}

async fn commit(source: &Path, message: &str) -> String {
    run(git().current_dir(source).args([
        "-c",
        "user.name=Automation Fixture",
        "-c",
        "user.email=automation@example.test",
        "commit",
        "--allow-empty",
        "-m",
        message,
    ]))
    .await;
    run(git().current_dir(source).args(["rev-parse", "HEAD"])).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a built crab-server and an isolated RustFS bucket"]
async fn rustfs_automation_revocation_blocks_admitted_push_and_survives_restart() {
    let fixture = ProcessFixture::new("automation").await;
    let client = &fixture.client;
    let first = &fixture.first;
    let second = &fixture.second;
    let first_process = fixture.start(first).await;
    let organization = Uuid::now_v7().to_string();
    let bootstrap = fixture.local.path().join("organization.json");
    std::fs::write(
        &bootstrap,
        serde_json::to_vec(&json!({
            "owner":{"issuer":fixture.provider.issuer,"subject":"alice-id"},
            "name":"Release engineering",
        }))
        .unwrap(),
    )
    .unwrap();
    run(fixture
        .command(first)
        .args([
            "cells",
            "bootstrap-organization",
            "--organization",
            &organization,
            "--input-file",
        ])
        .arg(bootstrap))
    .await;
    let org = format!("/api/organizations/{organization}");
    provision(client, &first.origin, "alice-id").await;
    let cookie = login(
        client,
        &first.origin,
        &format!("/-/organizations/{organization}"),
    )
    .await;
    let repo = write(
        client,
        &first.origin,
        &format!("{org}/repositories/creation"),
        &cookie,
        json!({"request_id":Uuid::now_v7(),"name":"library","description":""}),
    )
    .await;
    let path = format!("/git/{}/library.git", repo["owner"].as_str().unwrap());
    let remote_a = format!("{}{path}", first.origin);
    let remote_b = format!("{}{path}", second.origin);
    let automation = format!("{org}/automation");
    let change = async |origin: &str, version: u64, edit: Value| {
        write(
            client,
            origin,
            &automation,
            &cookie,
            json!({"expected_version":version,"edit":edit}),
        )
        .await
    };
    let created = change(
        &first.origin,
        0,
        json!({
            "kind":"create","name":"Release pipeline",
            "grants":[{"repository":repo["id"],"access":"write"}],
        }),
    )
    .await;
    let identity = &created["identity"];
    let issue = || json!({"kind":"issue","identity":identity,"hours":1});
    let issued = change(&first.origin, 1, issue()).await;
    let token = issued["token"].as_str().unwrap();
    let listed: Value = client
        .get(format!("{}{automation}", first.origin))
        .header(header::COOKIE, &cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let [listed_token] = listed["identities"][0]["tokens"]
        .as_array()
        .unwrap()
        .as_slice()
    else {
        panic!("only the first token should have been issued");
    };
    let replacement = change(&first.origin, 2, issue()).await;
    let fresh = replacement["token"].as_str().unwrap();

    let source = fixture.local.path().join("git");
    run(git()
        .args(["init", "--initial-branch=main", "--object-format=sha1"])
        .arg(&source))
    .await;
    let original = commit(&source, "Seed library").await;
    run(git_authenticated(token)
        .current_dir(&source)
        .args(["push", &remote_a, "main"]))
    .await;
    let second_process = fixture.start(second).await;
    for remote in [&remote_a, &remote_b] {
        assert_head(remote, token, &original).await;
    }
    let updated = commit(&source, "Release update").await;
    let pack = git()
        .current_dir(&source)
        .args(["pack-objects", "--stdout", "--all"])
        .output()
        .await
        .unwrap();
    assert!(pack.status.success() && pack.stdout.starts_with(b"PACK"));
    let update = format!("{original} {updated} refs/heads/main\0report-status object-format=sha1");
    let prefix = format!("{:04x}{update}0000", update.len() + 4).into_bytes();
    let (resume, paused) = tokio::sync::oneshot::channel();
    let body = stream::iter([Ok::<_, std::io::Error>(prefix.clone())]).chain(stream::once(async {
        paused.await.unwrap();
        Ok(pack.stdout)
    }));
    let request = client
        .post(format!("{remote_b}/git-receive-pack"))
        .timeout(Duration::from_secs(45))
        .basic_auth("crab", Some(token))
        .header(
            header::CONTENT_TYPE,
            "application/x-git-receive-pack-request",
        )
        .body(Body::wrap_stream(body));
    let push = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
    wait_for_receive(&second.data, &prefix).await;

    change(
        &first.origin,
        3,
        json!({"kind":"revoke","identity":identity,"token":listed_token["id"]}),
    )
    .await;
    resume.send(()).unwrap();
    assert_eq!(push.await.unwrap().unwrap().status(), StatusCode::NOT_FOUND);

    for remote in [&remote_a, &remote_b] {
        assert_revoked(client, remote, token).await;
        assert_head(remote, fresh, &original).await;
    }
    stop(second_process).await;
    let second_process = fixture.start(second).await;
    assert_revoked(client, &remote_b, token).await;
    assert_head(&remote_b, fresh, &original).await;

    run(git_authenticated(fresh)
        .current_dir(&source)
        .args(["push", &remote_b, "main"]))
    .await;
    for remote in [&remote_a, &remote_b] {
        assert_head(remote, fresh, &updated).await;
    }
    stop(second_process).await;
    stop(first_process).await;
}
