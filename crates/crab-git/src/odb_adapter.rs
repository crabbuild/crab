//! ODB adapter: composes the native git ODB with a xorb-backed blob
//! resolver behind a single `gix_object::Find + FindExt` surface.
//!
//! Once this adapter is plugged in, every gitoxide primitive that wants
//! blob bytes — `gix_worktree_state::checkout`, `gix_pack::data::output::
//! FromEntriesIter`, `gix_fsck::Connectivity`, `gix_status::
//! index_as_worktree` — transparently pulls xorb-backed content through
//! the shard reconstruction pipeline.
//!
//! # Dispatch model
//!
//! `try_find` runs three passes:
//!
//! 1. LRU of previously-resolved xorb blobs — served directly from
//!    [`Bytes::clone`] + a memcpy into the caller's buffer.
//! 2. Fast path — [`gix_odb::Handle::try_find`] serves commits, trees,
//!    tags, and every blob that is stored natively in the git ODB.
//! 3. Slow path — when both (1) and (2) miss the adapter asks a
//!    [`XorbBlobResolver`] for the blob bytes. Hits are admitted to the
//!    LRU so the next read of the same `ObjectId` goes through (1).
//!
//! Checking the LRU before the native ODB is safe because entries in
//! the LRU are populated only from successful xorb-resolver calls —
//! reaching the resolver requires the native ODB to have returned
//! `None` already. A cached hit cannot therefore shadow a real git
//! object.
//!
//! # Sync/async boundary
//!
//! `gix_object::Find::try_find` is synchronous. Crab's real hydration
//! path is `async`. The adapter resolves this at the trait boundary:
//! [`XorbBlobResolver`] is a **sync** trait. The built-in
//! `NoopXorbResolver` covers native-only ODBs; product integrations can bridge
//! an async hydrator behind the resolver without making Git's `Find` surface
//! asynchronous.
//!
//! # Thread-safety
//!
//! [`gix_odb::HandleArc`] keeps the shared store in an `Arc` while retaining
//! per-handle caches. Cloning the adapter therefore gives each checkout
//! worker its own `Send` handle instead of serializing native ODB reads.
//! The resolved-blob LRU remains shared behind a mutex.

use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use gix_hash::{ObjectId, oid};
use gix_object::{Data, Find, Kind};
use lru::LruCache;
use thiserror::Error;

macro_rules! gix_boundary {
    ($operation:literal) => {
        tracing::debug_span!(
            concat!("gix.odb.", $operation),
            gix_crate = "odb",
            gix_fn = $operation
        )
    };
}

/// Errors returned by the composite Git object database.
#[derive(Debug, Error)]
pub enum OdbError {
    #[error("git objects directory not found: {path}")]
    ObjectsDirectoryNotFound { path: String },
    #[error("{operation}")]
    Git {
        operation: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{resource} mutex poisoned")]
    Poisoned { resource: &'static str },
}

/// Result type for composite Git object database operations.
pub type Result<T> = std::result::Result<T, OdbError>;

/// Default byte budget for the resolved-blob LRU cache (128 MiB).
///
/// Blobs vary wildly in size, so the cap is measured in bytes rather
/// than entries. A single 200 MiB blob larger than the budget is
/// admitted (and immediately evicted on the next insert) rather than
/// rejected — inserting-then-evicting keeps the eviction path warm.
pub const DEFAULT_BLOB_CACHE_BYTES: u64 = 128 * 1024 * 1024;

/// Synchronous resolver for xorb-backed blobs.
///
/// The adapter queries this after the native git ODB returns `None`.
/// Implementations are responsible for their own sync/async bridging:
/// a production impl built on crab's hydration path wraps its async
/// calls in `spawn_blocking` (or a dedicated runtime) to present a
/// synchronous surface. The adapter never calls `block_on` itself.
///
/// # Errors
///
/// Returns `Ok(None)` when the resolver knows it does not own the
/// given `ObjectId`. `Err(...)` is reserved for genuine failures
/// (shard lookup failed, xorb fetch failed, reconstruction failed).
pub trait XorbBlobResolver: Send + Sync {
    /// Return the reconstructed blob bytes for `id`, or `None` when
    /// the resolver has no entry for this object.
    fn try_resolve_blob(&self, id: &oid) -> Result<Option<Bytes>>;
}

/// Resolver that knows about no blobs. Useful as a default in
/// integration sites that only need native git objects, and as the
/// default until a product-specific resolver is supplied.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopXorbResolver;

impl XorbBlobResolver for NoopXorbResolver {
    fn try_resolve_blob(&self, _id: &oid) -> Result<Option<Bytes>> {
        Ok(None)
    }
}

/// Byte-budget LRU for resolved blobs.
///
/// Internally wraps [`lru::LruCache`] and tracks a running byte total
/// of stored values. The cache uses a large entry-count ceiling so
/// eviction is effectively byte-driven, not count-driven.
struct BlobLruCache {
    inner: LruCache<ObjectId, Bytes>,
    /// Maximum sum of `Bytes::len()` across stored entries.
    byte_budget: u64,
    /// Current sum of `Bytes::len()` across stored entries.
    used_bytes: u64,
}

/// Upper bound on the number of distinct entries the LRU is allowed
/// to hold. The byte budget is the primary eviction trigger, but
/// `LruCache` requires an entry-count cap; ten million is large
/// enough that a realistic blob mix hits the byte budget first.
const BLOB_LRU_ENTRY_CEILING: usize = 10_000_000;

impl BlobLruCache {
    /// Build a cache with `byte_budget` bytes of capacity.
    fn new(byte_budget: u64) -> Self {
        let cap = NonZeroUsize::new(BLOB_LRU_ENTRY_CEILING).unwrap_or(NonZeroUsize::MIN);
        Self {
            inner: LruCache::new(cap),
            byte_budget,
            used_bytes: 0,
        }
    }

    /// Look up an entry. A hit promotes the entry to most-recently-used.
    fn get(&mut self, id: &ObjectId) -> Option<Bytes> {
        self.inner.get(id).cloned()
    }

    /// Insert `bytes` for `id`. Evicts from the back until the cache
    /// fits in its byte budget, or until only the newly-inserted entry
    /// remains (single-entry oversize case — we prefer a warm cache to
    /// repeated slow-path reads).
    fn put(&mut self, id: ObjectId, bytes: Bytes) {
        let len = bytes.len() as u64;

        // Replacing an existing entry — subtract the outgoing size.
        if let Some(old) = self.inner.pop(&id) {
            self.used_bytes = self.used_bytes.saturating_sub(old.len() as u64);
        }

        self.inner.put(id, bytes);
        self.used_bytes = self.used_bytes.saturating_add(len);

        // Byte-budget eviction: pop from the LRU tail until we're under
        // budget or down to a single entry.
        while self.used_bytes > self.byte_budget && self.inner.len() > 1 {
            if let Some((_, evicted)) = self.inner.pop_lru() {
                self.used_bytes = self.used_bytes.saturating_sub(evicted.len() as u64);
            } else {
                break;
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.len()
    }

    #[cfg(test)]
    fn used_bytes(&self) -> u64 {
        self.used_bytes
    }
}

/// Composite object database: native git ODB + xorb-backed blob
/// resolver, wrapped in an LRU.
///
/// Implements [`gix_object::Find`]. The blanket
/// [`gix_object::FindExt`](gix_object::FindExt) impl is picked up for
/// free, so callers can use `find_blob`, `find_tree`, etc.
pub struct CrabOdb {
    git_odb: gix_odb::HandleArc,
    xorb_resolver: Arc<dyn XorbBlobResolver>,
    blob_cache: Arc<Mutex<BlobLruCache>>,
}

impl std::fmt::Debug for CrabOdb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrabOdb").finish_non_exhaustive()
    }
}

impl Clone for CrabOdb {
    /// Clone the ODB handle and shared resolver/cache state for a checkout
    /// worker. The native handle clone has independent per-thread caches.
    fn clone(&self) -> Self {
        Self {
            git_odb: self.git_odb.clone(),
            xorb_resolver: Arc::clone(&self.xorb_resolver),
            blob_cache: Arc::clone(&self.blob_cache),
        }
    }
}

impl CrabOdb {
    /// Open the git ODB at `objects_dir` and pair it with `resolver`.
    ///
    /// Uses the default blob-cache byte budget
    /// ([`DEFAULT_BLOB_CACHE_BYTES`], 128 MiB). Use
    /// [`CrabOdb::with_cache_size`] for a custom budget.
    ///
    /// # Errors
    ///
    /// Returns [`OdbError::ObjectsDirectoryNotFound`] if `objects_dir` does
    /// not exist, or [`OdbError::Git`] if `gix_odb::at` rejects the path.
    pub fn new(objects_dir: &Path, resolver: Arc<dyn XorbBlobResolver>) -> Result<Self> {
        Self::with_cache_size(objects_dir, resolver, DEFAULT_BLOB_CACHE_BYTES)
    }

    /// Open the git ODB at `objects_dir` with a custom cache byte budget.
    pub fn with_cache_size(
        objects_dir: &Path,
        resolver: Arc<dyn XorbBlobResolver>,
        cache_bytes: u64,
    ) -> Result<Self> {
        if !objects_dir.is_dir() {
            return Err(OdbError::ObjectsDirectoryNotFound {
                path: objects_dir.display().to_string(),
            });
        }

        let odb = {
            let _span = gix_boundary!("at").entered();
            gix_odb::at(objects_dir)
                .and_then(gix_odb::Handle::into_arc)
                .map_err(|source| OdbError::Git {
                    operation: format!("failed to open git ODB at {}", objects_dir.display()),
                    source: Box::new(source),
                })?
        };

        Ok(Self {
            git_odb: odb,
            xorb_resolver: resolver,
            blob_cache: Arc::new(Mutex::new(BlobLruCache::new(cache_bytes))),
        })
    }

    /// Internal resolver that returns ODB-domain `Result`s.
    ///
    /// Separated from the trait impl because `gix_object::Find::try_find`
    /// has a fixed error type (`Box<dyn std::error::Error + Send + Sync>`)
    /// while callers want [`OdbError`]. The `Find` impl wraps
    /// anything this returns into the gitoxide error type.
    ///
    /// # Dispatch order
    ///
    /// 1. **LRU of xorb-resolved blobs.** Checked first so repeat reads
    ///    of a xorb-backed OID hit `Bytes::clone` + a memcpy instead of
    ///    paying for a native-ODB lookup on every call. Entries in the
    ///    LRU are always xorb-backed by construction (we only ever
    ///    admit on a successful xorb-resolver call), so short-circuiting
    ///    to the cache does not hide a git object.
    /// 2. **Native git ODB.** Covers commits, trees, tags, and any blob
    ///    stored natively in `{objects_dir}`.
    /// 3. **Xorb resolver.** Asked only after both the LRU and the
    ///    native ODB miss. Hits are admitted into the LRU so the next
    ///    read goes through path (1).
    fn resolve_internal(&self, id: &oid, out_buf: &mut Vec<u8>) -> Result<Option<Kind>> {
        let id_owned = id.to_owned();

        // (1) LRU of xorb-resolved blobs.
        let cached = {
            let mut cache = self.blob_cache.lock().map_err(|_| OdbError::Poisoned {
                resource: "blob cache",
            })?;
            cache.get(&id_owned)
        };
        if let Some(bytes) = cached {
            out_buf.clear();
            out_buf.extend_from_slice(&bytes);
            return Ok(Some(Kind::Blob));
        }

        // (2) Native git ODB. Each checkout worker owns a cloned handle with
        // independent caches over the same thread-safe store.
        {
            let _span = gix_boundary!("try_find").entered();
            let mut git_buf = Vec::new();
            match self.git_odb.try_find(id, &mut git_buf) {
                Ok(Some(data)) => {
                    let kind = data.kind;
                    out_buf.clear();
                    out_buf.extend_from_slice(data.data);
                    return Ok(Some(kind));
                }
                Ok(None) => {
                    // Fall through to the xorb path.
                }
                Err(source) => {
                    return Err(OdbError::Git {
                        operation: format!("git ODB read failed for {id}"),
                        source,
                    });
                }
            }
        }

        // (3) Xorb resolver. Admit hits into the LRU for future reads.
        let resolved = {
            let _span = gix_boundary!("xorb_resolve").entered();
            self.xorb_resolver.try_resolve_blob(id)?
        };

        match resolved {
            Some(bytes) => {
                out_buf.clear();
                out_buf.extend_from_slice(&bytes);
                let mut cache = self.blob_cache.lock().map_err(|_| OdbError::Poisoned {
                    resource: "blob cache",
                })?;
                cache.put(id_owned, bytes);
                Ok(Some(Kind::Blob))
            }
            None => Ok(None),
        }
    }
}

impl Find for CrabOdb {
    fn try_find<'a>(
        &self,
        id: &oid,
        buffer: &'a mut Vec<u8>,
    ) -> std::result::Result<Option<Data<'a>>, gix_object::find::Error> {
        match self.resolve_internal(id, buffer) {
            Ok(Some(kind)) => Ok(Some(Data::new(buffer, kind, gix_hash::Kind::Sha1))),
            Ok(None) => Ok(None),
            // `find::Error` is `Box<dyn Error + Send + Sync + 'static>`; our
            // `OdbError` implements those traits via `thiserror`.
            Err(e) => Err(Box::new(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    use gix_object::FindExt;

    // Test fixtures.

    /// Stub resolver returning canned bytes for a fixed set of OIDs.
    /// Counts slow-path invocations so tests can assert on call counts.
    struct StubResolver {
        entries: std::collections::HashMap<ObjectId, Bytes>,
        calls: AtomicUsize,
    }

    impl StubResolver {
        fn new() -> Self {
            Self {
                entries: std::collections::HashMap::new(),
                calls: AtomicUsize::new(0),
            }
        }

        fn insert(&mut self, id: ObjectId, bytes: Bytes) {
            self.entries.insert(id, bytes);
        }

        fn call_count(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl XorbBlobResolver for StubResolver {
        fn try_resolve_blob(&self, id: &oid) -> Result<Option<Bytes>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.entries.get(&id.to_owned()).cloned())
        }
    }

    /// Build a bare git repo at `git_dir` and write a blob containing
    /// `content`. Returns the blob's OID (hex string).
    fn create_bare_repo_with_blob(
        git_dir: &std::path::Path,
        content: &[u8],
    ) -> std::io::Result<String> {
        use std::io::Write;

        let output = std::process::Command::new("git")
            .args(["init", "--bare"])
            .arg(git_dir)
            .output()?;
        assert!(output.status.success(), "git init --bare failed");

        let mut child = std::process::Command::new("git")
            .args(["hash-object", "-w", "--stdin"])
            .env("GIT_DIR", git_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.as_mut().unwrap().write_all(content)?;
        let output = child.wait_with_output()?;
        assert!(output.status.success(), "git hash-object failed");

        Ok(String::from_utf8(output.stdout).unwrap().trim().to_owned())
    }

    /// Synthesize a random 20-byte SHA-1 OID. The value doesn't have
    /// to exist in any ODB — the xorb resolver is keyed on the raw
    /// bytes so collision risk against real git OIDs is negligible.
    fn synthetic_oid(seed: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[0] = seed;
        bytes[1] = 0xAB;
        bytes[2] = 0xCD;
        bytes[3] = 0xEF;
        ObjectId::from_bytes_or_panic(&bytes)
    }

    // Unit tests.

    #[test]
    fn resolves_native_git_object_via_gix_path() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        let content = b"hello from the native git odb";
        let oid_hex = create_bare_repo_with_blob(&git_dir, content).unwrap();
        let oid = ObjectId::from_hex(oid_hex.as_bytes()).unwrap();

        let resolver = Arc::new(StubResolver::new());
        let odb = CrabOdb::new(&git_dir.join("objects"), resolver.clone()).unwrap();

        let mut buf = Vec::new();
        let data = odb.try_find(&oid, &mut buf).unwrap().expect("blob present");
        assert_eq!(data.kind, Kind::Blob);
        assert_eq!(data.data, content);

        // Slow path must not have been consulted — the blob lived in
        // the native git ODB.
        assert_eq!(
            resolver.call_count(),
            0,
            "xorb resolver should not be called when git ODB has the object"
        );
    }

    #[test]
    fn resolves_xorb_blob_via_xorb_path() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        // Bare repo with no matching blob — the git ODB returns None.
        create_bare_repo_with_blob(&git_dir, b"unrelated blob").unwrap();

        let oid = synthetic_oid(0x01);
        let canned = Bytes::from_static(b"reconstructed xorb content");
        let mut resolver = StubResolver::new();
        resolver.insert(oid, canned.clone());
        let resolver = Arc::new(resolver);

        let odb = CrabOdb::new(&git_dir.join("objects"), resolver.clone()).unwrap();

        let mut buf = Vec::new();
        let data = odb
            .try_find(&oid, &mut buf)
            .unwrap()
            .expect("xorb path should resolve synthesized OID");
        assert_eq!(data.kind, Kind::Blob);
        assert_eq!(data.data, &canned[..]);
        assert_eq!(resolver.call_count(), 1);
    }

    #[test]
    fn returns_none_for_unknown_oid() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        create_bare_repo_with_blob(&git_dir, b"some unrelated blob").unwrap();

        let resolver = Arc::new(StubResolver::new());
        let odb = CrabOdb::new(&git_dir.join("objects"), resolver.clone()).unwrap();

        let unknown = synthetic_oid(0xFE);
        let mut buf = Vec::new();
        let result = odb.try_find(&unknown, &mut buf).unwrap();
        assert!(result.is_none(), "unknown OID should return None");
        // Slow path was consulted exactly once (fast path missed).
        assert_eq!(resolver.call_count(), 1);
    }

    #[test]
    fn lru_reuses_cached_entry_on_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        create_bare_repo_with_blob(&git_dir, b"unrelated").unwrap();

        let oid = synthetic_oid(0x10);
        let canned = Bytes::from_static(b"cached payload");
        let mut resolver = StubResolver::new();
        resolver.insert(oid, canned.clone());
        let resolver = Arc::new(resolver);

        let odb = CrabOdb::new(&git_dir.join("objects"), resolver.clone()).unwrap();

        let mut buf = Vec::new();
        let first = odb.try_find(&oid, &mut buf).unwrap().unwrap();
        assert_eq!(first.data, &canned[..]);

        let mut buf2 = Vec::new();
        let second = odb.try_find(&oid, &mut buf2).unwrap().unwrap();
        assert_eq!(second.data, &canned[..]);

        assert_eq!(
            resolver.call_count(),
            1,
            "second try_find must hit the LRU, not the resolver"
        );
    }

    #[test]
    fn lru_evicts_when_cache_pressure() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        create_bare_repo_with_blob(&git_dir, b"unrelated").unwrap();

        // Three 512-byte blobs, budget of 1 KiB. After inserting all
        // three, at most two fit.
        let big = |seed: u8| Bytes::from(vec![seed; 512]);
        let ids: Vec<_> = (0..3).map(|i| synthetic_oid(0x20 + i)).collect();
        let mut resolver = StubResolver::new();
        for (i, id) in ids.iter().enumerate() {
            resolver.insert(*id, big(i as u8));
        }
        let resolver = Arc::new(resolver);

        let odb = CrabOdb::with_cache_size(
            &git_dir.join("objects"),
            resolver.clone(),
            1024, // 1 KiB budget — fits two 512-byte blobs but not three.
        )
        .unwrap();

        // First pass: populate the cache.
        for id in &ids {
            let mut buf = Vec::new();
            odb.try_find(id, &mut buf).unwrap().unwrap();
        }
        assert_eq!(resolver.call_count(), 3);

        // ids[0] should have been evicted (oldest). Reading it again
        // re-calls the resolver; reading ids[2] (still warm) does not.
        let mut buf = Vec::new();
        odb.try_find(&ids[2], &mut buf).unwrap().unwrap();
        assert_eq!(
            resolver.call_count(),
            3,
            "ids[2] must still be cached after three inserts with 1 KiB budget"
        );

        let mut buf = Vec::new();
        odb.try_find(&ids[0], &mut buf).unwrap().unwrap();
        assert_eq!(
            resolver.call_count(),
            4,
            "ids[0] must have been evicted and re-fetched"
        );
    }

    #[test]
    fn lru_honours_byte_budget_invariant() {
        // White-box check: after every put, used_bytes <= budget OR
        // only one oversized entry remains.
        let mut cache = BlobLruCache::new(256);
        for seed in 0u8..8 {
            let id = synthetic_oid(seed);
            cache.put(id, Bytes::from(vec![seed; 100]));
            assert!(
                cache.used_bytes() <= 256 || cache.len() == 1,
                "byte budget violated: used={}, len={}",
                cache.used_bytes(),
                cache.len()
            );
        }
    }

    // The FindExt blanket impl should resolve a real blob through `find_blob`.

    #[test]
    fn find_ext_blanket_impl_available() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        let content = b"blob for find_ext";
        let oid_hex = create_bare_repo_with_blob(&git_dir, content).unwrap();
        let oid = ObjectId::from_hex(oid_hex.as_bytes()).unwrap();

        let resolver: Arc<dyn XorbBlobResolver> = Arc::new(NoopXorbResolver);
        let odb = CrabOdb::new(&git_dir.join("objects"), resolver).unwrap();

        let mut buf = Vec::new();
        let blob = odb.find_blob(&oid, &mut buf).expect("find_blob works");
        assert_eq!(blob.data, content);
    }

    // Ignored hot-cache microbench.

    #[test]
    #[ignore = "microbench — run with --ignored"]
    fn hot_cache_single_blob_under_20us() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("test.git");
        create_bare_repo_with_blob(&git_dir, b"seed").unwrap();

        let oid = synthetic_oid(0x7F);
        let payload = Bytes::from(vec![0xAA; 128 * 1024]); // 128 KiB blob.
        let mut resolver = StubResolver::new();
        resolver.insert(oid, payload.clone());
        let resolver = Arc::new(resolver);

        let odb = CrabOdb::new(&git_dir.join("objects"), resolver).unwrap();

        // Warm the cache once.
        let mut warm = Vec::new();
        odb.try_find(&oid, &mut warm).unwrap().unwrap();

        let iters = 10_000u32;
        let start = Instant::now();
        for _ in 0..iters {
            let mut buf = Vec::new();
            let data = odb.try_find(&oid, &mut buf).unwrap().unwrap();
            // Touch the data so the compiler doesn't elide the call.
            std::hint::black_box(data.data);
        }
        let elapsed = start.elapsed();
        let per_call_ns = elapsed.as_nanos() / u128::from(iters);
        let per_call_us = per_call_ns as f64 / 1000.0;

        assert!(
            per_call_us < 20.0,
            "hot-cache try_find exceeded 20 µs budget: {per_call_us:.2} µs/op over {iters} iters"
        );
    }
}
