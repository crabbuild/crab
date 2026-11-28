//! Drives the real GitHub CLI against Crab:
//!
//! ```sh
//! cargo test -p crab-server --lib github::tests::gh -- --ignored --nocapture
//! ```
//!
//! gh reaches Enterprise hosts over HTTPS on port 443, so `crab.test` is
//! served through two loopback relays: gh's own `http_unix_socket` setting
//! carries API traffic, and an HTTPS CONNECT proxy with a throwaway CA
//! carries Git, so remotes keep their real `https://crab.test/` URLs. Both
//! relays pass bytes through unchanged except the first request's Host.
//! Every gh and Git process runs with an isolated home and gh config, and
//! the token is stored in that config instead of the system keychain.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixListener},
};

use crate::test_support::http::Harness;

const HOST: &str = "crab.test";

/// Reads one HTTP head and returns it with the bytes read past its end.
async fn head<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = buffer.split_off(end + 4);
            return Ok((String::from_utf8_lossy(&buffer).into_owned(), rest));
        }
    }
}

/// Forwards one request to the server under its own Host. `Connection:
/// close` makes clients open a fresh relay for every request.
async fn relay<S: AsyncRead + AsyncWrite + Unpin>(
    mut client: S,
    upstream: SocketAddr,
) -> std::io::Result<()> {
    let (request, rest) = head(&mut client).await?;
    let mut lines = request.trim_end().split("\r\n");
    let mut rewritten = format!(
        "{}\r\nHost: {upstream}\r\n",
        lines.next().unwrap_or_default()
    );
    for line in lines {
        let lower = line.to_ascii_lowercase();
        if !lower.starts_with("host:") && !lower.starts_with("connection:") {
            rewritten.push_str(line);
            rewritten.push_str("\r\n");
        }
    }
    rewritten.push_str("Connection: close\r\n\r\n");
    let mut server = TcpStream::connect(upstream).await?;
    server.write_all(rewritten.as_bytes()).await?;
    server.write_all(&rest).await?;
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
}

fn openssl(directory: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("openssl")
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A CA and a `crab.test` server certificate, trusted only by this test's Git.
fn certificates(directory: &Path) -> tokio_rustls::TlsAcceptor {
    openssl(
        directory,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=Crab gh test CA",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign",
            "-keyout",
            "ca.key",
            "-out",
            "ca.crt",
        ],
    );
    openssl(
        directory,
        &[
            "req",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=crab.test",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
        ],
    );
    std::fs::write(
        directory.join("server.ext"),
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:crab.test\n",
    )
    .unwrap();
    openssl(
        directory,
        &[
            "x509",
            "-req",
            "-days",
            "1",
            "-in",
            "server.csr",
            "-CA",
            "ca.crt",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-extfile",
            "server.ext",
            "-out",
            "server.crt",
        ],
    );
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let certificates = rustls_pemfile::certs(
        &mut std::fs::read(directory.join("server.crt"))
            .unwrap()
            .as_slice(),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let key = rustls_pemfile::private_key(
        &mut std::fs::read(directory.join("server.key"))
            .unwrap()
            .as_slice(),
    )
    .unwrap()
    .unwrap();
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

struct Cli {
    directory: tempfile::TempDir,
    environment: Vec<(String, String)>,
    token: String,
}

impl Cli {
    fn home(&self) -> PathBuf {
        self.directory.path().join("home")
    }

    async fn run_in(
        &self,
        cwd: &Path,
        program: &str,
        arguments: &[&str],
        stdin: Option<&str>,
    ) -> String {
        let mut command = tokio::process::Command::new(program);
        command
            .args(arguments)
            .current_dir(cwd)
            .env_clear()
            .envs(self.environment.iter().map(|(name, value)| (name, value)))
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Some(input) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .await
                .unwrap();
        }
        let output = child.wait_with_output().await.unwrap();
        let redact = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&self.token, "[token]");
        let (stdout, stderr) = (redact(&output.stdout), redact(&output.stderr));
        println!("$ {program} {}\n{stdout}{stderr}", arguments.join(" "));
        assert!(
            output.status.success(),
            "{program} {arguments:?} failed:\n{stdout}\n{stderr}"
        );
        stdout
    }

    async fn gh(&self, cwd: &Path, arguments: &[&str]) -> String {
        self.run_in(cwd, "gh", arguments, None).await
    }

    async fn git(&self, cwd: &Path, arguments: &[&str]) -> String {
        self.run_in(cwd, "git", arguments, None).await
    }
}

async fn start(h: &Harness, token: &str) -> Cli {
    let upstream: SocketAddr = h.origin.trim_start_matches("http://").parse().unwrap();
    let directory = tempfile::Builder::new().prefix("gh").tempdir().unwrap();
    let root = directory.path().to_owned();
    // Unix socket addresses are limited to 104 bytes on macOS.
    assert!(
        root.join("gh.sock").as_os_str().len() < 100,
        "set TMPDIR to a shorter directory"
    );
    for path in ["home", "gh", "xdg", "work"] {
        std::fs::create_dir_all(root.join(path)).unwrap();
    }
    let acceptor = certificates(&root);

    let socket = root.join("gh.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(relay(stream, upstream));
        }
    });

    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = proxy.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let (connect, _) = head(&mut stream).await?;
                if !connect.starts_with(&format!("CONNECT {HOST}:443 ")) {
                    return stream.write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
                }
                stream
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                relay(acceptor.accept(stream).await?, upstream).await
            });
        }
    });

    std::fs::write(
        root.join("gh/config.yml"),
        format!(
            "version: 1\ngit_protocol: https\nprompt: disabled\nhttp_unix_socket: {}\n",
            socket.display()
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("home/.gitconfig"),
        "[user]\n\tname = Alice\n\temail = alice@example.test\n[init]\n\tdefaultBranch = main\n",
    )
    .unwrap();
    let path = |name: &str| root.join(name).display().to_string();
    let environment = vec![
        ("PATH".into(), std::env::var("PATH").unwrap_or_default()),
        ("HOME".into(), path("home")),
        ("GH_CONFIG_DIR".into(), path("gh")),
        ("XDG_CONFIG_HOME".into(), path("xdg")),
        ("XDG_STATE_HOME".into(), path("xdg")),
        ("XDG_DATA_HOME".into(), path("xdg")),
        ("XDG_CACHE_HOME".into(), path("xdg")),
        ("GH_PROMPT_DISABLED".into(), "1".into()),
        ("GH_NO_UPDATE_NOTIFIER".into(), "1".into()),
        ("GH_SPINNER_DISABLED".into(), "1".into()),
        ("GH_PAGER".into(), "cat".into()),
        ("NO_COLOR".into(), "1".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        // The system Git config may name the user's keychain helper.
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_SSL_CAINFO".into(), path("ca.crt")),
        ("GIT_CONFIG_COUNT".into(), "1".into()),
        ("GIT_CONFIG_KEY_0".into(), "http.proxy".into()),
        (
            "GIT_CONFIG_VALUE_0".into(),
            format!("http://{proxy_address}"),
        ),
    ];
    Cli {
        directory,
        environment,
        token: token.to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the gh, git, and openssl executables"]
async fn github_cli_manages_repositories_issues_and_pull_requests() {
    let h = super::rooted().await;
    let (_, token) = super::alice(&h).await;
    let cli = start(&h, &token).await;
    let home = cli.home();
    let work = cli.directory.path().join("work");

    // Authentication exactly as a user would set it up.
    cli.run_in(
        &home,
        "gh",
        &[
            "auth",
            "login",
            "--hostname",
            HOST,
            "--with-token",
            "--insecure-storage",
        ],
        Some(&format!("{token}\n")),
    )
    .await;
    let status = cli.gh(&home, &["auth", "status", "--hostname", HOST]).await;
    assert!(status.contains("alice") || status.is_empty());
    cli.gh(&home, &["auth", "setup-git", "--hostname", HOST])
        .await;
    let viewer: Value =
        serde_json::from_str(&cli.gh(&home, &["api", "--hostname", HOST, "user"]).await).unwrap();
    assert_eq!(viewer["login"], "alice");

    // Repository creation, clone, push, and browsing.
    let created = cli
        .gh(
            &home,
            &[
                "repo",
                "create",
                "demo",
                "--private",
                "--description",
                "gh demo",
            ],
        )
        .await;
    assert!(created.contains("alice/demo"), "{created}");
    cli.gh(&work, &["repo", "clone", &format!("{HOST}/alice/demo")])
        .await;
    let clone = work.join("demo");
    std::fs::write(clone.join("README.md"), "# Demo\n\nCreated with gh.\n").unwrap();
    cli.git(&clone, &["add", "README.md"]).await;
    cli.git(&clone, &["commit", "-m", "Initial commit"]).await;
    cli.git(&clone, &["push", "origin", "main"]).await;
    let view = cli.gh(&clone, &["repo", "view"]).await;
    assert!(view.contains("Created with gh."), "{view}");
    let repositories = cli
        .gh(
            &home,
            &["repo", "list", "alice", "--json", "nameWithOwner,isPrivate"],
        )
        .await;
    assert!(repositories.contains("alice/demo"), "{repositories}");

    // Issues and labels.
    cli.gh(
        &clone,
        &[
            "label",
            "create",
            "bug",
            "--color",
            "d73a4a",
            "--description",
            "Something is broken",
        ],
    )
    .await;
    assert!(cli.gh(&clone, &["label", "list"]).await.contains("bug"));
    let issue = cli
        .gh(
            &clone,
            &[
                "issue",
                "create",
                "--title",
                "Crash on start",
                "--body",
                "Repro steps",
                "--label",
                "bug",
            ],
        )
        .await;
    assert!(
        issue.trim_end().ends_with("/alice/demo/issues/1"),
        "{issue}"
    );
    assert!(
        cli.gh(&clone, &["issue", "list"])
            .await
            .contains("Crash on start")
    );
    cli.gh(&clone, &["issue", "comment", "1", "--body", "Seen on main"])
        .await;
    let issue = cli.gh(&clone, &["issue", "view", "1", "--comments"]).await;
    assert!(issue.contains("Seen on main"), "{issue}");
    cli.gh(&clone, &["issue", "close", "1", "--reason", "not planned"])
        .await;
    let state: Value = serde_json::from_str(
        &cli.gh(
            &clone,
            &["issue", "view", "1", "--json", "state,stateReason"],
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        (state["state"].as_str(), state["stateReason"].as_str()),
        (Some("CLOSED"), Some("NOT_PLANNED"))
    );
    cli.gh(&clone, &["issue", "reopen", "1"]).await;

    // Pull requests. Without a terminal gh opens a pull request only for a
    // branch that is already pushed, so push it first as a script would.
    cli.git(&clone, &["checkout", "-b", "docs"]).await;
    std::fs::write(clone.join("GUIDE.md"), "# Guide\n").unwrap();
    cli.git(&clone, &["add", "GUIDE.md"]).await;
    cli.git(&clone, &["commit", "-m", "Add a guide"]).await;
    cli.git(&clone, &["push", "-u", "origin", "docs"]).await;
    let pull = cli
        .gh(
            &clone,
            &[
                "pr",
                "create",
                "--title",
                "Add a guide",
                "--body",
                "Fixes #1",
                "--base",
                "main",
            ],
        )
        .await;
    assert!(pull.trim_end().ends_with("/alice/demo/pull/1"), "{pull}");
    assert!(
        cli.gh(&clone, &["pr", "list"])
            .await
            .contains("Add a guide")
    );
    let pull = cli.gh(&clone, &["pr", "view", "1"]).await;
    assert!(pull.contains("Add a guide"), "{pull}");
    let diff = cli.gh(&clone, &["pr", "diff", "1"]).await;
    assert!(diff.contains("+# Guide"), "{diff}");
    // Authors may comment on, but not approve, their own pull request.
    cli.gh(
        &clone,
        &[
            "pr",
            "review",
            "1",
            "--comment",
            "--body",
            "Reviewed with gh",
        ],
    )
    .await;
    let reviews = cli
        .gh(&clone, &["pr", "view", "1", "--json", "reviews"])
        .await;
    assert!(
        reviews.contains("Reviewed with gh") && reviews.contains("COMMENTED"),
        "{reviews}"
    );
    let fields: Value = serde_json::from_str(
        &cli.gh(
            &clone,
            &[
                "pr",
                "view",
                "1",
                "--json",
                "additions,deletions,headRefName,state",
            ],
        )
        .await,
    )
    .unwrap();
    assert_eq!(
        (
            fields["additions"].as_u64(),
            fields["headRefName"].as_str(),
            fields["state"].as_str()
        ),
        (Some(1), Some("docs"), Some("OPEN"))
    );
    cli.git(&clone, &["checkout", "main"]).await;
    cli.gh(&clone, &["pr", "checkout", "1"]).await;
    let branch = cli.git(&clone, &["branch", "--show-current"]).await;
    assert_eq!(branch.trim(), "docs");
    let status = cli.gh(&clone, &["pr", "status"]).await;
    assert!(status.contains("Add a guide"), "{status}");
    cli.gh(&clone, &["pr", "merge", "1", "--merge", "--delete-branch"])
        .await;
    let merged: Value = serde_json::from_str(
        &cli.gh(&clone, &["pr", "view", "1", "--json", "state"])
            .await,
    )
    .unwrap();
    assert_eq!(merged["state"], "MERGED");
    assert!(std::fs::read_to_string(clone.join("GUIDE.md")).is_ok());
    h.close().await;
}
