use std::{
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use axum::http::{StatusCode, header};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use crate::test_support::{
    http::{Provider, TEST_SCIM_TOKEN, cookie_pair, start_provider},
    tls::IdentityFiles,
};

pub(super) struct Replica {
    pub(super) config: PathBuf,
    pub(super) data: PathBuf,
    pub(super) origin: String,
    management: SocketAddr,
}

pub(super) struct ProcessFixture {
    binary: PathBuf,
    pub(super) local: tempfile::TempDir,
    pub(super) provider: Arc<Provider>,
    pub(super) client: Client,
    pub(super) first: Replica,
    pub(super) second: Replica,
    tls: IdentityFiles,
    _issuer_task: AbortOnDropHandle<()>,
}

impl ProcessFixture {
    pub(super) async fn new(scenario: &str) -> Self {
        let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
        let binary = std::path::PathBuf::from(required("CRAB_HTTP_CELL_TEST_BINARY"));
        let bucket = required("CRAB_HTTP_CELL_TEST_BUCKET");
        let prefix = required("CRAB_HTTP_CELL_TEST_PREFIX");
        let endpoint = url::Url::parse(&required("AWS_ENDPOINT_URL_S3")).unwrap();
        assert!(endpoint.host_str() == Some("127.0.0.1") && endpoint.scheme() == "http");
        assert!(!prefix.trim_matches('/').is_empty());
        let storage = format!("s3://{bucket}/{prefix}/{scenario}-{}", Uuid::now_v7());
        let local = tempfile::tempdir_in(binary.parent().unwrap()).unwrap();
        let files = IdentityFiles::generate();
        let (provider, issuer_task) = start_provider(0).await;
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
            let config = local.path().join(format!("{name}.toml"));
            let data = local.path().join(name);
            std::fs::write(
                &config,
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
                    data,
                    cells.peer_certificate,
                    cells.peer_private_key,
                    cells.peer_ca,
                    provider.issuer,
                    endpoint = cells.peer_advertise
                ),
            )
            .unwrap();
            Replica {
                config,
                data,
                origin: origin.into(),
                management,
            }
        };
        let first = config("first", api_a, management_a, &origin_a);
        let second = config("second", api_b, management_b, &origin_b);
        drop(listeners);

        run(server_command(&binary, &first.config)
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
        Self {
            binary,
            local,
            provider,
            client,
            first,
            second,
            tls: files,
            _issuer_task: AbortOnDropHandle::new(issuer_task),
        }
    }

    pub(super) fn command(&self, replica: &Replica) -> Command {
        server_command(&self.binary, &replica.config)
    }

    pub(super) async fn start(&self, replica: &Replica) -> Child {
        start(&self.binary, &replica.config, &self.client, &replica.origin).await
    }

    pub(super) async fn handoff(&self, replica: &Replica, process: Child) {
        let endpoint = format!("https://{}", replica.management).parse().unwrap();
        let tls = self.tls.config(endpoint);
        let mut pem = std::fs::read(tls.peer_certificate).unwrap();
        pem.extend(std::fs::read(tls.peer_private_key).unwrap());
        let identity = reqwest::Identity::from_pem(&pem).unwrap();
        let ca = reqwest::Certificate::from_pem(&std::fs::read(tls.peer_ca).unwrap()).unwrap();
        let client = Client::builder()
            .use_rustls_tls()
            .no_proxy()
            .tls_built_in_root_certs(false)
            .identity(identity)
            .add_root_certificate(ca)
            .resolve("localhost", replica.management)
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let port = replica.management.port();
        let response = client
            .post(format!(
                "https://localhost:{port}/internal/cells/v1/scale-down"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        wait(process).await;
    }
}

fn server_command(binary: &Path, config: &Path) -> Command {
    let mut command = Command::new(binary);
    command.arg("--config").arg(config).kill_on_drop(true);
    command
}

pub(super) async fn run(command: &mut Command) -> String {
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

pub(super) async fn stop(process: Child) {
    run(Command::new("kill").args(["-TERM", &process.id().unwrap().to_string()])).await;
    wait(process).await;
}

async fn wait(mut process: Child) {
    assert!(
        tokio::time::timeout(Duration::from_secs(60), process.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

pub(super) async fn provision(client: &Client, origin: &str, subject: &str) -> String {
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

pub(super) async fn get_session(client: &Client, origin: &str, cookie: &str) -> Value {
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

pub(super) async fn login(client: &Client, origin: &str, return_to: &str) -> String {
    let flow = client
        .get(format!("{origin}/auth/login"))
        .query(&[("return_to", return_to)])
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
    assert_eq!(callback.headers()[header::LOCATION], return_to);
    cookie_pair(&callback, "crab_session")
}

pub(super) fn git() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    command
}

pub(super) fn git_authenticated(token: &str) -> Command {
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
