//! Benchmark: staging area open/recovery time.
//!
//! Pre-populates a staging area with synthetic data at different scales
//! (simulating 1 GiB, 10 GiB via many small segments), then benchmarks
//! `StagingArea::open()` time. Since creating 100 GiB of real data in a
//! benchmark is impractical, we use smaller representative sizes and
//! extrapolate.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crab_staging::StagingArea;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};

const CHUNK_SIZE: usize = 64 * 1024; // 64 KiB

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Pre-populate a staging area with `total_bytes` of chunk data, then
/// close it so the next `open` exercises recovery.
fn populate_staging(root: &std::path::Path, total_bytes: u64) {
    let runtime = rt();
    runtime.block_on(async {
        let staging = StagingArea::open(root.to_path_buf())
            .await
            .expect("open for populate");

        let chunks_needed = (total_bytes as usize) / CHUNK_SIZE;
        let chunks: Vec<(MerkleHash, Vec<u8>)> = (0..chunks_needed)
            .map(|i| {
                let mut data = vec![0u8; CHUNK_SIZE];
                data[..8].copy_from_slice(&(i as u64).to_le_bytes());
                let hash = compute_data_hash(&data);
                (hash, data)
            })
            .collect();
        let file_id = format!("staging-recovery-file-{total_bytes}");
        let file_hash = compute_data_hash(file_id.as_bytes());
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre_register_file");
        let refs: Vec<(&MerkleHash, &[u8])> = chunks
            .iter()
            .map(|(hash, data)| (hash, data.as_slice()))
            .collect();
        staging
            .stage_chunks_batch(&refs, &file_hash, 0)
            .await
            .expect("stage_chunks_batch");

        staging.close().await.expect("close");
    });
}

fn bench_open_recovery(c: &mut Criterion) {
    let mut group = c.benchmark_group("staging_recovery");
    // Use shorter measurement time since setup is expensive.
    group.sample_size(10);

    // Simulate different scales. Real 100 GiB is impractical in a bench,
    // so we use representative sizes: 16 MiB (~1 GiB proxy with fewer
    // segments), 160 MiB (~10 GiB proxy), 512 MiB (~100 GiB proxy).
    // The open-time cost is dominated by segment count and SQLite row
    // count, not raw bytes, so these proxies are meaningful.
    let scales: &[(&str, u64)] = &[
        ("16MiB_proxy_1GiB", 16 * 1024 * 1024),
        ("160MiB_proxy_10GiB", 160 * 1024 * 1024),
    ];

    for (label, total_bytes) in scales {
        // Pre-populate once per parameter.
        let tmp = tempfile::tempdir().expect("tempdir");
        populate_staging(tmp.path(), *total_bytes);

        group.bench_with_input(BenchmarkId::new("open", label), total_bytes, |b, _| {
            let runtime = rt();
            b.iter(|| {
                runtime.block_on(async {
                    let staging = StagingArea::open(tmp.path().to_path_buf())
                        .await
                        .expect("open");
                    staging.close().await.expect("close");
                });
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_open_recovery);
criterion_main!(benches);
