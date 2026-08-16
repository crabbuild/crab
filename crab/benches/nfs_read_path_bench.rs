//! Synthetic VFS read-path benchmark for the NFS backend design.
//!
//! This benchmark does not mount the OS NFS client. It exercises the shared
//! engine path that NFS uses after `ReadLeasePool` has pinned a `VfsReadLease`:
//! pointer-backed snapshot resolution, engine read-source caching, chunk
//! hydration, chunk-cache hits, overlay backing reads after copy-on-write
//! promotion, and `read_at` on a reused lease.
//!
//! Usage:
//!
//! ```text
//! cargo bench -p crab --bench nfs_read_path_bench --no-default-features --features nfs
//! cargo bench -p crab --bench nfs_read_path_bench -- --size 64MiB --read-size 1MiB
//! ```
//!
//! The harness prints JSON lines so release evidence can archive exact
//! workloads and compare them across commits.

#![allow(clippy::expect_used)]

use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use crab::cache::ChunkCache;
use crab::vfs::data_plane::{FileIndexResolver, ReconstructionTerm, ShardLoader, XorbFetcher};
use crab::vfs::engine::{OverlayWriter, VfsEngine};
use crab::vfs::hydration::HydrationService;
use crab::vfs::overlay::OverlayStore;
use crab::vfs::resolver::{FuseResolver, OverlayLookup};
use crab::vfs::snapshot::{BaseNode, NodeType, SnapshotStore};
use crab::vfs::verified_set::VerifiedSet;
use crab::vfs::{Result as VfsResult, VfsError};
use crab_types::pointer::Pointer;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

const DEFAULT_FILE_SIZE: usize = 32 * 1024 * 1024;
const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;
const DEFAULT_READ_SIZE: usize = 1024 * 1024;
const DEFAULT_RANDOM_READS: usize = 512;
const BENCH_PATH: &str = "model.bin";

#[derive(Debug, Clone, Copy)]
struct BenchOptions {
    file_size: usize,
    chunk_size: usize,
    read_size: usize,
    random_reads: usize,
}

#[derive(Debug, Serialize)]
struct BenchRecord {
    scenario: &'static str,
    file_size: usize,
    chunk_size: usize,
    read_size: usize,
    reads: usize,
    bytes_returned: usize,
    elapsed_ms: u128,
    mib_per_sec: f64,
}

#[derive(Clone)]
struct StaticFileIndexResolver {
    file_hash: [u8; 32],
    shard_hash: [u8; 32],
}

#[derive(Clone)]
struct StaticShardLoader {
    file_hash: [u8; 32],
    shard_hash: [u8; 32],
    terms: Arc<Vec<ReconstructionTerm>>,
}

#[derive(Clone)]
struct StaticXorbFetcher {
    xorb_hash: [u8; 32],
    data: Arc<Vec<u8>>,
}

struct BenchFixture {
    _root: tempfile::TempDir,
    engine: Arc<VfsEngine>,
}

fn main() {
    let options = parse_args();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let fixture = build_fixture(options, false);
    emit(runtime.block_on(sequential_path_reads(&fixture, options)));
    emit(runtime.block_on(sequential_lease_reads(&fixture, options)));
    emit(runtime.block_on(random_path_reads(&fixture, options)));
    emit(runtime.block_on(random_lease_reads(&fixture, options)));

    let overlay_path_fixture = build_fixture(options, true);
    runtime.block_on(prepare_overlay_write(&overlay_path_fixture, options));
    emit(runtime.block_on(overlay_modified_path_rereads(
        &overlay_path_fixture,
        options,
    )));

    let overlay_lease_fixture = build_fixture(options, true);
    runtime.block_on(prepare_overlay_write(&overlay_lease_fixture, options));
    emit(runtime.block_on(overlay_modified_lease_rereads(
        &overlay_lease_fixture,
        options,
    )));
}

async fn sequential_path_reads(fixture: &BenchFixture, options: BenchOptions) -> BenchRecord {
    let ranges = sequential_ranges(options.file_size, options.read_size);
    measure(
        "pointer_sequential_path_read",
        options,
        ranges,
        |offset, size| async move {
            fixture
                .engine
                .read(BENCH_PATH, offset, size)
                .await
                .expect("sequential path read")
        },
    )
    .await
}

async fn sequential_lease_reads(fixture: &BenchFixture, options: BenchOptions) -> BenchRecord {
    let ranges = sequential_ranges(options.file_size, options.read_size);
    let lease = fixture
        .engine
        .open_read(BENCH_PATH)
        .expect("open read lease");
    measure(
        "pointer_sequential_lease_read",
        options,
        ranges,
        |offset, size| {
            let lease = lease.clone();
            async move {
                fixture
                    .engine
                    .read_at(&lease, offset, size)
                    .await
                    .expect("sequential lease read")
            }
        },
    )
    .await
}

async fn random_path_reads(fixture: &BenchFixture, options: BenchOptions) -> BenchRecord {
    let ranges = random_ranges(options.file_size, options.read_size, options.random_reads);
    measure(
        "pointer_random_path_read",
        options,
        ranges,
        |offset, size| async move {
            fixture
                .engine
                .read(BENCH_PATH, offset, size)
                .await
                .expect("random path read")
        },
    )
    .await
}

async fn random_lease_reads(fixture: &BenchFixture, options: BenchOptions) -> BenchRecord {
    let ranges = random_ranges(options.file_size, options.read_size, options.random_reads);
    let lease = fixture
        .engine
        .open_read(BENCH_PATH)
        .expect("open read lease");
    measure(
        "pointer_random_lease_read",
        options,
        ranges,
        |offset, size| {
            let lease = lease.clone();
            async move {
                fixture
                    .engine
                    .read_at(&lease, offset, size)
                    .await
                    .expect("random lease read")
            }
        },
    )
    .await
}

async fn prepare_overlay_write(fixture: &BenchFixture, options: BenchOptions) {
    let patch = overlay_patch(options.read_size);
    let written = fixture
        .engine
        .write(BENCH_PATH, 0, &patch)
        .await
        .expect("overlay write");
    assert_eq!(written, patch.len(), "overlay write length");
}

async fn overlay_modified_path_rereads(
    fixture: &BenchFixture,
    options: BenchOptions,
) -> BenchRecord {
    let ranges = repeated_ranges(options.file_size, options.read_size);
    measure(
        "overlay_modified_path_reread",
        options,
        ranges,
        |offset, size| async move {
            fixture
                .engine
                .read(BENCH_PATH, offset, size)
                .await
                .expect("overlay-modified path reread")
        },
    )
    .await
}

async fn overlay_modified_lease_rereads(
    fixture: &BenchFixture,
    options: BenchOptions,
) -> BenchRecord {
    let ranges = repeated_ranges(options.file_size, options.read_size);
    let lease = fixture
        .engine
        .open_read(BENCH_PATH)
        .expect("open overlay-modified read lease");
    measure(
        "overlay_modified_lease_reread",
        options,
        ranges,
        |offset, size| {
            let lease = lease.clone();
            async move {
                fixture
                    .engine
                    .read_at(&lease, offset, size)
                    .await
                    .expect("overlay-modified lease reread")
            }
        },
    )
    .await
}

async fn measure<F, Fut>(
    scenario: &'static str,
    options: BenchOptions,
    ranges: Vec<(u64, u32)>,
    mut read: F,
) -> BenchRecord
where
    F: FnMut(u64, u32) -> Fut,
    Fut: std::future::Future<Output = Bytes>,
{
    let started = Instant::now();
    let mut bytes_returned = 0usize;
    for (offset, size) in &ranges {
        bytes_returned = bytes_returned.saturating_add(read(*offset, *size).await.len());
    }
    let elapsed = started.elapsed();
    let elapsed_ms = elapsed.as_millis();
    let secs = elapsed.as_secs_f64().max(f64::EPSILON);
    let mib_per_sec = bytes_returned as f64 / (1024.0 * 1024.0) / secs;

    BenchRecord {
        scenario,
        file_size: options.file_size,
        chunk_size: options.chunk_size,
        read_size: options.read_size,
        reads: ranges.len(),
        bytes_returned,
        elapsed_ms,
        mib_per_sec,
    }
}

fn build_fixture(options: BenchOptions, writable: bool) -> BenchFixture {
    assert!(options.file_size > 0, "file size must be positive");
    assert!(options.chunk_size > 0, "chunk size must be positive");
    assert_eq!(
        options.file_size % options.chunk_size,
        0,
        "file size must be a multiple of chunk size"
    );

    let root = tempfile::tempdir().expect("tempdir");
    let data = Arc::new(synthetic_file(options.file_size));
    let file_hash = *blake3::hash(&data).as_bytes();
    let shard_hash = *blake3::hash(b"nfs-read-path-bench-shard").as_bytes();
    let xorb_hash = *blake3::hash(&data).as_bytes();
    let pointer = Pointer {
        file_hash,
        size: options.file_size as u64,
        shard_hint: Some(shard_hash),
    };
    let terms = Arc::new(reconstruction_terms(&data, options.chunk_size, xorb_hash));

    let snapshot = Arc::new(
        SnapshotStore::open_or_create(&root.path().join("snapshot.sqlite"))
            .expect("open snapshot store"),
    );
    snapshot
        .publish_generation(
            "0000000000000000000000000000000000000000",
            "refs/heads/bench",
            &[BaseNode {
                path: BENCH_PATH.to_owned(),
                node_type: NodeType::File,
                mode: 0o100_644,
                object_oid: None,
                pointer: Some(pointer),
                size: options.file_size as u64,
            }],
        )
        .expect("publish snapshot");
    let generation = snapshot
        .current_generation()
        .expect("read current generation")
        .expect("snapshot generation");
    let overlay = if writable {
        Some(Arc::new(
            OverlayStore::open(&root.path().join("overlay.db"), &root.path().join("upper"))
                .expect("open overlay store"),
        ))
    } else {
        None
    };
    let overlay_lookup: Option<Arc<dyn OverlayLookup>> = overlay.as_ref().map(|store| {
        let store: Arc<dyn OverlayLookup> = store.clone();
        store
    });
    let overlay_writer: Option<Arc<dyn OverlayWriter>> = overlay.as_ref().map(|store| {
        let store: Arc<dyn OverlayWriter> = store.clone();
        store
    });
    let resolver = Arc::new(FuseResolver::new(
        Arc::clone(&snapshot),
        overlay_lookup,
        generation,
        0,
    ));

    let hydration = HydrationService::new(
        Arc::new(ChunkCache::open(root.path().join("chunks"), None).expect("open chunk cache")),
        Arc::new(VerifiedSet::default()),
        Arc::new(StaticFileIndexResolver {
            file_hash,
            shard_hash,
        }),
        Arc::new(StaticShardLoader {
            file_hash,
            shard_hash,
            terms,
        }),
        Arc::new(StaticXorbFetcher {
            xorb_hash,
            data: Arc::clone(&data),
        }),
        None,
        None,
        Some(1),
        CancellationToken::new(),
    );

    let engine = Arc::new(VfsEngine::new(
        resolver,
        overlay_writer,
        hydration,
        None,
        Some(snapshot),
    ));
    BenchFixture {
        _root: root,
        engine,
    }
}

fn reconstruction_terms(
    data: &[u8],
    chunk_size: usize,
    xorb_hash: [u8; 32],
) -> Vec<ReconstructionTerm> {
    data.chunks(chunk_size)
        .enumerate()
        .map(|(index, chunk)| ReconstructionTerm {
            xorb_hash,
            offset: (index * chunk_size) as u64,
            length: chunk.len() as u64,
            chunk_hash: *blake3::hash(chunk).as_bytes(),
        })
        .collect()
}

fn synthetic_file(size: usize) -> Vec<u8> {
    (0..size)
        .map(|index| {
            let word = (index as u64)
                .wrapping_mul(6_364_136_223_846_793_005)
                .rotate_left(17);
            (word ^ (word >> 32)) as u8
        })
        .collect()
}

fn overlay_patch(read_size: usize) -> Vec<u8> {
    let len = read_size.min(DEFAULT_CHUNK_SIZE).max(1);
    (0..len)
        .map(|index| 255u8.wrapping_sub((index % 251) as u8))
        .collect()
}

fn sequential_ranges(file_size: usize, read_size: usize) -> Vec<(u64, u32)> {
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while offset < file_size {
        let len = read_size.min(file_size - offset);
        ranges.push((offset as u64, len as u32));
        offset += len;
    }
    ranges
}

fn random_ranges(file_size: usize, read_size: usize, count: usize) -> Vec<(u64, u32)> {
    if file_size <= read_size {
        return vec![(0, file_size as u32); count.max(1)];
    }

    let max_start = file_size - read_size;
    let aligned_slots = (max_start / read_size).max(1);
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut ranges = Vec::with_capacity(count);
    for _ in 0..count {
        state ^= state << 7;
        state ^= state >> 9;
        state = state.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        let slot = (state as usize) % aligned_slots;
        ranges.push(((slot * read_size) as u64, read_size as u32));
    }
    ranges
}

fn repeated_ranges(file_size: usize, read_size: usize) -> Vec<(u64, u32)> {
    let base = sequential_ranges(file_size, read_size);
    let mut ranges = Vec::with_capacity(base.len() * 2);
    for range in base {
        ranges.push(range);
        ranges.push(range);
    }
    ranges
}

fn emit(record: BenchRecord) {
    println!(
        "{}",
        serde_json::to_string(&record).expect("serialize bench record")
    );
}

fn parse_args() -> BenchOptions {
    let mut options = BenchOptions {
        file_size: DEFAULT_FILE_SIZE,
        chunk_size: DEFAULT_CHUNK_SIZE,
        read_size: DEFAULT_READ_SIZE,
        random_reads: DEFAULT_RANDOM_READS,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bench" => {}
            "--size" => options.file_size = parse_size_arg(&next_arg(&mut args, "--size")),
            "--chunk-size" => {
                options.chunk_size = parse_size_arg(&next_arg(&mut args, "--chunk-size"));
            }
            "--read-size" => {
                options.read_size = parse_size_arg(&next_arg(&mut args, "--read-size"));
            }
            "--random-reads" => {
                options.random_reads = next_arg(&mut args, "--random-reads")
                    .parse()
                    .expect("valid --random-reads");
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => panic!("unknown argument {other}"),
        }
    }

    assert!(options.read_size > 0, "read size must be positive");
    assert!(
        options.read_size <= u32::MAX as usize,
        "read size must fit NFS read count"
    );
    options
}

fn next_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next()
        .unwrap_or_else(|| panic!("missing value for {flag}"))
}

fn parse_size_arg(raw: &str) -> usize {
    let lower = raw.trim().to_ascii_lowercase();
    let (number, multiplier) = if let Some(value) = lower.strip_suffix("mib") {
        (value, 1024 * 1024)
    } else if let Some(value) = lower.strip_suffix("mb") {
        (value, 1_000_000)
    } else if let Some(value) = lower.strip_suffix("kib") {
        (value, 1024)
    } else if let Some(value) = lower.strip_suffix("kb") {
        (value, 1_000)
    } else {
        (lower.as_str(), 1)
    };
    number
        .parse::<usize>()
        .map(|value| value.saturating_mul(multiplier))
        .expect("valid size")
}

fn print_help() {
    println!(
        "\
Usage: cargo bench -p crab --bench nfs_read_path_bench -- [OPTIONS]

Options:
  --size BYTES          synthetic pointer file size, supports KiB/MiB suffixes
  --chunk-size BYTES    reconstruction chunk size, default 64KiB
  --read-size BYTES     read request size, default 1MiB
  --random-reads N      number of random read requests, default 512
"
    );
}

impl FileIndexResolver for StaticFileIndexResolver {
    fn resolve_file_index(
        &self,
        file_hash: &[u8; 32],
        shard_hint: Option<&[u8; 32]>,
    ) -> VfsResult<Option<[u8; 32]>> {
        if file_hash == &self.file_hash {
            return Ok(shard_hint.copied().or(Some(self.shard_hash)));
        }
        Ok(None)
    }

    fn scan_shard_list_for_file(&self, file_hash: &[u8; 32]) -> VfsResult<Option<[u8; 32]>> {
        Ok((file_hash == &self.file_hash).then_some(self.shard_hash))
    }
}

impl ShardLoader for StaticShardLoader {
    fn load_reconstruction_terms(
        &self,
        shard_hash: &[u8; 32],
        file_hash: &[u8; 32],
    ) -> VfsResult<Vec<ReconstructionTerm>> {
        if shard_hash == &self.shard_hash && file_hash == &self.file_hash {
            return Ok((*self.terms).clone());
        }
        Err(VfsError::NotFound {
            path: "synthetic reconstruction terms".to_owned(),
        })
    }
}

impl XorbFetcher for StaticXorbFetcher {
    fn fetch_range(&self, xorb_hash: &[u8; 32], range: Range<u64>) -> VfsResult<Vec<u8>> {
        if xorb_hash != &self.xorb_hash {
            return Err(VfsError::NotFound {
                path: "synthetic xorb".to_owned(),
            });
        }
        let start = usize::try_from(range.start)
            .map_err(|_| VfsError::Internal("range start overflow".into()))?;
        let end = usize::try_from(range.end)
            .map_err(|_| VfsError::Internal("range end overflow".into()))?;
        let Some(bytes) = self.data.get(start..end) else {
            return Err(VfsError::Internal(format!(
                "range {start}..{end} is outside synthetic xorb"
            )));
        };
        Ok(bytes.to_vec())
    }
}
