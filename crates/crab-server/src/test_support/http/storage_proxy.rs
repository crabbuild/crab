use axum::{
    Json, Router,
    extract::{Path, State},
    http::{Request, Response, StatusCode},
    routing::{get, post},
};
use crab_storage::{StorageObservation, StorageObserver, StorageOperation, Store};
use http_body_util::BodyExt;
use hyper::{body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
enum Mode {
    Forward,
    Hold(String),
    Disconnected,
}

struct Control {
    mode: watch::Sender<Mode>,
    observed: AtomicBool,
    held: AtomicUsize,
    readers: Mutex<ReaderCounts>,
}

#[derive(Default, Clone, Copy, serde::Serialize)]
struct ReaderCounts {
    active: usize,
    peak: usize,
    started: usize,
    finished: usize,
}

impl StorageObserver for Control {
    fn started(&self, operation: StorageOperation) {
        if !is_read(operation) {
            return;
        }
        let mut readers = self.readers.lock().unwrap();
        readers.started += 1;
        readers.active += 1;
        readers.peak = readers.peak.max(readers.active);
    }

    fn finished(&self, observation: StorageObservation) {
        if is_read(observation.operation) {
            let mut readers = self.readers.lock().unwrap();
            readers.active -= 1;
            readers.finished += 1;
        }
    }
}

fn is_read(operation: StorageOperation) -> bool {
    matches!(
        operation,
        StorageOperation::Get
            | StorageOperation::Head
            | StorageOperation::Range
            | StorageOperation::List
    )
}

struct Held(Arc<Control>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.held.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(crate) struct StorageProxy {
    pub(crate) endpoint: String,
    pub(crate) control_origin: String,
    control: Arc<Control>,
    stop: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl StorageProxy {
    pub(crate) async fn start(endpoint: &str) -> Self {
        let upstream = url::Url::parse(endpoint).unwrap();
        assert_eq!(upstream.scheme(), "http");
        let host: std::net::IpAddr = upstream.host_str().unwrap().parse().unwrap();
        assert!(host.is_loopback(), "owned loopback storage is required");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let controls = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_origin = format!("http://{}", controls.local_addr().unwrap());
        let (mode, _) = watch::channel(Mode::Forward);
        let control = Arc::new(Control {
            mode,
            observed: AtomicBool::new(false),
            held: AtomicUsize::new(0),
            readers: Mutex::new(ReaderCounts::default()),
        });
        let stop = CancellationToken::new();
        let routes = Router::new()
            .route("/", get(state))
            .route("/hold/{hash}", post(hold))
            .route("/disconnect", post(disconnect))
            .route("/resume", post(resume))
            .with_state(control.clone());
        let stopping = stop.clone();
        let controller = tokio::spawn(async move {
            axum::serve(controls, routes)
                .with_graceful_shutdown(stopping.cancelled_owned())
                .await
                .unwrap();
        });
        let shared = control.clone();
        let stopping = stop.clone();
        let transport = tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    () = stopping.cancelled() => break,
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let control = shared.clone();
                        let mut mode = control.mode.subscribe();
                        let disconnected = matches!(*mode.borrow_and_update(), Mode::Disconnected);
                        if disconnected { continue; }
                        let client = client.clone();
                        let upstream = upstream.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |request| {
                                forward(request, control.clone(), client.clone(), upstream.clone())
                            });
                            tokio::select! {
                                _ = mode.changed() => {},
                                _ = http1::Builder::new().serve_connection(TokioIo::new(socket), service) => {},
                            }
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {},
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
        });
        Self {
            endpoint,
            control_origin,
            control,
            stop,
            tasks: vec![controller, transport],
        }
    }

    pub(crate) fn resume(&self) {
        self.control.mode.send_replace(Mode::Forward);
    }

    pub(crate) fn observed_store(&self, store: Store) -> Store {
        store.with_storage_observer(self.control.clone())
    }

    pub(crate) async fn close(mut self) {
        self.stop.cancel();
        for task in self.tasks.drain(..) {
            task.await.unwrap();
        }
    }
}

impl Drop for StorageProxy {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn forward(
    request: Request<Incoming>,
    control: Arc<Control>,
    client: reqwest::Client,
    upstream: url::Url,
) -> io::Result<Response<axum::body::Body>> {
    let mode = control.mode.borrow().clone();
    if matches!(mode, Mode::Disconnected) {
        return Err(io::Error::other("fixture transport disconnected"));
    }
    let uri = request.uri().path_and_query().unwrap().as_str();
    let target = upstream.join(uri).unwrap();
    let selected = matches!(mode, Mode::Hold(ref hash) if request.method() == "GET" && request.uri().path().ends_with(&format!("/workflow/results/objects/{hash}")));
    let (parts, body) = request.into_parts();
    // Host and signed headers remain intact. The upstream response is real;
    // cancelling its connection, rather than returning fake JSON, injects loss.
    let response = client
        .request(parts.method, target)
        .headers(parts.headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await
        .map_err(io::Error::other)?;
    if selected {
        assert_eq!(response.status(), StatusCode::OK);
        // A visible held response must already have its real GET/200 witness.
        control.observed.store(true, Ordering::SeqCst);
        control.held.fetch_add(1, Ordering::SeqCst);
        let _held = Held(control.clone());
        std::future::pending::<()>().await;
    }
    let mut outgoing = Response::builder().status(response.status());
    *outgoing.headers_mut().unwrap() = response.headers().clone();
    outgoing
        .body(axum::body::Body::from_stream(response.bytes_stream()))
        .map_err(io::Error::other)
}

async fn state(State(control): State<Arc<Control>>) -> Json<serde_json::Value> {
    // Count the actual server test process, including pooled connections. The
    // browser takes these samples after complete authenticated Results reads.
    let descriptors = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let fd_count = std::fs::read_dir(descriptors).unwrap().count();
    // A single snapshot prevents a concurrent completion from making started
    // and finished appear unbalanced. Observer callbacks never await this lock.
    let readers = *control.readers.lock().unwrap();
    Json(serde_json::json!({
        "held": control.held.load(Ordering::SeqCst),
        "observed": control.observed.load(Ordering::SeqCst).then(|| serde_json::json!({"method":"GET", "status":200})),
        "readers": readers,
        "fd_counts": {"server": fd_count},
    }))
}

async fn hold(State(control): State<Arc<Control>>, Path(hash): Path<String>) -> StatusCode {
    if hash.len() != 64
        || !hash.bytes().all(|c| c.is_ascii_hexdigit())
        || control.held.load(Ordering::SeqCst) != 0
    {
        return StatusCode::BAD_REQUEST;
    }
    control.observed.store(false, Ordering::SeqCst);
    control.mode.send_replace(Mode::Hold(hash));
    StatusCode::NO_CONTENT
}

async fn disconnect(State(control): State<Arc<Control>>) -> StatusCode {
    if control.held.load(Ordering::SeqCst) == 0 {
        return StatusCode::CONFLICT;
    }
    control.mode.send_replace(Mode::Disconnected);
    StatusCode::NO_CONTENT
}

async fn resume(State(control): State<Arc<Control>>) -> StatusCode {
    control.mode.send_replace(Mode::Forward);
    StatusCode::NO_CONTENT
}
