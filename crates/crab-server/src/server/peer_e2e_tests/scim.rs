use std::{net::TcpListener, path::Path, process::Stdio, time::Duration};

use axum::http::{StatusCode, header};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use crate::test_support::{
    http::{TEST_SCIM_TOKEN, cookie_pair, start_provider},
    tls::IdentityFiles,
};

const REPO: &str = "/api/repos/team/private/refs";

fn server_command(binary: &Path, config: &Path) -> Command {
    let mut command = Command::new(binary);
    command.arg("--config").arg(config).kill_on_drop(true);
    command
}

async fn run(command: &mut Command) -> String {
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

async fn start(binary: &Path, config: &Path, client: &Client, origin: &str) -> Child {
    let log_path = config.with_extension("log");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut process = server_command(binary, config)
        .arg("serve")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    let started = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if process.try_wait().unwrap().is_some() {
                return false;
            }
            if client
                .get(format!("{origin}/api/session"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert_eq!(
        started.ok(),
        Some(true),
        "Crab replica did not become ready\n{}",
        std::fs::read_to_string(log_path).unwrap_or_default()
    );
    process
}

async fn stop(mut process: Child) {
    run(Command::new("kill").args(["-TERM", &process.id().unwrap().to_string()])).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(60), process.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

async fn provision(client: &Client, origin: &str, subject: &str) -> String {
    let user: Value = client
        .post(format!("{origin}/scim/v2/Users"))
        .bearer_auth(TEST_SCIM_TOKEN)
        .header(header::CONTENT_TYPE, "application/scim+json")
        .json(&json!({
            "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
            "externalId": subject, "userName": format!("{subject}@example.test"),
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    user["id"].as_str().unwrap().to_owned()
}

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

async fn get_session(client: &Client, origin: &str, cookie: &str) -> Value {
    client
        .get(format!("{origin}/api/session"))
        .header(header::COOKIE, cookie)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn sign_in(client: &Client, origin: &str, access: &str) -> (String, String) {
    let flow = client
        .get(format!("{origin}/auth/login?return_to=%2Fteam%2Fprivate"))
        .send()
        .await
        .unwrap();
    assert_eq!(flow.status(), StatusCode::SEE_OTHER);
    let authorize = client
        .get(flow.headers()[header::LOCATION].to_str().unwrap())
        .send()
        .await
        .unwrap();
    let callback = client
        .get(authorize.headers()[header::LOCATION].to_str().unwrap())
        .header(header::COOKIE, cookie_pair(&flow, "crab_login"))
        .send()
        .await
        .unwrap();
    assert_eq!(callback.headers()[header::LOCATION], "/team/private");
    let cookie = cookie_pair(&callback, "crab_session");
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

fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    command
}

fn git_authenticated(token: &str) -> Command {
    let mut command = git();
    command
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!(
                "Authorization: Basic {}",
                STANDARD.encode(format!("crab:{token}"))
            ),
        );
    command
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
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let binary = std::path::PathBuf::from(required("CRAB_HTTP_CELL_TEST_BINARY"));
    let bucket = required("CRAB_HTTP_CELL_TEST_BUCKET");
    let prefix = required("CRAB_HTTP_CELL_TEST_PREFIX");
    let endpoint = url::Url::parse(&required("AWS_ENDPOINT_URL_S3")).unwrap();
    assert!(endpoint.host_str() == Some("127.0.0.1") && endpoint.scheme() == "http");
    assert!(!prefix.trim_matches('/').is_empty());
    let storage = format!("s3://{bucket}/{prefix}/scim-{}", Uuid::now_v7());
    let local = tempfile::tempdir_in(binary.parent().unwrap()).unwrap();
    let files = IdentityFiles::generate();
    let (provider, issuer_task) = start_provider(0).await;
    let _issuer_task = AbortOnDropHandle::new(issuer_task);
    let state_key = local.path().join("state-key");
    let scim_key = local.path().join("scim-token");
    std::fs::write(&state_key, [42_u8; 32]).unwrap();
    std::fs::write(&scim_key, TEST_SCIM_TOKEN).unwrap();
    let members = local.path().join("members.toml");
    std::fs::write(
        &members,
        r#"
[[members]]
subject = "alice-id"
name = "Alice"
access = "admin"
[[members]]
subject = "bob-id"
name = "Bob"
access = "read"
"#,
    )
    .unwrap();

    let listeners: [TcpListener; 4] =
        std::array::from_fn(|_| TcpListener::bind("127.0.0.1:0").unwrap());
    let [api_a, management_a, api_b, management_b] =
        listeners.each_ref().map(|s| s.local_addr().unwrap());
    let [origin_a, origin_b] = [api_a, api_b].map(|a| format!("http://{a}"));
    let config = |name: &str, api, management, origin: &str| {
        let cells = files.config(format!("https://{management}").parse().unwrap());
        let path = local.path().join(format!("{name}.toml"));
        std::fs::write(
            &path,
            format!(
                r#"listen = "{api}"
management_listen = "{management}"
[storage]
url = "{storage}"
[cells]
data_dir = {:?}
local_disk_limit_bytes = 34359738368
peer_advertise = "{endpoint}"
peer_tls_server_name = "localhost"
peer_certificate = {:?}
peer_private_key = {:?}
peer_ca = {:?}
durability = "object"
[auth]
issuer = "{}"
client_id = "crab-browser"
public_url = "{origin}"
state_key_file = {state_key:?}
scim_token_file = {scim_key:?}
"#,
                local.path().join(name),
                cells.peer_certificate,
                cells.peer_private_key,
                cells.peer_ca,
                provider.issuer,
                endpoint = cells.peer_advertise
            ),
        )
        .unwrap();
        path
    };
    let first = config("first", api_a, management_a, &origin_a);
    let second = config("second", api_b, management_b, &origin_b);
    drop(listeners);

    run(server_command(&binary, &first)
        .args([
            "repository",
            "create",
            "--owner",
            "team",
            "--name",
            "private",
            "--prefix",
            "team/private",
            "--members-file",
        ])
        .arg(&members))
    .await;
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();
    let first_process = start(&binary, &first, &client, &origin_a).await;
    run(server_command(&binary, &first).args(["cells", "initialize-directory"])).await;
    let second_process = start(&binary, &second, &client, &origin_b).await;
    provision(&client, &origin_a, "alice-id").await;
    let bob_id = provision(&client, &origin_a, "bob-id").await;
    let alice = sign_in(&client, &origin_a, "write").await;
    let source = local.path().join("git");
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
    *provider.mode.lock().await = "member".into();
    let bob = sign_in(&client, &origin_a, "read").await;
    let replicas = [&origin_a, &origin_b];
    for origin in replicas {
        check_access(&client, origin, &bob, Some(&head)).await;
    }

    // Deactivate through the other replica after both have served these credentials.
    set_active(&client, &origin_b, &bob_id, false).await;
    for origin in replicas {
        check_access(&client, origin, &bob, None).await;
        check_access(&client, origin, &alice, Some(&head)).await;
    }
    stop(second_process).await;
    let second_process = start(&binary, &second, &client, &origin_b).await;
    check_access(&client, &origin_b, &bob, None).await;

    set_active(&client, &origin_a, &bob_id, true).await;
    let fresh = sign_in(&client, &origin_b, "read").await;
    for origin in replicas {
        check_access(&client, origin, &bob, None).await;
        check_access(&client, origin, &fresh, Some(&head)).await;
    }
    stop(second_process).await;
    stop(first_process).await;
}
