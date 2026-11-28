//! Stock `git upload-pack` behind protocol-v2 smart HTTP, with a Git LFS batch API.
//!
//! Import tests fetch from this host exactly as they would from a provider, so
//! the server's own client, staging, and verification run end to end.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path as Route, State},
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

/// A running host serving every repository below one directory.
pub(crate) struct GitHost {
    pub(crate) origin: String,
    lfs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    fetches: Arc<AtomicUsize>,
    token: Arc<Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for GitHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
struct Served {
    root: Arc<PathBuf>,
    origin: Arc<String>,
    lfs: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    fetches: Arc<AtomicUsize>,
    token: Arc<Mutex<Option<String>>>,
}

impl GitHost {
    /// Serve `root/<name>` as `<origin>/<name>.git`.
    pub(crate) async fn serve(root: &Path) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let lfs = Arc::new(Mutex::new(BTreeMap::new()));
        let fetches = Arc::new(AtomicUsize::new(0));
        let token = Arc::new(Mutex::new(None));
        let state = Served {
            root: Arc::new(root.to_owned()),
            origin: Arc::new(origin.clone()),
            lfs: Arc::clone(&lfs),
            fetches: Arc::clone(&fetches),
            token: Arc::clone(&token),
        };
        let app = Router::new()
            .route("/{repository}/info/refs", get(advertise))
            .route("/{repository}/git-upload-pack", post(upload_pack))
            .route("/{repository}/info/lfs/objects/batch", post(batch))
            .route("/lfs/{oid}", get(download))
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            origin,
            lfs,
            fetches,
            token,
            task,
        }
    }

    /// Require `Authorization: Bearer <token>` for Git and LFS batch requests.
    pub(crate) fn require_token(&self, token: &str) {
        *self.token.lock().unwrap() = Some(token.to_owned());
    }

    /// Clone URL for `root/<name>`.
    pub(crate) fn url(&self, name: &str) -> String {
        format!("{}/{name}.git", self.origin)
    }

    /// Offer one LFS payload through the batch API.
    pub(crate) fn offer_lfs(&self, payload: &[u8]) -> String {
        use sha2::Digest as _;
        let oid = format!("{:x}", sha2::Sha256::digest(payload));
        self.lfs
            .lock()
            .unwrap()
            .insert(oid.clone(), payload.to_vec());
        oid
    }

    /// Number of protocol-v2 `fetch` commands served.
    pub(crate) fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

fn authorized(state: &Served, headers: &HeaderMap) -> bool {
    let expected = state.token.lock().unwrap().clone();
    expected.is_none_or(|token| {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            == Some(format!("Bearer {token}").as_str())
    })
}

fn repository(state: &Served, name: &str) -> Option<PathBuf> {
    let name = name.strip_suffix(".git")?;
    (!name.contains(['/', '\\']) && !name.starts_with('.')).then(|| state.root.join(name))
}

async fn advertise(
    State(state): State<Served>,
    Route(name): Route<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(directory) = repository(&state, &name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // Protocol v2 replaces the v0 `# service=` preamble with its capability list.
    let body = run(&directory, &headers, true, Bytes::new()).await;
    (
        [(CONTENT_TYPE, "application/x-git-upload-pack-advertisement")],
        body,
    )
        .into_response()
}

async fn upload_pack(
    State(state): State<Served>,
    Route(name): Route<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(directory) = repository(&state, &name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if body.windows(13).any(|window| window == b"command=fetch") {
        state.fetches.fetch_add(1, Ordering::SeqCst);
    }
    let body = run(&directory, &headers, false, body).await;
    (
        [(CONTENT_TYPE, "application/x-git-upload-pack-result")],
        body,
    )
        .into_response()
}

async fn run(directory: &Path, headers: &HeaderMap, advertise: bool, input: Bytes) -> Vec<u8> {
    let protocol = headers
        .get("git-protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let mut command = tokio::process::Command::new("git");
    command.arg("upload-pack").arg("--stateless-rpc");
    if advertise {
        command.arg("--advertise-refs");
    }
    let mut child = command
        .arg(directory)
        .env("GIT_PROTOCOL", protocol)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let writer = tokio::spawn(async move {
        stdin.write_all(&input).await.unwrap();
    });
    let output = child.wait_with_output().await.unwrap();
    writer.await.unwrap();
    assert!(
        output.status.success(),
        "git upload-pack failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

async fn batch(
    State(state): State<Served>,
    Route(_): Route<String>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    if !authorized(&state, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let lfs = state.lfs.lock().unwrap();
    let objects = request["objects"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|object| {
            let oid = object["oid"].as_str().unwrap_or_default();
            match lfs.get(oid) {
                Some(payload) => json!({
                    "oid": oid,
                    "size": payload.len(),
                    "actions": {"download": {"href": format!("{}/lfs/{oid}", state.origin)}},
                }),
                None => json!({
                    "oid": oid,
                    "size": object["size"],
                    "error": {"code": 404, "message": "Object does not exist"},
                }),
            }
        })
        .collect::<Vec<_>>();
    (
        [(CONTENT_TYPE, "application/vnd.git-lfs+json")],
        Json(json!({"transfer": "basic", "objects": objects})),
    )
        .into_response()
}

async fn download(State(state): State<Served>, Route(oid): Route<String>) -> Response {
    match state.lfs.lock().unwrap().get(&oid) {
        Some(payload) => payload.clone().into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
