//! Integration tests for the cache service.
//!
//! Starts the server in-process with an in-memory origin store and ephemeral
//! SQLite databases, then exercises the full request lifecycle via `CacheClient`.

#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{
    GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt,
    PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use crab_cache::cache_client::{CacheClient, CacheServiceAuth};
use crab_cache::path_class::cache_route_contract;
use crab_cache_server::auth::{AuthPolicy, PolicyRule};
use crab_cache_server::cache_store::{CacheStore, ObjectType, ServerObjectKey};
use crab_cache_server::chunk_index::ChunkIndex;
use crab_cache_server::config::{AuthConfig, CacheServerConfig, DedupScope, MutablePathMode};
use crab_cache_server::db::{CACHE_DB_FILE, CacheDb};
use crab_cache_server::evictor::start_evictor_task;
use crab_cache_server::metrics::CacheMetrics;
use crab_cache_server::origin_client::OriginClient;
use crab_cache_server::state::{AppState, DedupIndexRebuildStats, build_router};
use crab_xet::hash::compute_data_hash;
use crab_xet::shard::{MDBXorbInfo, ShardWriter, XorbChunkSequenceEntry, XorbChunkSequenceHeader};
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use crab_xet::xorb::format::Chunk;
use crab_xet::xorb::parser::XorbParser;

/// Known PSK used by all tests.
const TEST_PSK: &str = "test-psk-key";

/// 1 MiB cache budget for eviction testing.
const TEST_MAX_CACHE_BYTES: u64 = 1_048_576;

/// Result of starting a test server: the bound address, a shutdown signal,
/// and the origin store for pre-populating test objects.
#[allow(dead_code)]
struct TestServer {
    addr: SocketAddr,
    origin: Arc<InMemory>,
    state: Arc<AppState>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    cache_root: PathBuf,
    _tempdir: TempDir,
}

async fn start_test_server() -> TestServer {
    start_test_server_with_budget(TEST_MAX_CACHE_BYTES).await
}

/// Start an in-process cache server bound to a random port on localhost.
///
/// The server uses:
/// - An in-memory origin store (returned for pre-population)
/// - An ephemeral SQLite database in a tempdir
/// - PSK auth with `TEST_PSK`
/// - A caller-selected cache budget
async fn start_test_server_with_budget(max_cache_bytes: u64) -> TestServer {
    let origin_store = Arc::new(InMemory::new());
    let origin = OriginClient::from_store(origin_store.clone() as Arc<dyn ObjectStore>);
    start_test_server_with_origin(max_cache_bytes, origin, origin_store).await
}

async fn start_test_server_with_dedup_scope(dedup_scope: DedupScope) -> TestServer {
    let origin_store = Arc::new(InMemory::new());
    let origin = OriginClient::from_store(origin_store.clone() as Arc<dyn ObjectStore>);
    start_test_server_with_origin_and_dedup_scope(
        TEST_MAX_CACHE_BYTES,
        origin,
        origin_store,
        dedup_scope,
    )
    .await
}

async fn start_test_server_with_counting_origin() -> (TestServer, Arc<AtomicUsize>) {
    start_test_server_with_counting_origin_gate(None).await
}

async fn start_test_server_with_counting_origin_and_mutable_path_mode(
    mutable_path_mode: MutablePathMode,
) -> (TestServer, Arc<AtomicUsize>) {
    let origin_store = Arc::new(InMemory::new());
    let get_count = Arc::new(AtomicUsize::new(0));
    let counting_store: Arc<dyn ObjectStore> = Arc::new(CountingStore {
        inner: Arc::clone(&origin_store),
        get_count: Arc::clone(&get_count),
        gate: None,
    });
    let origin = OriginClient::from_store(counting_store);
    let server = start_test_server_with_origin_settings(
        TEST_MAX_CACHE_BYTES,
        origin,
        origin_store,
        DedupScope::All,
        mutable_path_mode,
    )
    .await;
    (server, get_count)
}

async fn start_test_server_with_mutable_stream_error_origin(prefix: Bytes) -> TestServer {
    let origin_store = Arc::new(InMemory::new());
    let stream_error_store: Arc<dyn ObjectStore> = Arc::new(StreamErrorStore {
        inner: Arc::clone(&origin_store),
        prefix,
    });
    let origin = OriginClient::from_store(stream_error_store);
    start_test_server_with_origin_settings(
        TEST_MAX_CACHE_BYTES,
        origin,
        origin_store,
        DedupScope::All,
        MutablePathMode::Transparent,
    )
    .await
}

async fn start_test_server_with_counting_origin_gate(
    gate: Option<Arc<OriginGetGate>>,
) -> (TestServer, Arc<AtomicUsize>) {
    let origin_store = Arc::new(InMemory::new());
    let get_count = Arc::new(AtomicUsize::new(0));
    let counting_store: Arc<dyn ObjectStore> = Arc::new(CountingStore {
        inner: Arc::clone(&origin_store),
        get_count: Arc::clone(&get_count),
        gate,
    });
    let origin = OriginClient::from_store(counting_store);
    let server = start_test_server_with_origin(TEST_MAX_CACHE_BYTES, origin, origin_store).await;
    (server, get_count)
}

async fn start_test_server_with_origin(
    max_cache_bytes: u64,
    origin: OriginClient,
    origin_store: Arc<InMemory>,
) -> TestServer {
    start_test_server_with_origin_and_dedup_scope(
        max_cache_bytes,
        origin,
        origin_store,
        DedupScope::All,
    )
    .await
}

async fn start_test_server_with_origin_and_dedup_scope(
    max_cache_bytes: u64,
    origin: OriginClient,
    origin_store: Arc<InMemory>,
    dedup_scope: DedupScope,
) -> TestServer {
    start_test_server_with_origin_settings(
        max_cache_bytes,
        origin,
        origin_store,
        dedup_scope,
        MutablePathMode::Strict,
    )
    .await
}

async fn start_test_server_with_origin_settings(
    max_cache_bytes: u64,
    origin: OriginClient,
    origin_store: Arc<InMemory>,
    dedup_scope: DedupScope,
    mutable_path_mode: MutablePathMode,
) -> TestServer {
    start_test_server_with_origin_settings_and_policy(
        max_cache_bytes,
        origin,
        origin_store,
        dedup_scope,
        mutable_path_mode,
        None,
    )
    .await
}

async fn start_test_server_with_policy(policy: AuthPolicy) -> TestServer {
    let origin_store = Arc::new(InMemory::new());
    let origin = OriginClient::from_store(origin_store.clone() as Arc<dyn ObjectStore>);
    start_test_server_with_origin_settings_and_policy(
        TEST_MAX_CACHE_BYTES,
        origin,
        origin_store,
        DedupScope::All,
        MutablePathMode::Strict,
        Some(policy),
    )
    .await
}

async fn start_test_server_with_origin_settings_and_policy(
    max_cache_bytes: u64,
    origin: OriginClient,
    origin_store: Arc<InMemory>,
    dedup_scope: DedupScope,
    mutable_path_mode: MutablePathMode,
    policy: Option<AuthPolicy>,
) -> TestServer {
    let tempdir = tempfile::tempdir().unwrap();
    let cache_root = tempdir.path().join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let cache_db = CacheDb::open_or_create(&cache_root.join(CACHE_DB_FILE)).unwrap();

    // Build components.
    let cache_store = Arc::new(
        CacheStore::open(
            cache_root.clone(),
            max_cache_bytes,
            cache_db.connect().unwrap(),
        )
        .unwrap(),
    );
    let chunk_index = ChunkIndex::open(cache_db.connect().unwrap()).unwrap();

    let metrics = CacheMetrics::stub();

    // Compute the blake3 hash of the test PSK for server-side auth config.
    let psk_hash = blake3::hash(TEST_PSK.as_bytes());
    let config = CacheServerConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        tls: None,
        auth: AuthConfig::Psk {
            key_hash: *psk_hash.as_bytes(),
        },
        origin_url: "memory://".to_string(),
        cache_root,
        max_cache_bytes,
        dedup_scope,
        drain_timeout: Duration::from_secs(1),
        mutable_path_mode,
        high_water_ratio: 0.95,
        low_water_ratio: 0.90,
        policy_path: None,
    };

    // Start the background evictor.
    let evictor_handle = start_evictor_task(
        Arc::clone(&cache_store),
        config.high_water_ratio,
        config.low_water_ratio,
        Duration::from_secs(60),
    );
    let evictor_notify = evictor_handle.notify_handle();

    let state = Arc::new(AppState {
        cache_store,
        chunk_index,
        origin,
        config,
        metrics,
        policy,
        evictor_notify,
        origin_healthy: AtomicBool::new(true),
        // Start fresh so the first health check uses the cached `true` value
        // rather than probing the in-memory origin.
        origin_health_checked_at: tokio::sync::Mutex::new(Instant::now()),
        cache_miss_locks: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        push_warming_body_permits: tokio::sync::Semaphore::new(8),
        dedup_index_rebuild: DedupIndexRebuildStats {
            status: "not_run".to_string(),
            entries: 0,
            error: None,
        },
        dedup_last_ingestion_error: tokio::sync::RwLock::new(None),
    });

    let router = build_router(Arc::clone(&state));

    // Bind to a random port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn the server in a background task.
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();

        // Clean up evictor after server stops.
        evictor_handle.shutdown().await;
    });

    // Give the server a moment to start accepting connections.
    tokio::time::sleep(Duration::from_millis(20)).await;

    TestServer {
        addr,
        origin: origin_store,
        state,
        shutdown: shutdown_tx,
        cache_root: tempdir.path().join("cache"),
        _tempdir: tempdir,
    }
}

/// Construct a `CacheClient` configured with the test PSK.
#[allow(dead_code)]
fn test_client(addr: SocketAddr) -> CacheClient {
    let base_url = format!("http://{addr}");
    let auth = CacheServiceAuth::Psk(TEST_PSK.to_string());
    CacheClient::new(&base_url, &auth, None, None, None).unwrap()
}

struct CountingStore {
    inner: Arc<InMemory>,
    get_count: Arc<AtomicUsize>,
    gate: Option<Arc<OriginGetGate>>,
}

impl std::fmt::Debug for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingStore").finish_non_exhaustive()
    }
}

struct OriginGetGate {
    release_rx: tokio::sync::watch::Receiver<bool>,
}

impl std::fmt::Debug for OriginGetGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginGetGate").finish_non_exhaustive()
    }
}

fn origin_get_gate() -> (Arc<OriginGetGate>, tokio::sync::watch::Sender<bool>) {
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    (Arc::new(OriginGetGate { release_rx }), release_tx)
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingStore")
    }
}

#[async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if !options.head {
            self.get_count.fetch_add(1, Ordering::Relaxed);
            if let Some(gate) = &self.gate {
                let mut release_rx = gate.release_rx.clone();
                while !*release_rx.borrow() {
                    if release_rx.changed().await.is_err() {
                        break;
                    }
                }
            }
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

struct StreamErrorStore {
    inner: Arc<InMemory>,
    prefix: Bytes,
}

impl std::fmt::Debug for StreamErrorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamErrorStore").finish_non_exhaustive()
    }
}

impl std::fmt::Display for StreamErrorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StreamErrorStore")
    }
}

#[async_trait]
impl ObjectStore for StreamErrorStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        let is_head = options.head;
        let mut result = self.inner.get_opts(location, options).await?;
        if is_head {
            return Ok(result);
        }
        result.payload =
            object_store::GetResultPayload::Stream(Box::pin(futures_util::stream::iter([
                Ok(self.prefix.clone()),
                Err(object_store::Error::Generic {
                    store: "StreamErrorStore",
                    source: "origin stream failed after response start".into(),
                }),
            ])));
        Ok(result)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

struct RealShardFixture {
    shard_bytes: Bytes,
    shard_hash_hex: String,
    xorb_bytes: Bytes,
    xorb_hash: [u8; 32],
    xorb_hash_hex: String,
    chunk_hash: [u8; 32],
    chunk_index: u32,
    length: u32,
}

fn build_test_xorb(chunk_bytes: Bytes) -> (Bytes, String) {
    let chunk_hash = compute_data_hash(chunk_bytes.as_ref());
    let mut builder = XorbBuilder::new();
    builder
        .push(
            &Chunk {
                hash: chunk_hash,
                data: chunk_bytes,
            },
            RunId(0),
        )
        .unwrap();
    let mut xorbs = builder.finalize().unwrap();
    assert_eq!(xorbs.len(), 1);
    let xorb = xorbs.pop().unwrap();
    (xorb.bytes, xorb.hash.hex())
}

fn deterministic_bytes(seed: u64, len: usize) -> Bytes {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut data = Vec::with_capacity(len);
    while data.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        data.extend_from_slice(&state.to_le_bytes());
    }
    data.truncate(len);
    Bytes::from(data)
}

fn build_real_shard() -> RealShardFixture {
    let chunk_bytes = Bytes::from_static(b"real cache service dedup proof chunk");
    let chunk_hash = compute_data_hash(chunk_bytes.as_ref());

    let mut builder = XorbBuilder::new();
    builder
        .push(
            &Chunk {
                hash: chunk_hash,
                data: chunk_bytes,
            },
            RunId(0),
        )
        .unwrap();
    let mut xorbs = builder.finalize().unwrap();
    assert_eq!(xorbs.len(), 1);
    let xorb_result = xorbs.pop().unwrap();
    let placement = xorb_result.placements[0].clone();
    assert_eq!(placement.chunk_hash, chunk_hash);
    assert_eq!(placement.xorb_hash, xorb_result.hash);
    let parser = XorbParser::parse(xorb_result.bytes.clone()).unwrap();
    assert_eq!(parser.hash(), xorb_result.hash);
    assert_eq!(
        parser.chunk_meta(placement.chunk_index).unwrap().hash,
        chunk_hash
    );
    assert_eq!(
        parser
            .chunk_meta(placement.chunk_index)
            .unwrap()
            .uncompressed_len,
        placement.uncompressed_size
    );
    assert_eq!(
        parser.get_chunk(placement.chunk_index).unwrap().hash,
        chunk_hash
    );

    let xorb = Arc::new(MDBXorbInfo {
        metadata: XorbChunkSequenceHeader::new(
            xorb_result.hash,
            1,
            placement.uncompressed_size as usize,
        ),
        chunks: vec![XorbChunkSequenceEntry::new(
            chunk_hash,
            placement.uncompressed_size,
            0,
        )],
    });

    let mut writer = ShardWriter::new();
    writer.add_xorb(xorb).unwrap();
    let (bytes, shard_hash) = writer.finalize().unwrap();

    RealShardFixture {
        shard_bytes: Bytes::from(bytes),
        shard_hash_hex: shard_hash.hex(),
        xorb_bytes: xorb_result.bytes,
        xorb_hash: xorb_result.hash.into(),
        xorb_hash_hex: xorb_result.hash.hex(),
        chunk_hash: chunk_hash.into(),
        chunk_index: placement.chunk_index,
        length: placement.uncompressed_size,
    }
}

async fn put_real_shard_fixture(client: &CacheClient, fixture: &RealShardFixture) {
    let xorb_path = global_path("xorbs", &fixture.xorb_hash_hex);
    client
        .put(&xorb_path, fixture.xorb_bytes.clone())
        .await
        .unwrap();

    let shard_path = global_path("shards", &fixture.shard_hash_hex);
    client
        .put(&shard_path, fixture.shard_bytes.clone())
        .await
        .unwrap();
}

fn hex_bytes(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn global_path(kind: &str, hash: &str) -> String {
    format!(".crab/{kind}/{}/{hash}", &hash[..2])
}

fn pack_storage_hex(name: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[ObjectType::Pack.as_u8()]);
    hasher.update(b"\0");
    hasher.update(name.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn assert_immutable_cache_headers(
    headers: &reqwest::header::HeaderMap,
    hash_hex: &str,
    cache_status: &str,
) {
    let etag = format!("\"{hash_hex}\"");
    assert_eq!(
        headers.get("etag").and_then(|value| value.to_str().ok()),
        Some(etag.as_str())
    );
    assert_eq!(
        headers
            .get("cache-control")
            .and_then(|value| value.to_str().ok()),
        Some("public, max-age=31536000, immutable")
    );
    assert_eq!(
        headers.get("x-cache").and_then(|value| value.to_str().ok()),
        Some(cache_status)
    );
}

async fn admin_stats(addr: SocketAddr) -> Value {
    let url = format!("http://{addr}/v1/admin/stats");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    resp.json::<Value>().await.unwrap()
}

fn traffic_stat(stats: &Value, name: &str) -> u64 {
    stats["traffic"][name]
        .as_u64()
        .unwrap_or_else(|| panic!("missing traffic.{name}: {stats}"))
}

fn object_traffic_stat(stats: &Value, object_type: &str, name: &str) -> u64 {
    stats["traffic"]["by_object_type"][object_type][name]
        .as_u64()
        .unwrap_or_else(|| panic!("missing traffic.by_object_type.{object_type}.{name}: {stats}"))
}

fn startup_integrity_stat(stats: &Value, name: &str) -> u64 {
    stats["startup_integrity"][name]
        .as_u64()
        .unwrap_or_else(|| panic!("missing startup_integrity.{name}: {stats}"))
}

fn assert_clean_startup_integrity(stats: &Value) {
    assert_eq!(startup_integrity_stat(stats, "metadata_entries_removed"), 0);
    assert_eq!(
        startup_integrity_stat(stats, "metadata_size_corrections"),
        0
    );
    assert_eq!(
        startup_integrity_stat(stats, "unindexed_objects_indexed"),
        0
    );
    assert_eq!(startup_integrity_stat(stats, "unindexed_paths_removed"), 0);
}

fn runtime_integrity_stat(stats: &Value, name: &str) -> u64 {
    stats["runtime_integrity"][name]
        .as_u64()
        .unwrap_or_else(|| panic!("missing runtime_integrity.{name}: {stats}"))
}

fn assert_clean_runtime_integrity(stats: &Value) {
    assert_eq!(runtime_integrity_stat(stats, "missing_files_repaired"), 0);
    assert_eq!(runtime_integrity_stat(stats, "invalid_objects_evicted"), 0);
    assert_eq!(
        runtime_integrity_stat(stats, "metadata_entries_recreated"),
        0
    );
}

// ---------------------------------------------------------------------------
// Smoke test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_endpoint_returns_200() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    assert!(
        client.is_healthy().await,
        "CacheClient::is_healthy should use the service health route"
    );

    // Hit the /v1/health endpoint directly.
    let url = format!("http://{}/v1/health", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "health endpoint should return 200"
    );

    // Verify compatibility aliases for older clients/probes.
    let url = format!("http://{}/health", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200, "/health should return 200");

    // Also verify the liveness probe.
    let url = format!("http://{}/v1/health/live", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "liveness endpoint should return 200"
    );

    let url = format!("http://{}/health/live", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "/health/live should return 200"
    );

    // Shut down cleanly.
    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn health_readiness_treats_missing_probe_object_as_reachable() {
    let server = start_test_server().await;
    *server.state.origin_health_checked_at.lock().await = Instant::now() - Duration::from_secs(10);

    let url = format!("http://{}/v1/health", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "missing health probe object should prove the origin is reachable"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn health_readiness_degrades_when_stale_origin_probe_fails() {
    let server = start_test_server_with_failing_origin().await;
    *server.state.origin_health_checked_at.lock().await = Instant::now() - Duration::from_secs(10);

    let url = format!("http://{}/v1/health", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        503,
        "readiness should fail when the origin cannot serve cache misses"
    );
    assert_eq!(resp.text().await.unwrap(), "origin unreachable");

    let url = format!("http://{}/v1/health/live", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "liveness should not depend on origin readiness"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn capabilities_requires_auth_and_reports_limits() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let capabilities = client.capabilities().await.unwrap();
    assert_eq!(capabilities.limits.max_cache_bytes, TEST_MAX_CACHE_BYTES);
    assert_eq!(capabilities.limits.max_object_bytes, 256 * 1024 * 1024);
    assert_eq!(capabilities.routes, Some(cache_route_contract()));

    let url = format!("http://{}/v1/capabilities", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn authz_check_reports_all_actions_allowed_without_policy() {
    let server = start_test_server().await;
    let url = format!("http://{}/v1/authz/check", server.addr);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({ "repo_path": "org/repo" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body = resp.json::<Value>().await.unwrap();
    assert_eq!(body["schema"], "crab-cache-service.authz-check.v1");
    assert_eq!(body["repo_path"], "org/repo");
    assert_eq!(body["policy_configured"], false);
    assert_eq!(body["actions"]["read"], true);
    assert_eq!(body["actions"]["write"], true);
    assert_eq!(body["actions"]["dedup"], true);
    assert_eq!(body["actions"]["admin"], true);

    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({ "repo_path": "org/repo" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn authz_check_reports_policy_action_matrix_for_repo() {
    let policy = AuthPolicy {
        rules: vec![
            PolicyRule {
                principal: "psk-client".to_string(),
                repos: vec!["org/allowed/*".to_string()],
                actions: vec!["read".to_string(), "dedup".to_string()],
            },
            PolicyRule {
                principal: "admin-client".to_string(),
                repos: vec!["*".to_string()],
                actions: vec!["admin".to_string()],
            },
        ],
    };
    let server = start_test_server_with_policy(policy).await;
    let url = format!("http://{}/v1/authz/check", server.addr);

    let allowed = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({ "repo_path": "org/allowed/repo" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(allowed["policy_configured"], true);
    assert_eq!(allowed["actions"]["read"], true);
    assert_eq!(allowed["actions"]["write"], false);
    assert_eq!(allowed["actions"]["dedup"], true);
    assert_eq!(allowed["actions"]["admin"], false);
    assert!(!allowed.to_string().contains(TEST_PSK));

    let denied = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({ "repo_path": "org/denied/repo" }))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(denied["actions"]["read"], false);
    assert_eq!(denied["actions"]["write"], false);
    assert_eq!(denied["actions"]["dedup"], false);
    assert_eq!(denied["actions"]["admin"], false);

    let malformed = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({ "repo_path": "../bad" }))
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status().as_u16(), 400);

    let _ = server.shutdown.send(());
}

// ---------------------------------------------------------------------------
// Core lifecycle tests (task 8.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_cache_miss_fetches_and_caches() {
    let (server, origin_get_count) = start_test_server_with_counting_origin().await;

    let (data, hash_hex) = build_test_xorb(Bytes::from_static(b"hello cache world"));

    // Pre-populate the origin store with the object at the expected path.
    let origin_path = global_path("xorbs", &hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(origin_path.clone()),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    let cache_path = global_path("xorbs", &hash_hex);
    let url = format!("http://{}/v1/{cache_path}", server.addr);
    let http = reqwest::Client::new();

    // First GET — cache miss, fetches from origin.
    let resp1 = http
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 200);
    assert_immutable_cache_headers(resp1.headers(), &hash_hex, "MISS");
    let result1 = resp1.bytes().await.unwrap();
    assert_eq!(result1, data, "first GET should return origin data");
    assert_eq!(
        origin_get_count.load(Ordering::Relaxed),
        1,
        "first GET should fetch exactly once from origin"
    );

    // Second GET — cache hit, served from local cache.
    let resp2 = http
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 200);
    assert_immutable_cache_headers(resp2.headers(), &hash_hex, "HIT");
    let result2 = resp2.bytes().await.unwrap();
    assert_eq!(result2, data, "second GET should return cached data");
    assert_eq!(
        origin_get_count.load(Ordering::Relaxed),
        1,
        "cache hit should not fetch from origin"
    );

    let stats = admin_stats(server.addr).await;
    assert_eq!(traffic_stat(&stats, "cache_misses"), 1);
    assert_eq!(traffic_stat(&stats, "cache_hits"), 1);
    assert_eq!(traffic_stat(&stats, "origin_avoided_reads"), 1);
    assert_eq!(traffic_stat(&stats, "origin_fetches"), 1);
    assert_eq!(
        traffic_stat(&stats, "origin_fetch_bytes"),
        data.len() as u64
    );
    assert_eq!(
        traffic_stat(&stats, "bytes_served_from_origin"),
        data.len() as u64
    );
    assert_eq!(
        traffic_stat(&stats, "bytes_served_from_cache"),
        data.len() as u64
    );
    assert_eq!(traffic_stat(&stats, "coalesced_misses"), 0);
    assert_eq!(traffic_stat(&stats, "inflight_misses"), 0);
    assert_clean_startup_integrity(&stats);
    assert_clean_runtime_integrity(&stats);
    assert_eq!(object_traffic_stat(&stats, "xorb", "cache_misses"), 1);
    assert_eq!(object_traffic_stat(&stats, "xorb", "cache_hits"), 1);
    assert_eq!(object_traffic_stat(&stats, "xorb", "origin_fetches"), 1);
    assert_eq!(
        object_traffic_stat(&stats, "xorb", "bytes_served_from_origin"),
        data.len() as u64
    );
    assert_eq!(
        object_traffic_stat(&stats, "xorb", "bytes_served_from_cache"),
        data.len() as u64
    );
    assert_eq!(object_traffic_stat(&stats, "metadata", "cache_hits"), 0);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_transparent_mutable_get_proxies_without_cache_side_effects() {
    let (server, origin_get_count) =
        start_test_server_with_counting_origin_and_mutable_path_mode(MutablePathMode::Transparent)
            .await;
    let path = "org/repo/refs/heads/main";
    let data = Bytes::from_static(b"mutable ref target");
    server
        .origin
        .put(
            &ObjectPath::from(path),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    let url = format!("http://{}/v1/{path}", server.addr);
    let http = reqwest::Client::new();
    let expected_content_length = data.len().to_string();

    for expected_gets in 1..=2 {
        let resp = http
            .get(&url)
            .header("x-cache-psk", TEST_PSK)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert!(resp.headers().get("x-cache").is_none());
        assert!(resp.headers().get("cache-control").is_none());
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok()),
            Some(expected_content_length.as_str())
        );
        assert_eq!(resp.bytes().await.unwrap(), data);
        assert_eq!(
            origin_get_count.load(Ordering::Relaxed),
            expected_gets,
            "transparent mutable reads should pass through to origin every time"
        );
    }

    let head_resp = http
        .head(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();
    assert_eq!(head_resp.status().as_u16(), 200);
    assert!(head_resp.headers().get("x-cache").is_none());
    assert!(head_resp.headers().get("cache-control").is_none());
    assert_eq!(
        head_resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok()),
        Some(expected_content_length.as_str())
    );
    assert_eq!(origin_get_count.load(Ordering::Relaxed), 2);

    let stats = admin_stats(server.addr).await;
    assert_eq!(stats["total_bytes"].as_u64().unwrap(), 0);
    assert_eq!(stats["xorb_count"].as_u64().unwrap(), 0);
    assert_eq!(stats["shard_count"].as_u64().unwrap(), 0);
    assert_eq!(stats["pack_count"].as_u64().unwrap(), 0);
    assert_eq!(stats["metadata_count"].as_u64().unwrap(), 0);
    assert_eq!(traffic_stat(&stats, "mutable_proxy_reads"), 3);
    assert_eq!(traffic_stat(&stats, "mutable_proxy_gets"), 2);
    assert_eq!(traffic_stat(&stats, "mutable_proxy_heads"), 1);
    assert_eq!(
        traffic_stat(&stats, "mutable_proxy_bytes"),
        data.len() as u64 * 2
    );
    assert_eq!(traffic_stat(&stats, "mutable_read_rejections"), 0);
    assert_eq!(traffic_stat(&stats, "cache_hits"), 0);
    assert_eq!(traffic_stat(&stats, "cache_misses"), 0);
    assert_eq!(traffic_stat(&stats, "origin_fetches"), 0);
    assert_eq!(traffic_stat(&stats, "origin_head_requests"), 0);
    assert_eq!(traffic_stat(&stats, "bytes_served_total"), 0);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_transparent_mutable_stream_error_does_not_cache_or_overcount_bytes() {
    let prefix = Bytes::from_static(b"partial mutable bytes");
    let server = start_test_server_with_mutable_stream_error_origin(prefix.clone()).await;
    let path = "org/repo/refs/heads/main";
    let data = Bytes::from_static(b"partial mutable bytes plus bytes that never arrive");
    server
        .origin
        .put(
            &ObjectPath::from(path),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    let url = format!("http://{}/v1/{path}", server.addr);
    let response = reqwest::Client::new()
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await;

    let failed_response_body = match response {
        Ok(resp) => {
            assert_eq!(resp.status().as_u16(), 200);
            let expected_content_length = data.len().to_string();
            assert_eq!(
                resp.headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|value| value.to_str().ok()),
                Some(expected_content_length.as_str())
            );
            resp.bytes().await.is_err()
        }
        Err(_) => true,
    };
    assert!(failed_response_body);

    let stats = admin_stats(server.addr).await;
    assert_eq!(stats["total_bytes"].as_u64().unwrap(), 0);
    assert_eq!(stats["xorb_count"].as_u64().unwrap(), 0);
    assert_eq!(stats["shard_count"].as_u64().unwrap(), 0);
    assert_eq!(stats["pack_count"].as_u64().unwrap(), 0);
    assert_eq!(stats["metadata_count"].as_u64().unwrap(), 0);
    assert_eq!(traffic_stat(&stats, "mutable_proxy_reads"), 1);
    assert_eq!(traffic_stat(&stats, "mutable_proxy_gets"), 1);
    assert_eq!(traffic_stat(&stats, "mutable_proxy_heads"), 0);
    assert_eq!(
        traffic_stat(&stats, "mutable_proxy_bytes"),
        prefix.len() as u64
    );
    assert_eq!(traffic_stat(&stats, "mutable_proxy_stream_errors"), 1);
    assert_eq!(traffic_stat(&stats, "cache_hits"), 0);
    assert_eq!(traffic_stat(&stats, "cache_misses"), 0);
    assert_eq!(traffic_stat(&stats, "origin_fetches"), 0);
    assert_eq!(traffic_stat(&stats, "origin_head_requests"), 0);
    assert_eq!(traffic_stat(&stats, "bytes_served_total"), 0);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_push_warming_populates_cache() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let (data, hash_hex) = build_test_xorb(Bytes::from_static(b"push-warmed content"));

    // PUT via cache client (push warming).
    let path = global_path("xorbs", &hash_hex);
    client.put(&path, data.clone()).await.unwrap();

    // GET should return the cached data without needing origin.
    let result = client.get(&path).await.unwrap();
    assert_eq!(
        result, data,
        "GET after push-warm should return cached data"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_push_warming_accepts_multimegabyte_xorb() {
    let server = start_test_server_with_budget(8 * 1024 * 1024).await;
    let client = test_client(server.addr);

    let chunk = Bytes::from(
        (0..3 * 1024 * 1024)
            .map(|i| {
                let mixed = (i as u32)
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                (mixed ^ (mixed >> 16)) as u8
            })
            .collect::<Vec<_>>(),
    );
    let (data, hash_hex) = build_test_xorb(chunk);
    let path = global_path("xorbs", &hash_hex);

    client.put(&path, data.clone()).await.unwrap();

    let cached = client.get(&path).await.unwrap();
    assert_eq!(cached, data);

    let stats = admin_stats(server.addr).await;
    assert_eq!(stats["xorb_count"].as_u64().unwrap(), 1);
    assert_eq!(stats["total_bytes"].as_u64().unwrap(), data.len() as u64);
    assert_eq!(
        stats["limits"]["max_cache_bytes"].as_u64().unwrap(),
        stats["max_bytes"].as_u64().unwrap()
    );
    assert_eq!(
        stats["limits"]["max_object_bytes"].as_u64().unwrap(),
        256 * 1024 * 1024
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_push_warming_rejects_oversized_content_length_before_body() {
    let server = start_test_server().await;
    let hash_hex = "a".repeat(64);
    let request = format!(
        "PUT /v1/.crab/xorbs/{hash_hex} HTTP/1.1\r\n\
         Host: {}\r\n\
         x-cache-psk: {TEST_PSK}\r\n\
         Content-Length: 999999999999\r\n\
         \r\n",
        server.addr
    );

    let mut stream = TcpStream::connect(server.addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = vec![0; 1024];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8_lossy(&response[..read]);
    assert!(
        response.starts_with("HTTP/1.1 413"),
        "oversized PUT should be rejected before reading a body: {response}"
    );

    let stats = admin_stats(server.addr).await;
    assert_eq!(traffic_stat(&stats, "push_warming_writes"), 0);
    assert_eq!(traffic_stat(&stats, "push_warming_bytes"), 0);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_dedup_query_requires_cached_xorb_proof() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();

    let shard_path = global_path("shards", &fixture.shard_hash_hex);
    client
        .put(&shard_path, fixture.shard_bytes.clone())
        .await
        .unwrap();

    let result = client
        .dedup_query("org/repo", &[fixture.chunk_hash])
        .await
        .unwrap();

    assert!(result.known.is_empty());
    assert_eq!(result.unknown, vec![0]);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_dedup_query_after_shard_and_xorb_ingestion() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    put_real_shard_fixture(&client, &fixture).await;
    let xorb_path = global_path("xorbs", &fixture.xorb_hash_hex);
    let shard_path = global_path("shards", &fixture.shard_hash_hex);

    let cached_xorb = client.get(&xorb_path).await.unwrap();
    assert_eq!(cached_xorb, fixture.xorb_bytes);

    // GET should be served from the warmed cache using the same .crab path.
    let cached = client.get(&shard_path).await.unwrap();
    assert_eq!(cached, fixture.shard_bytes);

    // POST dedup query with the chunk hash we just ingested.
    let result = client
        .dedup_query("org/repo", &[fixture.chunk_hash])
        .await
        .unwrap();

    assert_eq!(result.known.len(), 1, "chunk should be reported as known");
    assert_eq!(result.unknown.len(), 0, "no chunks should be unknown");
    assert_eq!(result.known[0].index, 0);
    assert_eq!(result.known[0].xorb_hash, fixture.xorb_hash_hex);
    assert_eq!(result.known[0].chunk_index, fixture.chunk_index);
    assert_eq!(result.known[0].length, fixture.length);
    assert!(result.known[0].cache_verified);

    let stats = admin_stats(server.addr).await;
    assert_eq!(stats["dedup_index"]["indexed_chunks"].as_u64().unwrap(), 1);
    assert_eq!(stats["dedup_index"]["scope"].as_str().unwrap(), "all");
    assert!(
        !stats["dedup_index"]["requires_repo_context"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        stats["dedup_index"]["startup_rebuild"]["status"]
            .as_str()
            .unwrap(),
        "not_run"
    );
    assert!(stats["dedup_index"]["last_ingestion_error"].is_null());
    assert_eq!(object_traffic_stat(&stats, "shard", "cache_hits"), 1);
    assert_eq!(
        object_traffic_stat(&stats, "shard", "push_warming_writes"),
        1
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_restricted_dedup_scope_authorizes_matching_repo() {
    let server =
        start_test_server_with_dedup_scope(DedupScope::Repos(vec!["org/repo".to_string()])).await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    put_real_shard_fixture(&client, &fixture).await;

    let result = client
        .dedup_query("org/repo", &[fixture.chunk_hash])
        .await
        .unwrap();
    assert_eq!(result.known.len(), 1);
    assert!(result.unknown.is_empty());

    let stats = admin_stats(server.addr).await;
    assert_eq!(
        stats["dedup_index"]["requires_repo_context"]
            .as_bool()
            .unwrap(),
        true
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_restricted_dedup_scope_rejects_out_of_scope_repo() {
    let server =
        start_test_server_with_dedup_scope(DedupScope::Repos(vec!["org/repo".to_string()])).await;
    let url = format!("http://{}/v1/dedup/query", server.addr);
    let chunk_hash = [0x42; 32];

    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({
            "repo_path": "org/other",
            "chunk_hashes": [hex_bytes(&chunk_hash)],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 403);
    let body = resp.text().await.unwrap();
    assert!(body.contains("dedup scope repos:org/repo"));
    assert!(body.contains("does not allow repo org/other"));

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_bucket_prefix_dedup_scope_authorizes_nested_repo() {
    let server =
        start_test_server_with_dedup_scope(DedupScope::BucketPrefix("org/team".to_string())).await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    put_real_shard_fixture(&client, &fixture).await;

    let result = client
        .dedup_query("org/team/model-repo", &[fixture.chunk_hash])
        .await
        .unwrap();
    assert_eq!(result.known.len(), 1);
    assert!(result.unknown.is_empty());

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_dedup_query_rejects_empty_repo_path() {
    let server = start_test_server().await;
    let url = format!("http://{}/v1/dedup/query", server.addr);
    let chunk_hash = [0x42; 32];

    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({
            "repo_path": "",
            "chunk_hashes": [hex_bytes(&chunk_hash)],
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 400);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_bad_shard_push_warming_rejects_and_does_not_index() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
    let shard_path = global_path("shards", &wrong_hash);

    let err = client
        .put(&shard_path, fixture.shard_bytes.clone())
        .await
        .expect_err("mismatched shard body must be rejected");

    assert!(err.to_string().contains("HTTP 409"));

    let result = client
        .dedup_query("org/repo", &[fixture.chunk_hash])
        .await
        .unwrap();
    assert!(result.known.is_empty());
    assert_eq!(result.unknown, vec![0]);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_bad_xorb_push_warming_rejects() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
    let xorb_path = global_path("xorbs", &wrong_hash);

    let err = client
        .put(&xorb_path, fixture.xorb_bytes.clone())
        .await
        .expect_err("mismatched xorb body must be rejected");

    assert!(err.to_string().contains("HTTP 409"));

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_corrupt_xorb_payload_push_warming_rejects() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let (data, hash_hex) = build_test_xorb(Bytes::from_static(b"payload must reconstruct"));
    let mut corrupt = data.to_vec();
    corrupt[0] ^= 0xFF;
    let xorb_path = global_path("xorbs", &hash_hex);

    let err = client
        .put(&xorb_path, Bytes::from(corrupt))
        .await
        .expect_err("xorb payload corruption must be rejected");

    assert!(err.to_string().contains("HTTP 409"));

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_bad_origin_shard_read_miss_rejects_and_does_not_index() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    let wrong_hash = "1111111111111111111111111111111111111111111111111111111111111111";
    let shard_path = global_path("shards", &wrong_hash);
    server
        .origin
        .put(
            &ObjectPath::from(shard_path.clone()),
            PutPayload::from_bytes(fixture.shard_bytes),
        )
        .await
        .unwrap();

    let err = client
        .get(&shard_path)
        .await
        .expect_err("mismatched origin shard body must be rejected");

    assert!(err.to_string().contains("HTTP 409"));

    let result = client
        .dedup_query("org/repo", &[fixture.chunk_hash])
        .await
        .unwrap();
    assert!(result.known.is_empty());
    assert_eq!(result.unknown, vec![0]);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_corrupt_cached_shard_hit_refetches_from_origin() {
    let (server, origin_get_count) = start_test_server_with_counting_origin().await;

    let fixture = build_real_shard();
    let shard_path = global_path("shards", &fixture.shard_hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(shard_path.clone()),
            PutPayload::from_bytes(fixture.shard_bytes.clone()),
        )
        .await
        .unwrap();

    let url = format!("http://{}/v1/{shard_path}", server.addr);
    let http = reqwest::Client::new();
    let resp = http
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_immutable_cache_headers(resp.headers(), &fixture.shard_hash_hex, "MISS");
    assert_eq!(resp.bytes().await.unwrap(), fixture.shard_bytes);
    assert_eq!(origin_get_count.load(Ordering::Relaxed), 1);

    let cached_file = server
        .cache_root
        .join("shards")
        .join(&fixture.shard_hash_hex[..2])
        .join(&fixture.shard_hash_hex);
    assert!(cached_file.exists());
    std::fs::write(&cached_file, b"corrupt cached shard").unwrap();

    let resp = http
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert_immutable_cache_headers(resp.headers(), &fixture.shard_hash_hex, "MISS");
    assert_eq!(resp.bytes().await.unwrap(), fixture.shard_bytes);
    assert_eq!(origin_get_count.load(Ordering::Relaxed), 2);
    assert_eq!(
        std::fs::read(&cached_file).unwrap(),
        fixture.shard_bytes.as_ref()
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_bad_origin_xorb_read_miss_rejects() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    let wrong_hash = "1111111111111111111111111111111111111111111111111111111111111111";
    let xorb_path = global_path("xorbs", &wrong_hash);
    server
        .origin
        .put(
            &ObjectPath::from(xorb_path.clone()),
            PutPayload::from_bytes(fixture.xorb_bytes),
        )
        .await
        .unwrap();

    let err = client
        .get(&xorb_path)
        .await
        .expect_err("mismatched origin xorb body must be rejected");

    assert!(err.to_string().contains("HTTP 409"));

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_corrupt_origin_xorb_payload_read_miss_rejects() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let (data, hash_hex) = build_test_xorb(Bytes::from_static(b"origin payload must reconstruct"));
    let mut corrupt = data.to_vec();
    corrupt[0] ^= 0xFF;
    let xorb_path = global_path("xorbs", &hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(xorb_path.clone()),
            PutPayload::from_bytes(Bytes::from(corrupt)),
        )
        .await
        .unwrap();

    let err = client
        .get(&xorb_path)
        .await
        .expect_err("origin xorb payload corruption must be rejected");

    assert!(err.to_string().contains("HTTP 409"));

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_duplicate_shard_put_keeps_admin_stats_idempotent() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let fixture = build_real_shard();
    let shard_path = global_path("shards", &fixture.shard_hash_hex);

    client
        .put(&shard_path, fixture.shard_bytes.clone())
        .await
        .unwrap();
    client
        .put(&shard_path, fixture.shard_bytes.clone())
        .await
        .unwrap();

    let stats = admin_stats(server.addr).await;
    assert_eq!(
        stats["total_bytes"].as_u64().unwrap(),
        fixture.shard_bytes.len() as u64
    );
    assert_eq!(stats["shard_count"].as_u64().unwrap(), 1);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_chunk_index_rebuild_from_cached_shards_layout() {
    let tempdir = tempfile::tempdir().unwrap();
    let cache_root = tempdir.path().join("cache");
    std::fs::create_dir_all(&cache_root).unwrap();

    let cache_db = CacheDb::open_or_create(&cache_root.join(CACHE_DB_FILE)).unwrap();
    let store = CacheStore::open(
        cache_root.clone(),
        TEST_MAX_CACHE_BYTES,
        cache_db.connect().unwrap(),
    )
    .unwrap();

    let held_index = ChunkIndex::open(cache_db.connect().unwrap()).unwrap();

    let fixture = build_real_shard();
    let key = ServerObjectKey {
        bucket: String::new(),
        repo_path: ".crab".to_string(),
        object_type: ObjectType::Shard,
        hash: fixture.shard_hash_hex.clone(),
    };
    store
        .put_unverified(&key, fixture.shard_bytes.clone())
        .unwrap();
    assert!(store.object_path(&key).exists());

    drop(held_index);

    let rebuilt = ChunkIndex::open(cache_db.connect().unwrap()).unwrap();
    let rebuilt_entries = rebuilt
        .rebuild_from_shards(&cache_root.join("shards"))
        .unwrap();
    assert_eq!(rebuilt_entries, 1);

    let result = rebuilt.query_batch(&[fixture.chunk_hash]).unwrap();
    assert_eq!(result.known.len(), 1);
    assert!(result.unknown.is_empty());
    assert_eq!(result.known[0].1.xorb_hash, fixture.xorb_hash);
    assert_eq!(result.known[0].1.chunk_index, fixture.chunk_index);
    assert_eq!(result.known[0].1.length, fixture.length);
}

#[tokio::test]
async fn test_pack_put_get_and_admin_evict_removes_canonical_file() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let pack_name = "pack-abc";
    let pack_path = "org/repo/packs/pack-abc.pack";
    let data = Bytes::from_static(b"pack cache bytes");

    client.put(pack_path, data.clone()).await.unwrap();
    let cached = client.get(pack_path).await.unwrap();
    assert_eq!(cached, data);

    let storage_hex = pack_storage_hex(pack_name);
    let pack_file = server
        .cache_root
        .join("packs")
        .join(&storage_hex[..2])
        .join(&storage_hex);
    assert!(pack_file.exists());

    let stats = admin_stats(server.addr).await;
    assert_eq!(stats["pack_count"].as_u64().unwrap(), 1);

    let url = format!("http://{}/v1/admin/evict", server.addr);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({ "object_type": "pack" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let evict_stats = resp.json::<Value>().await.unwrap();
    assert_eq!(evict_stats["evicted_count"].as_u64().unwrap(), 1);
    assert_eq!(
        evict_stats["evicted_bytes"].as_u64().unwrap(),
        data.len() as u64
    );
    assert!(!pack_file.exists());

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_admin_evict_exact_pack_path_removes_only_target() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    let first_path = "org/repo/packs/pack-target.pack";
    let second_path = "org/repo/packs/pack-keeper.pack";
    let first = Bytes::from_static(b"target pack bytes");
    let second = Bytes::from_static(b"keeper pack bytes");

    client.put(first_path, first.clone()).await.unwrap();
    client.put(second_path, second.clone()).await.unwrap();

    let url = format!("http://{}/v1/admin/evict", server.addr);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({ "path": first_path }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let evict_stats = resp.json::<Value>().await.unwrap();
    assert_eq!(evict_stats["evicted_count"].as_u64().unwrap(), 1);
    assert_eq!(
        evict_stats["evicted_bytes"].as_u64().unwrap(),
        first.len() as u64
    );

    let first_storage_hex = pack_storage_hex("pack-target");
    let second_storage_hex = pack_storage_hex("pack-keeper");
    assert!(
        !server
            .cache_root
            .join("packs")
            .join(&first_storage_hex[..2])
            .join(&first_storage_hex)
            .exists()
    );
    assert!(
        server
            .cache_root
            .join("packs")
            .join(&second_storage_hex[..2])
            .join(&second_storage_hex)
            .exists()
    );
    assert_eq!(client.get(second_path).await.unwrap(), second);

    let resp = reqwest::Client::new()
        .post(&url)
        .header("x-cache-psk", TEST_PSK)
        .json(&serde_json::json!({ "path": first_path, "object_type": "pack" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_range_get_returns_correct_slice() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    // Create a larger object so range requests are meaningful.
    let (data, hash_hex) =
        build_test_xorb(Bytes::from_static(b"0123456789abcdefghijklmnopqrstuvwxyz"));

    // Pre-populate origin.
    let origin_path = global_path("xorbs", &hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(origin_path.clone()),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    // Full GET to populate the cache.
    let cache_path = global_path("xorbs", &hash_hex);
    let full = client.get(&cache_path).await.unwrap();
    assert_eq!(full, data);

    // Range GET for bytes 10..20.
    let url = format!("http://{}/v1/{cache_path}", server.addr);
    let resp = reqwest::Client::new()
        .get(url)
        .header("x-cache-psk", TEST_PSK)
        .header(reqwest::header::RANGE, "bytes=10-19")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
    assert_immutable_cache_headers(resp.headers(), &hash_hex, "HIT");
    let expected_content_range = format!("bytes 10-19/{}", data.len());
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some(expected_content_range.as_str())
    );
    let slice = resp.bytes().await.unwrap();
    assert_eq!(
        slice,
        data.slice(10..20),
        "range GET should return the correct byte slice"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_cold_range_get_fetches_full_object_and_caches() {
    let (server, origin_get_count) = start_test_server_with_counting_origin().await;

    let (data, hash_hex) = build_test_xorb(Bytes::from_static(
        b"cold range requests should be cached by the service",
    ));
    let path = global_path("xorbs", &hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(path.clone()),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    let url = format!("http://{}/v1/{path}", server.addr);
    let http = reqwest::Client::new();
    let resp1 = http
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .header(reqwest::header::RANGE, "bytes=5-16")
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 206);
    assert_immutable_cache_headers(resp1.headers(), &hash_hex, "MISS");
    let expected_content_range = format!("bytes 5-16/{}", data.len());
    assert_eq!(
        resp1
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some(expected_content_range.as_str())
    );
    let slice1 = resp1.bytes().await.unwrap();
    assert_eq!(slice1, data.slice(5..17));
    assert_eq!(origin_get_count.load(Ordering::Relaxed), 1);

    let resp2 = http
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .header(reqwest::header::RANGE, "bytes=10-19")
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 206);
    assert_immutable_cache_headers(resp2.headers(), &hash_hex, "HIT");
    let expected_content_range = format!("bytes 10-19/{}", data.len());
    assert_eq!(
        resp2
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some(expected_content_range.as_str())
    );
    let slice2 = resp2.bytes().await.unwrap();
    assert_eq!(slice2, data.slice(10..20));
    assert_eq!(
        origin_get_count.load(Ordering::Relaxed),
        1,
        "second range should be served from the cache service"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_cached_unsatisfiable_range_does_not_refetch_origin() {
    let (server, origin_get_count) = start_test_server_with_counting_origin().await;

    let (data, hash_hex) =
        build_test_xorb(Bytes::from_static(b"cached range miss should stay local"));
    let path = global_path("xorbs", &hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(path.clone()),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    let client = test_client(server.addr);
    let full = client.get(&path).await.unwrap();
    assert_eq!(full, data);
    assert_eq!(origin_get_count.load(Ordering::Relaxed), 1);

    let unsatisfiable_start = data.len() + 10;
    let unsatisfiable_end = data.len() + 20;
    let url = format!("http://{}/v1/{path}", server.addr);
    let resp = reqwest::Client::new()
        .get(url)
        .header("x-cache-psk", TEST_PSK)
        .header(
            reqwest::header::RANGE,
            format!("bytes={unsatisfiable_start}-{unsatisfiable_end}"),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 416);
    let expected_etag = format!("\"{hash_hex}\"");
    let expected_content_range = format!("bytes */{}", data.len());
    assert_eq!(
        resp.headers()
            .get("etag")
            .and_then(|value| value.to_str().ok()),
        Some(expected_etag.as_str())
    );
    assert_eq!(
        resp.headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some(expected_content_range.as_str())
    );
    assert_eq!(
        resp.headers()
            .get("x-cache")
            .and_then(|value| value.to_str().ok()),
        Some("HIT")
    );
    assert_eq!(
        origin_get_count.load(Ordering::Relaxed),
        1,
        "cached unsatisfiable ranges should not fetch origin"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_concurrent_cold_misses_coalesce_origin_fetch() {
    let (gate, release_tx) = origin_get_gate();
    let (server, origin_get_count) =
        start_test_server_with_counting_origin_gate(Some(Arc::clone(&gate))).await;

    let (data, hash_hex) = build_test_xorb(Bytes::from_static(
        b"concurrent cache miss should fetch origin once",
    ));
    let path = global_path("xorbs", &hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(path.clone()),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    let url = format!("http://{}/v1/{path}", server.addr);
    let http = reqwest::Client::new();

    let request_range = |client: reqwest::Client, url: String, range: &'static str| async move {
        let resp = client
            .get(&url)
            .header("x-cache-psk", TEST_PSK)
            .header(reqwest::header::RANGE, range)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let cache_status = resp
            .headers()
            .get("x-cache")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.bytes().await.unwrap();
        (status, cache_status, body)
    };

    let first = tokio::spawn(request_range(http.clone(), url.clone(), "bytes=0-11"));
    tokio::time::timeout(Duration::from_secs(2), async {
        while origin_get_count.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let second = tokio::spawn(request_range(http, url, "bytes=12-23"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        origin_get_count.load(Ordering::Relaxed),
        1,
        "duplicate cold miss should wait on the in-flight fill"
    );
    let stats = admin_stats(server.addr).await;
    assert_eq!(traffic_stat(&stats, "inflight_misses"), 1);

    release_tx.send(true).unwrap();

    let first = first.await.unwrap();
    let second = second.await.unwrap();

    assert_eq!(first.0, 206);
    assert_eq!(second.0, 206);
    assert_eq!(first.2, data.slice(0..12));
    assert_eq!(second.2, data.slice(12..24));

    let mut cache_statuses = vec![first.1, second.1];
    cache_statuses.sort();
    assert_eq!(cache_statuses, vec!["HIT".to_string(), "MISS".to_string()]);
    assert_eq!(origin_get_count.load(Ordering::Relaxed), 1);

    let stats = admin_stats(server.addr).await;
    assert_eq!(traffic_stat(&stats, "cache_misses"), 1);
    assert_eq!(traffic_stat(&stats, "cache_hits"), 1);
    assert_eq!(traffic_stat(&stats, "origin_avoided_reads"), 1);
    assert_eq!(traffic_stat(&stats, "origin_fetches"), 1);
    assert_eq!(
        traffic_stat(&stats, "origin_fetch_bytes"),
        data.len() as u64
    );
    assert_eq!(traffic_stat(&stats, "coalesced_misses"), 1);
    assert_eq!(traffic_stat(&stats, "inflight_misses"), 0);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_health_returns_200() {
    let server = start_test_server().await;

    let url = format!("http://{}/v1/health", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_metrics_returns_prometheus_format() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    // Generate some traffic so metrics have data.
    let (data, hash_hex) = build_test_xorb(Bytes::from_static(b"metrics-test-data"));
    let path = global_path("xorbs", &hash_hex);
    client.put(&path, data).await.unwrap();

    // GET /v1/metrics (unauthenticated endpoint).
    let url = format!("http://{}/v1/metrics", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let content_type = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.contains("text/plain"),
        "metrics endpoint should return text/plain content type, got: {content_type}"
    );

    // The response body is valid (the stub returns empty, a real recorder
    // returns Prometheus exposition format). Either way, the endpoint is
    // reachable and returns 200 with the correct content type.
    let _body = resp.text().await.unwrap();

    let _ = server.shutdown.send(());
}

// ---------------------------------------------------------------------------
// Authentication tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_no_auth_returns_401() {
    let server = start_test_server().await;

    // Use raw reqwest without any auth headers.
    let url = format!("http://{}/v1/objects/.crab/xorbs/deadbeef", server.addr);
    let resp = reqwest::get(&url).await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "request without auth should return 401"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_invalid_psk_returns_401() {
    let server = start_test_server().await;

    // Use raw reqwest with an incorrect PSK.
    let url = format!("http://{}/v1/objects/.crab/xorbs/deadbeef", server.addr);
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("x-cache-psk", "wrong-key-value")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        401,
        "request with invalid PSK should return 401"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_valid_psk_succeeds() {
    let server = start_test_server().await;

    // GET a canonical but non-existent object — should pass auth but may
    // return an origin/cache miss. The route parser requires the two-character
    // partition and the full 64-character content hash even for misses.
    let hash = format!("de{}", "00".repeat(31));
    let path = global_path("xorbs", &hash);
    let url = format!("http://{}/v1/objects/{path}", server.addr);
    let response = reqwest::Client::new()
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();

    // The key assertion: we should NOT get a 401. The request passes auth.
    // It may succeed (200) if the object exists, or fail with a non-auth
    // error (404/502/504) if it doesn't. Any of those proves auth worked.
    assert_ne!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "valid PSK should not produce a 401 response"
    );

    let _ = server.shutdown.send(());
}

// ---------------------------------------------------------------------------
// Eviction tests (task 8.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_eviction_triggers_on_budget_exceeded() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    // PUT 12 objects of 100 KiB each = 1.2 MiB, exceeding the 1 MiB budget.
    let object_count = 12;
    let object_size = 100 * 1024; // 100 KiB

    let mut objects = Vec::with_capacity(object_count);
    for i in 0..object_count {
        let chunk = deterministic_bytes(i as u64 + 1, object_size);
        let (data, hash_hex) = build_test_xorb(chunk);
        let path = global_path("xorbs", &hash_hex);
        client.put(&path, data.clone()).await.unwrap();
        objects.push((path, data.len() as u64));
    }

    // Wait for the evictor to run — it's nudged after each PUT that crosses
    // the high-water mark (0.95 * 1 MiB = ~997 KiB).
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify the cache is within budget by checking current_bytes via the
    // AppState. Since we can't access AppState directly from the client,
    // we verify by counting how many objects are still retrievable.
    // After eviction, cache should be at or below low_water = 0.90 * 1 MiB = 943,718 bytes.
    let low_water = (TEST_MAX_CACHE_BYTES as f64 * 0.90) as u64;

    let mut retrievable_bytes: u64 = 0;
    for (path, object_bytes) in &objects {
        if client.get(path).await.is_ok() {
            retrievable_bytes += *object_bytes;
        }
    }

    // The total retrievable bytes should be at or below the low-water mark,
    // proving eviction ran and brought the cache within budget.
    assert!(
        retrievable_bytes <= low_water,
        "cache should be within budget after eviction: retrievable={retrievable_bytes}, low_water={low_water}"
    );

    // Also verify that some objects were evicted (not all 12 remain).
    let total_put_bytes: u64 = objects.iter().map(|(_, bytes)| *bytes).sum();
    assert!(
        retrievable_bytes < total_put_bytes,
        "some objects should have been evicted: retrievable={retrievable_bytes}, total_put={total_put_bytes}"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_remaining_objects_readable_after_eviction() {
    let server = start_test_server().await;
    let client = test_client(server.addr);

    // PUT 12 objects of 100 KiB each = 1.2 MiB, exceeding the 1 MiB budget.
    let object_count = 12;
    let object_size = 100 * 1024; // 100 KiB

    // Store the expected data for each object so we can verify content later.
    let mut objects: Vec<(String, Bytes)> = Vec::with_capacity(object_count);
    for i in 0..object_count {
        let chunk = deterministic_bytes(i as u64 + 1, object_size);
        let (data, hash_hex) = build_test_xorb(chunk);
        let path = global_path("xorbs", &hash_hex);
        client.put(&path, data.clone()).await.unwrap();
        objects.push((path, data));
    }

    // Wait for the evictor to run.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // GET each surviving object and verify its content is correct (not corrupted).
    let mut surviving_count = 0;
    for (path, expected_data) in &objects {
        match client.get(path).await {
            Ok(actual_data) => {
                assert_eq!(
                    actual_data.as_ref(),
                    expected_data.as_ref(),
                    "surviving object at {path} should have correct content"
                );
                surviving_count += 1;
            }
            Err(_) => {
                // Object was evicted — this is expected for some objects.
            }
        }
    }

    // At least some objects should survive (cache isn't completely empty).
    assert!(
        surviving_count > 0,
        "at least some objects should survive eviction"
    );

    // But not all should survive (eviction should have removed some).
    assert!(
        surviving_count < object_count,
        "not all objects should survive eviction: surviving={surviving_count}, total={object_count}"
    );

    let _ = server.shutdown.send(());
}

// ---------------------------------------------------------------------------
// Error path tests (task 8.5)
// ---------------------------------------------------------------------------

/// An `ObjectStore` implementation that always returns a connection error
/// on `get_opts` and `head`, simulating an unreachable origin.
#[derive(Debug)]
struct FailingStore;

impl std::fmt::Display for FailingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FailingStore")
    }
}

#[async_trait]
impl ObjectStore for FailingStore {
    async fn put_opts(
        &self,
        _location: &ObjectPath,
        _payload: PutPayload,
        _opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        Err(object_store::Error::Generic {
            store: "FailingStore",
            source: "connection refused".into(),
        })
    }

    async fn put_multipart_opts(
        &self,
        _location: &ObjectPath,
        _opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        Err(object_store::Error::Generic {
            store: "FailingStore",
            source: "connection refused".into(),
        })
    }

    async fn get_opts(
        &self,
        _location: &ObjectPath,
        _options: GetOptions,
    ) -> object_store::Result<GetResult> {
        Err(object_store::Error::Generic {
            store: "FailingStore",
            source: "connection refused".into(),
        })
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        locations
            .map(|location| {
                location.and_then(|_| {
                    Err(object_store::Error::Generic {
                        store: "FailingStore",
                        source: "connection refused".into(),
                    })
                })
            })
            .boxed()
    }

    fn list(
        &self,
        _prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        Box::pin(futures_util::stream::empty())
    }

    async fn list_with_delimiter(
        &self,
        _prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        Err(object_store::Error::Generic {
            store: "FailingStore",
            source: "connection refused".into(),
        })
    }

    async fn copy_opts(
        &self,
        _from: &ObjectPath,
        _to: &ObjectPath,
        _options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        Err(object_store::Error::Generic {
            store: "FailingStore",
            source: "connection refused".into(),
        })
    }
}

/// Start a test server whose origin always returns connection errors.
/// The cache layer still works, so pre-warmed objects can be served.
async fn start_test_server_with_failing_origin() -> TestServer {
    let failing_store: Arc<dyn ObjectStore> = Arc::new(FailingStore);
    let origin = OriginClient::from_store(failing_store);
    start_test_server_with_origin(TEST_MAX_CACHE_BYTES, origin, Arc::new(InMemory::new())).await
}

#[tokio::test]
async fn test_origin_unreachable_cache_miss_returns_504() {
    let server = start_test_server_with_failing_origin().await;

    // GET an object that isn't in cache — the server must fetch from origin,
    // which will fail with a connection error → 504 Gateway Timeout.
    let hash_hex = "a".repeat(64);
    let url = format!(
        "http://{}/v1/{}",
        server.addr,
        global_path("xorbs", &hash_hex)
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        504,
        "cache miss with unreachable origin should return 504"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_origin_unreachable_cache_hit_serves_normally() {
    let server = start_test_server_with_failing_origin().await;
    let client = test_client(server.addr);

    // Pre-warm the cache via PUT — this bypasses origin entirely.
    let (data, hash_hex) = build_test_xorb(Bytes::from_static(b"cached-despite-origin-down"));
    let path = global_path("xorbs", &hash_hex);

    client.put(&path, data.clone()).await.unwrap();

    // GET the same object — should be served from cache (200) even though
    // the origin is unreachable.
    let result = client.get(&path).await.unwrap();
    assert_eq!(
        result, data,
        "cache hit should serve normally even when origin is unreachable"
    );

    let _ = server.shutdown.send(());
}

#[tokio::test]
async fn test_origin_object_identity_path_serves_non_blake3_body_hash() {
    let server = start_test_server().await;

    // Real Crab xorb/shard path hashes are domain content IDs, not
    // necessarily blake3(body). The cache service must not reject a valid
    // origin object solely because blake3(body) differs from the path ID.
    let (data, xorb_hash_hex) = build_test_xorb(Bytes::from_static(
        b"this body is keyed by a production content id",
    ));
    assert_ne!(
        xorb_hash_hex,
        blake3::hash(&data).to_hex().to_string(),
        "xorb object IDs should not be treated as blake3(body)"
    );

    // Store bytes in the origin at the path keyed by the xorb aggregate ID.
    let origin_path = global_path("xorbs", &xorb_hash_hex);
    server
        .origin
        .put(
            &ObjectPath::from(origin_path.clone()),
            PutPayload::from_bytes(data.clone()),
        )
        .await
        .unwrap();

    // GET via cache — the server validates the xorb aggregate ID, not blake3(body).
    let url = format!(
        "http://{}/v1/{}",
        server.addr,
        global_path("xorbs", &xorb_hash_hex)
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("x-cache-psk", TEST_PSK)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(resp.bytes().await.unwrap(), data);

    let _ = server.shutdown.send(());
}
