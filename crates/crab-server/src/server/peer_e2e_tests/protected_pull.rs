use std::{fs::OpenOptions, io::Write as _, time::Duration};

use axum::http::{StatusCode, header};
use reqwest::{Client, Method, Response};
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, net::TcpStream, process::Command};
use uuid::Uuid;

use super::process::{
    ProcessFixture, get_session, git, git_authenticated, login, provision, run, stop,
};
use crate::test_support::{git::success as local_git, http::github_workflow_request};

const ROOT: &str = "/api/repos/team/private";
const PULL: &str = "/api/repos/team/private/pulls/1";
const MERGE: &str = "/api/repos/team/private/pulls/1/merge";
const APP_SECRET: &str = "fixture-only-github-app-push-webhook-secret";

async fn json_response(response: Response, status: StatusCode) -> Value {
    let received = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(received, status, "error code: {}", body["error"]["code"]);
    body
}

fn merge_input(pull: &Value) -> Value {
    json!({
        "request_id":Uuid::now_v7(), "method":"fast_forward",
        "version":pull["version"],"base_oid":pull["base_oid"],"head_oid":pull["head_oid"],
    })
}

async fn configure_app(fixture: &ProcessFixture, repository: Uuid) {
    let key = fixture.local.path().join("app-key.pem");
    let secret = fixture.local.path().join("app-secret");
    run(Command::new("openssl")
        .args(["genpkey", "-algorithm", "RSA", "-out"])
        .arg(&key))
    .await;
    std::fs::write(&secret, APP_SECRET).unwrap();
    for replica in [&fixture.first, &fixture.second] {
        write!(
            OpenOptions::new()
                .append(true)
                .open(&replica.config)
                .unwrap(),
            r#"
[github_actions]
app_id = 7
private_key_file = {key:?}
webhook_secret_file = {secret:?}
[[github_actions.bindings]]
crab_repository = "{repository}"
installation = 42
github_repository = 123
workflow = 456
branch = "main"
"#,
        )
        .unwrap();
    }
}

struct BrowserSession<'a> {
    client: &'a Client,
    repository: Uuid,
    cookie: String,
    csrf: String,
}

impl<'a> BrowserSession<'a> {
    async fn new(client: &'a Client, repository: Uuid, origin: &str) -> Self {
        let cookie = login(client, origin, "/team/private/pull/1").await;
        let session = get_session(client, origin, &cookie).await;
        let csrf = session["csrf"].as_str().unwrap().to_owned();
        Self {
            client,
            repository,
            cookie,
            csrf,
        }
    }

    fn request(&self, origin: &str, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{origin}{path}"))
            .header(header::COOKIE, &self.cookie)
            .header(header::ORIGIN, origin)
            .header("x-csrf-token", &self.csrf)
            .header("x-crab-repository-id", self.repository.to_string())
    }

    async fn get(&self, origin: &str, path: &str) -> Value {
        json_response(
            self.request(origin, Method::GET, path)
                .send()
                .await
                .unwrap(),
            StatusCode::OK,
        )
        .await
    }

    async fn write(&self, origin: &str, method: Method, path: &str, body: Value) -> Response {
        self.request(origin, method, path)
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn unread_merge_reply(&self, origin: &str, body: &Value) -> TcpStream {
        let host = origin.strip_prefix("http://127.0.0.1:").unwrap();
        let mut connection = TcpStream::connect(("127.0.0.1", host.parse::<u16>().unwrap()))
            .await
            .unwrap();
        let body = serde_json::to_string(body).unwrap();
        let request = format!(
            "POST {MERGE} HTTP/1.1\r\nHost: 127.0.0.1:{host}\r\nOrigin: {origin}\r\nCookie: {}\r\nX-Csrf-Token: {}\r\nX-Crab-Repository-Id: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.cookie,
            self.csrf,
            self.repository,
            body.len(),
        );
        connection.write_all(request.as_bytes()).await.unwrap();
        connection
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a built crab-server and an isolated RustFS bucket"]
async fn rustfs_protected_pull_enforces_exact_review_and_app_check_then_recovers_on_peer() {
    let fixture = ProcessFixture::new("protected-pull").await;
    let client = &fixture.client;
    let first = &fixture.first;
    let second = &fixture.second;
    let a = first.origin.as_str();
    let b = second.origin.as_str();
    let catalog: Value =
        serde_json::from_str(&run(fixture.command(first).args(["repository", "list"])).await)
            .unwrap();
    let [repo] = catalog["repositories"].as_array().unwrap().as_slice() else {
        panic!("expected the fixture repository");
    };
    let repository = repo["id"].as_str().unwrap().parse().unwrap();
    configure_app(&fixture, repository).await;
    let first_process = fixture.start(first).await;
    run(fixture
        .command(first)
        .args(["cells", "initialize-directory"]))
    .await;
    for subject in ["alice-id", "bob-id"] {
        provision(client, a, subject).await;
    }
    let alice = BrowserSession::new(client, repository, a).await;
    *fixture.provider.mode.lock().await = "member".into();
    let bob = BrowserSession::new(client, repository, a).await;
    let token = json_response(
        alice
            .write(
                a,
                Method::POST,
                "/api/git-token",
                json!({"owner":"team","repository":"private","access":"write"}),
            )
            .await,
        StatusCode::OK,
    )
    .await;
    let token = token["token"].as_str().unwrap();
    let remote_a = format!("{a}/git/team/private.git");
    let remote_b = format!("{b}/git/team/private.git");
    let source = fixture.local.path().join("git");
    run(git()
        .args(["init", "--initial-branch=main", "--object-format=sha1"])
        .arg(&source))
    .await;
    let commit = async |content: &str, subject: &str| {
        std::fs::write(source.join("README.md"), content).unwrap();
        local_git(&source, &["add", "README.md"]).await;
        local_git(&source, &["commit", "-m", subject]).await;
        local_git(&source, &["rev-parse", "HEAD"]).await
    };
    let base = commit("base\n", "Seed protected branch").await;
    let old_head = commit("base\nreviewed\n", "Propose change").await;
    run(git_authenticated(token).current_dir(&source).args([
        "push",
        &remote_a,
        &format!("{base}:refs/heads/main"),
        "HEAD:refs/heads/feature",
    ]))
    .await;
    let connection = Uuid::now_v7();
    let integration = json_response(alice.write(a, Method::POST, &format!("{ROOT}/settings/integrations"),
        json!({"kind":"connect","id":connection,"name":"ci/test","github_repository":123,"workflow":456})).await,
        StatusCode::OK).await;
    assert_eq!(integration["connection"]["app_managed"], true);
    alice.write(a, Method::PUT, &format!("{ROOT}/settings/branch-protections"),
        json!({"expected_version":0,"rules":[{"branch":"main","required_approvals":1,"required_integrations":[connection]}]}))
        .await.error_for_status().unwrap();
    let created = json_response(
        alice
            .write(
                a,
                Method::POST,
                &format!("{ROOT}/pulls"),
                json!({
                    "request_id":Uuid::now_v7(),"title":"Protect the deploy branch","body":"",
                    "base_ref":"refs/heads/main","head_ref":"refs/heads/feature",
                    "expected":{"base_oid":base,"head_oid":old_head},
                }),
            )
            .await,
        StatusCode::CREATED,
    )
    .await;
    assert_eq!(created["number"], 1);
    let second_process = fixture.start(second).await;

    let set_reviewer = async |access: &str| {
        let path = format!("{ROOT}/members");
        let mut page = alice.get(a, &path).await;
        for member in page["members"].as_array_mut().unwrap() {
            if member["subject"] == "bob-id" {
                member["access"] = json!(access);
            }
        }
        alice
            .write(
                a,
                Method::PUT,
                &path,
                json!({"expected_revision":page["revision"],"members":page["members"],"groups":page["groups"]}),
            )
            .await
            .error_for_status()
            .unwrap();
    };
    let approve = async |oid: &str| {
        bob.write(b, Method::POST, &format!("{PULL}/reviews"), json!({
            "request_id":Uuid::now_v7(),"state":"approved","body":"Reviewed this commit","commit_oid":oid,
        })).await.error_for_status().unwrap();
    };
    let app_report = async |oid: &str, run: u64, installation: u64| {
        let payload = json!({
            "action":"completed","installation":{"id":installation},
            "repository":{"id":123,"full_name":"example/mirror"},
            "workflow_run":{"id":run,"workflow_id":456,"run_number":run,"run_attempt":1,
                "head_sha":oid,"event":"push","status":"completed","conclusion":"success"},
        });
        let response = github_workflow_request(
            client,
            &format!("{b}/webhooks/github-actions/app"),
            APP_SECRET,
            Uuid::now_v7(),
            &payload,
            &payload,
        )
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    };
    let merge_error = async |input: Value, code: &str| {
        let response = alice.write(b, Method::POST, MERGE, input).await;
        let rejected = json_response(response, StatusCode::CONFLICT).await;
        assert_eq!(rejected["error"]["code"], code);
    };

    set_reviewer("write").await;
    approve(&old_head).await;
    alice
        .write(
            a,
            Method::POST,
            &format!("{ROOT}/check-runs"),
            json!({
                "request_id":Uuid::now_v7(),"head_sha":old_head,"name":"ci/test",
                "status":"completed","conclusion":"success","details_url":null,
                "output":{"title":"Manual report","summary":"Same name.","text":null},
            }),
        )
        .await
        .error_for_status()
        .unwrap();
    app_report(&old_head, 10, 43).await;
    app_report(&base, 11, 42).await;
    let untrusted = alice.get(b, PULL).await;
    assert_eq!(untrusted["merge_requirements"]["approvals"], 1);
    assert_eq!(untrusted["merge_requirements"]["checks_satisfied"], false);
    merge_error(merge_input(&untrusted), "merge_blocked").await;

    app_report(&old_head, 12, 42).await;
    let ready = alice.get(b, PULL).await;
    assert_eq!(ready["can_merge"], true);
    let check = &ready["merge_requirements"]["checks"][0];
    assert_eq!(check["provider"], "github_actions");
    assert_eq!(check["head_sha"], old_head);
    assert_eq!(check["integration_id"], json!(connection));
    set_reviewer("read").await;
    let revoked = alice.get(b, PULL).await;
    assert_eq!(revoked["merge_requirements"]["approvals"], 0);
    assert_eq!(revoked["merge_requirements"]["checks_satisfied"], true);
    merge_error(merge_input(&revoked), "merge_blocked").await;

    set_reviewer("write").await;
    let head = commit("base\nreviewed\nupdated\n", "Advance reviewed branch").await;
    run(git_authenticated(token).current_dir(&source).args([
        "push",
        &remote_b,
        "HEAD:refs/heads/feature",
    ]))
    .await;
    merge_error(merge_input(&ready), "merge_conflict").await;
    let moved = alice.get(b, PULL).await;
    assert_eq!(moved["merge_requirements"]["approvals"], 0);
    assert_eq!(moved["merge_requirements"]["checks_satisfied"], false);
    let observed =
        run(git_authenticated(token).args(["ls-remote", &remote_b, "refs/heads/main"])).await;
    assert_eq!(observed, format!("{base}\trefs/heads/main"));

    approve(&head).await;
    app_report(&head, 13, 42).await;
    let approved = alice.get(a, PULL).await;
    assert_eq!(approved["can_merge"], true);
    let direct = git_authenticated(token)
        .current_dir(&source)
        .args(["push", &remote_b, "HEAD:refs/heads/main"])
        .output()
        .await
        .unwrap();
    assert!(
        !direct.status.success(),
        "the protected branch requires a PR"
    );
    let merge = merge_input(&approved);
    // Keep the response unread and observe publication through the other
    // process. The caller only knows the original request, not its result.
    let unread_reply = alice.unread_merge_reply(a, &merge).await;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let observed =
                run(git_authenticated(token).args(["ls-remote", &remote_b, "refs/heads/main"]))
                    .await;
            if observed == format!("{head}\trefs/heads/main") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("peer must advertise the committed merge");
    drop(unread_reply);
    fixture.handoff(first, first_process).await;

    let recovered = json_response(
        alice.write(b, Method::POST, MERGE, merge.clone()).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(recovered["state"], "merged");
    assert_eq!(recovered["merge"]["commit_oid"], head);
    let replay = json_response(
        alice.write(b, Method::POST, MERGE, merge).await,
        StatusCode::OK,
    )
    .await;
    assert_eq!(replay["merged_at"], recovered["merged_at"]);
    assert_eq!(replay["merge"], recovered["merge"]);
    run(git_authenticated(token)
        .current_dir(&source)
        .args(["fetch", &remote_b, "main"]))
    .await;
    assert_eq!(local_git(&source, &["rev-parse", "FETCH_HEAD"]).await, head);
    assert_eq!(
        local_git(&source, &["show", "FETCH_HEAD:README.md"]).await,
        "base\nreviewed\nupdated"
    );
    stop(second_process).await;
}
