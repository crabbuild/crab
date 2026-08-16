//! Benchmark: staging throughput.
//!
//! Stages N chunks of 64 KiB each and measures throughput in MB/s.
//! Uses criterion for statistical analysis and reporting.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use crab::core::metrics::Metrics;
use crab_staging::StagingArea;
use crab_xet::hash::{MerkleHash, compute_data_hash};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

const CHUNK_SIZE: usize = 64 * 1024; // 64 KiB

/// Build a tokio runtime for running async staging operations.
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Generate `n` distinct 64 KiB chunks with deterministic but unique content.
fn generate_chunks(n: usize) -> Vec<(MerkleHash, Vec<u8>)> {
    (0..n)
        .map(|i| {
            let mut data = vec![0u8; CHUNK_SIZE];
            // Stamp the chunk index into the first 8 bytes for uniqueness.
            data[..8].copy_from_slice(&(i as u64).to_le_bytes());
            let hash = compute_data_hash(&data);
            (hash, data)
        })
        .collect()
}

fn bench_stage_chunks(c: &mut Criterion) {
    let mut group = c.benchmark_group("staging_throughput");

    for &n in &[100, 1000] {
        let total_bytes = (n * CHUNK_SIZE) as u64;
        group.throughput(Throughput::Bytes(total_bytes));

        group.bench_with_input(BenchmarkId::new("stage_chunks", n), &n, |b, &n| {
            let chunks = generate_chunks(n);
            let runtime = rt();

            b.iter(|| {
                runtime.block_on(async {
                    let tmp = tempfile::tempdir().expect("tempdir");
                    let mut staging = StagingArea::open(tmp.path().to_path_buf())
                        .await
                        .expect("open staging");

                    let metrics = Arc::new(Metrics::new());
                    staging.set_metrics(Arc::clone(&metrics));

                    let file_id = format!("staging-bench-file-{n}");
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

                    let snap = metrics.snapshot();
                    // The fsync count is available via metrics for analysis.
                    let _ = snap.staging_fsyncs;

                    staging.close().await.expect("close");
                });
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_stage_chunks);
criterion_main!(benches);
