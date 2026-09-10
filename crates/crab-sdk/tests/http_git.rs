#![cfg(feature = "local")]

mod local_support;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

static HTTP_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

use crab_sdk::local::{
    CloneOptions, CommitOptions as LocalCommitOptions, FetchDepth, FetchOptions, PullOptions,
    PushOptions, PushOutcome as LocalPushOutcome, PushRefspec,
};
use crab_sdk::remote::write::CommitIdentity;
use crab_sdk::{ErrorKind, RepositoryLocator, WritePolicy};
use local_support::{client, commit, git_path, run};

struct GitHttpServer {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl GitHttpServer {
    fn start(project_root: PathBuf, git: PathBuf) -> Self {
        Self::start_with_lost_ack(project_root, git, false)
    }

    fn start_with_lost_ack(project_root: PathBuf, git: PathBuf, lose_receive_ack: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut offline = false;
            while !worker_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) if worker_stop.load(Ordering::Acquire) => {
                        drop(stream);
                        break;
                    }
                    Ok((stream, _)) if offline => drop(stream),
                    Ok((stream, _)) => {
                        offline = serve_request(stream, &project_root, &git, lose_receive_ack);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("HTTP Git listener failed: {error}"),
                }
            }
        });
        Self {
            address,
            stop,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("http://{}/remote.git", self.address)
    }
}

impl Drop for GitHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn serve_request(
    mut stream: TcpStream,
    project_root: &Path,
    git: &Path,
    lose_receive_ack: bool,
) -> bool {
    // macOS inherits O_NONBLOCK from the listener on accepted sockets. The
    // request parser is deliberately blocking, so normalize both platforms.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (method, target, headers, body) = read_request(&mut stream);
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let mut child = Command::new(git)
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", project_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_LENGTH", body.len().to_string())
        .env(
            "CONTENT_TYPE",
            headers.get("content-type").map_or("", String::as_str),
        )
        .env(
            "HTTP_GIT_PROTOCOL",
            headers.get("git-protocol").map_or("", String::as_str),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&body).unwrap();
    let output = child.wait_with_output().unwrap();
    if !output.status.success() {
        write_response(
            &mut stream,
            "500 Internal Server Error",
            &[],
            &output.stderr,
        );
        return false;
    }
    let (header_bytes, response_body) = split_headers(&output.stdout);
    let header_text = std::str::from_utf8(header_bytes).unwrap();
    let mut status = "200 OK".to_owned();
    let mut response_headers = Vec::new();
    for line in header_text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("Status: ") {
            status = value.to_owned();
        } else if !line.is_empty() {
            response_headers.push(line.to_owned());
        }
    }
    let lose_ack = lose_receive_ack && method == "POST" && path.ends_with("/git-receive-pack");
    if !lose_ack {
        write_response(&mut stream, &status, &response_headers, response_body);
    }
    lose_ack
}

fn read_request(
    stream: &mut TcpStream,
) -> (
    String,
    String,
    std::collections::BTreeMap<String, String>,
    Vec<u8>,
) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0u8; 16 * 1024];
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "HTTP request ended before its headers");
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(
            bytes.len() <= 1024 * 1024,
            "HTTP request headers exceeded 1 MiB"
        );
    };
    let head = std::str::from_utf8(&bytes[..header_end - 4]).unwrap();
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap().split_whitespace();
    let method = request.next().unwrap().to_owned();
    let target = request.next().unwrap().to_owned();
    let mut headers = std::collections::BTreeMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or_default();
    while bytes.len() < header_end + content_length {
        let mut buffer = [0u8; 64 * 1024];
        let read = stream.read(&mut buffer).unwrap();
        assert_ne!(read, 0, "HTTP request body ended early");
        bytes.extend_from_slice(&buffer[..read]);
    }
    let body = bytes[header_end..header_end + content_length].to_vec();
    (method, target, headers, body)
}

fn split_headers(response: &[u8]) -> (&[u8], &[u8]) {
    if let Some(index) = response.windows(4).position(|window| window == b"\r\n\r\n") {
        return (&response[..index], &response[index + 4..]);
    }
    let index = response
        .windows(2)
        .position(|window| window == b"\n\n")
        .expect("Git HTTP backend response has headers");
    (&response[..index], &response[index + 2..])
}

fn write_response(stream: &mut TcpStream, status: &str, headers: &[String], body: &[u8]) {
    write!(stream, "HTTP/1.1 {status}\r\n").unwrap();
    for header in headers {
        write!(stream, "{header}\r\n").unwrap();
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
}

fn seeded_remote(root: &Path, deny_non_fast_forwards: bool) -> (PathBuf, String) {
    let remote = root.join("remote.git");
    run(&git_path(), root, &["init", "--bare", "remote.git"]);
    for (key, value) in [
        ("http.receivepack", "true"),
        (
            "receive.denyNonFastForwards",
            if deny_non_fast_forwards {
                "true"
            } else {
                "false"
            },
        ),
    ] {
        run(
            &git_path(),
            root,
            &["--git-dir", remote.to_str().unwrap(), "config", key, value],
        );
    }
    let seed = root.join("seed");
    std::fs::create_dir(&seed).unwrap();
    run(&git_path(), &seed, &["init", "--initial-branch=main"]);
    let first = commit(&git_path(), &seed, "file.txt", "first");
    run(
        &git_path(),
        &seed,
        &["push", remote.to_str().unwrap(), "main:main"],
    );
    run(
        &git_path(),
        root,
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ],
    );
    (remote, first)
}

fn seeded_sha256_remote(root: &Path) {
    let remote = root.join("remote.git");
    run(
        &git_path(),
        root,
        &["init", "--bare", "--object-format=sha256", "remote.git"],
    );
    run(
        &git_path(),
        root,
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "config",
            "http.receivepack",
            "true",
        ],
    );
    let seed = root.join("sha256-seed");
    std::fs::create_dir(&seed).unwrap();
    run(
        &git_path(),
        &seed,
        &["init", "--initial-branch=main", "--object-format=sha256"],
    );
    commit(&git_path(), &seed, "file.txt", "sha256");
    run(
        &git_path(),
        &seed,
        &["push", remote.to_str().unwrap(), "main:main"],
    );
    run(
        &git_path(),
        root,
        &[
            "--git-dir",
            remote.to_str().unwrap(),
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ],
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn native_http_round_trip() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    let (remote, first) = seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("checkout");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    assert_eq!(run(&git_path(), &checkout, &["rev-parse", "HEAD"]), first);

    let second = commit(&git_path(), &checkout, "file.txt", "second");
    let outcome = local
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(
        matches!(outcome, LocalPushOutcome::TransportCommitted { .. }),
        "unexpected HTTP push outcome: {outcome:?}"
    );
    assert_eq!(
        run(
            &git_path(),
            fixture.path(),
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main"
            ],
        ),
        second
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_hydration_precedence_matches_committed_project_config() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    let (remote, _) = seeded_remote(fixture.path(), false);
    commit(
        &git_path(),
        &fixture.path().join("seed"),
        "crab.toml",
        "[remote]\nurl = \"crab://bucket/repository\"\n\n[hydrate]\ndefault = \"eager\"\n",
    );
    run(
        &git_path(),
        &fixture.path().join("seed"),
        &["push", remote.to_str().unwrap(), "main:main"],
    );
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());

    let configured = fixture.path().join("configured-hydration");
    sdk.clone_local(
        RepositoryLocator::http(&server.url()).unwrap(),
        &configured,
        CloneOptions::default(),
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(configured.join(".crab/local.toml"))
            .unwrap()
            .contains("lazy = false")
    );

    let overridden = fixture.path().join("explicit-hydration");
    sdk.clone_local(
        RepositoryLocator::http(&server.url()).unwrap(),
        &overridden,
        CloneOptions::default().lazy(),
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(overridden.join(".crab/local.toml"))
            .unwrap()
            .contains("lazy = true")
    );

    #[cfg(unix)]
    {
        std::fs::write(
            fixture.path().join("crab fixture.enable-hydrate"),
            b"enabled",
        )
        .unwrap();
        commit(
            &git_path(),
            &fixture.path().join("seed"),
            "crab.toml",
            "[remote]\nurl = \"crab://bucket/repository\"\n\n[hydrate]\ndefault = \"eager\"\nauto_patterns = [\"models/**\", \"-leading\"]\n",
        );
        run(
            &git_path(),
            &fixture.path().join("seed"),
            &["push", remote.to_str().unwrap(), "main:main"],
        );
        let patterned = fixture.path().join("pattern-hydration");
        sdk.clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &patterned,
            CloneOptions::default(),
        )
        .await
        .unwrap();
        assert!(
            std::fs::read_to_string(patterned.join(".crab/local.toml"))
                .unwrap()
                .contains("lazy = true")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("crab fixture.hydrate")).unwrap(),
            "--json\n--\nmodels/**\n-leading\n"
        );
    }
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn server_force_rejection_is_typed() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    let (_remote, first) = seeded_remote(fixture.path(), true);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("force-rejection");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    run(
        &git_path(),
        &checkout,
        &["checkout", "--orphan", "replacement"],
    );
    run(&git_path(), &checkout, &["rm", "-rf", "."]);
    let replacement = commit(&git_path(), &checkout, "replacement.txt", "replacement");
    assert_ne!(replacement, first);
    let error = local
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(vec![
                    PushRefspec::update("refs/heads/replacement", "refs/heads/main").unwrap(),
                ])
                .unwrap()
                .with_policy(WritePolicy::ForceWithLease),
        )
        .await
        .unwrap()
        .execute()
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn lost_acknowledgement_is_indeterminate() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    let (remote, _first) = seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start_with_lost_ack(fixture.path().to_owned(), git_path(), true);
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("lost-ack");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    let target = commit(
        &git_path(),
        &checkout,
        "file.txt",
        "after lost acknowledgement",
    );
    let outcome = local
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(outcome, LocalPushOutcome::Indeterminate { .. }));
    assert_eq!(
        run(
            &git_path(),
            fixture.path(),
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/main"
            ],
        ),
        target
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sha256_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    seeded_sha256_remote(fixture.path());
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("sha256-checkout");
    let error = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), ErrorKind::UnsupportedCapability);
    assert!(!checkout.exists());
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn local_edit_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("local-edit");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    std::fs::write(checkout.join("file.txt"), b"edited through SDK\n").unwrap();
    assert!(!local.status().await.unwrap().is_clean());
    local.stage(vec!["file.txt".into()]).await.unwrap();
    let identity =
        CommitIdentity::new("SDK test", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let commit = local
        .commit(
            LocalCommitOptions::new(identity.clone(), identity, b"SDK HTTP edit\n".to_vec())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        run(&git_path(), &checkout, &["rev-parse", "HEAD"]),
        commit.to_string()
    );
    assert!(local.status().await.unwrap().is_clean());
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    let (_remote, first) = seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("fetch");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    let second = commit(
        &git_path(),
        &fixture.path().join("seed"),
        "second.txt",
        "second",
    );
    run(
        &git_path(),
        &fixture.path().join("seed"),
        &[
            "push",
            fixture.path().join("remote.git").to_str().unwrap(),
            "main:main",
        ],
    );
    local.fetch(FetchOptions::default()).await.unwrap();
    assert_eq!(run(&git_path(), &checkout, &["rev-parse", "HEAD"]), first);
    assert_eq!(
        run(
            &git_path(),
            &checkout,
            &["rev-parse", "refs/remotes/origin/main"]
        ),
        second
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    seeded_remote(fixture.path(), false);
    let seed = fixture.path().join("seed");
    for index in 1..=2 {
        commit(
            &git_path(),
            &seed,
            &format!("{index}.txt"),
            &format!("commit {index}"),
        );
    }
    run(
        &git_path(),
        &seed,
        &[
            "push",
            fixture.path().join("remote.git").to_str().unwrap(),
            "main:main",
        ],
    );
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("shallow");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default().with_depth(1).unwrap(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    assert_eq!(
        run(&git_path(), &checkout, &["rev-list", "--count", "HEAD"]),
        "1"
    );
    let deepened = local
        .fetch(
            FetchOptions::default()
                .with_depth(FetchDepth::Deepen(1))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(deepened.is_shallow());
    local
        .fetch(
            FetchOptions::default()
                .with_depth(FetchDepth::Unshallow)
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        run(&git_path(), &checkout, &["rev-list", "--count", "HEAD"]),
        "3"
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("pull");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    let seed = fixture.path().join("seed");
    let second = commit(&git_path(), &seed, "second.txt", "second");
    run(
        &git_path(),
        &seed,
        &[
            "push",
            fixture.path().join("remote.git").to_str().unwrap(),
            "main:main",
        ],
    );
    local
        .pull(PullOptions::fast_forward_only().hydrate(false))
        .await
        .unwrap();
    assert_eq!(run(&git_path(), &checkout, &["rev-parse", "HEAD"]), second);
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn atomic_push_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("atomic");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    let target = commit(&git_path(), &checkout, "atomic.txt", "atomic");
    run(&git_path(), &checkout, &["tag", "release"]);
    local
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(vec![
                    PushRefspec::update("refs/heads/main", "refs/heads/main").unwrap(),
                    PushRefspec::update("refs/tags/release", "refs/tags/release").unwrap(),
                ])
                .unwrap(),
        )
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    for name in ["refs/heads/main", "refs/tags/release"] {
        assert_eq!(
            run(
                &git_path(),
                fixture.path(),
                &["--git-dir", "remote.git", "rev-parse", name],
            ),
            target
        );
    }
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    let (_remote, first) = seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("dry-run");
    let repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = repository.local().unwrap();
    commit(&git_path(), &checkout, "dry.txt", "dry");
    let outcome = local
        .prepare_push(PushOptions::current_branch().dry_run(true))
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(outcome, LocalPushOutcome::DryRun { .. }));
    assert_eq!(
        run(
            &git_path(),
            fixture.path(),
            &["--git-dir", "remote.git", "rev-parse", "refs/heads/main"],
        ),
        first
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_worktree_http_contract() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("primary");
    let primary_repository = sdk
        .clone_local(
            RepositoryLocator::http(&server.url()).unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let primary = primary_repository.local().unwrap();
    let linked = fixture.path().join("linked");
    run(
        &git_path(),
        &checkout,
        &["worktree", "add", "-b", "linked", linked.to_str().unwrap()],
    );
    let opened_repository = sdk
        .open(crab_sdk::OpenOptions::local(&linked))
        .await
        .unwrap();
    let opened = opened_repository.local().unwrap();
    std::fs::write(linked.join("linked.txt"), b"linked\n").unwrap();
    assert!(!opened.status().await.unwrap().is_clean());
    assert_eq!(opened.common_directory(), primary.common_directory());
    assert!(!checkout.join("linked.txt").exists());
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_http_extensions_remain_disabled() {
    let _serial = HTTP_TESTS.lock().await;
    let fixture = tempfile::tempdir().unwrap();
    seeded_remote(fixture.path(), false);
    let server = GitHttpServer::start(fixture.path().to_owned(), git_path());
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), fixture.path());
    let checkout = fixture.path().join("ordinary-clone");
    sdk.clone_local(
        RepositoryLocator::http(&server.url()).unwrap(),
        &checkout,
        CloneOptions::default(),
    )
    .await
    .unwrap();
    let config = Command::new(git_path())
        .current_dir(&checkout)
        .args([
            "config",
            "--get-regexp",
            "^(remote\\..*\\.promisor|core\\.sparseCheckout)$",
        ])
        .output()
        .unwrap();
    assert_eq!(config.status.code(), Some(1));
    assert!(config.stdout.is_empty());
    sdk.close().await.unwrap();
}
