//! Benchmark: sweep_orphans with varying dead-segment ratios.
//!
//! Creates a staging area with sealed segments at 10%, 50%, and 100%
//! dead ratios, then benchmarks `sweep_orphans()`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{cell::OnceCell, path::Path};

use crab_staging::StagingArea;
use crab_xet::hash::compute_data_hash;
use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};

const SEGMENT_PAYLOAD_BYTES: usize = 256 * 1024 * 1024;
/// Number of segments to create for the sweep benchmark.
const SEGMENT_COUNT: usize = 10;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn dead_segment_count(segment_count: usize, dead_pct: usize) -> usize {
    if dead_pct == 0 {
        0
    } else {
        (segment_count * dead_pct).div_ceil(100)
    }
}

fn clone_staging_fixture(template_root: &Path) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");

    std::fs::copy(template_root.join("index.db"), tmp.path().join("index.db")).expect("copy index");

    let source_segments = template_root.join("segments");
    let target_segments = tmp.path().join("segments");
    std::fs::create_dir_all(&target_segments).expect("segments dir");

    for entry in std::fs::read_dir(source_segments).expect("read segments") {
        let entry = entry.expect("segment entry");
        if !entry.file_type().expect("segment type").is_file() {
            continue;
        }

        let source = entry.path();
        let target = target_segments.join(entry.file_name());
        if std::fs::hard_link(&source, &target).is_err() {
            std::fs::copy(&source, &target).expect("copy segment");
        }
    }

    tmp
}

/// Populate a staging area with `segment_count` sealed segments.
///
/// `dead_pct` controls what fraction of segments have zero live chunks
/// (i.e. are sweep candidates). Segments are made dead by first
/// registering their file rows, then unregistering them so pending rows
/// are gone and `live_chunk_count` drops to zero.
///
/// Returns the tempdir, which must be kept alive while the fixture is used.
fn populate_for_sweep(segment_count: usize, dead_pct: usize) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let runtime = rt();

    runtime.block_on(async {
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");

        let dead_count = dead_segment_count(segment_count, dead_pct);
        for seg in 0..segment_count {
            let is_dead = seg < dead_count;
            let mut data = vec![0u8; SEGMENT_PAYLOAD_BYTES];
            data[..8].copy_from_slice(&(seg as u64).to_le_bytes());
            let chunk_hash = compute_data_hash(&data);

            let total_bytes = SEGMENT_PAYLOAD_BYTES as u64;
            let file_id = format!("staging-sweep-file-{seg}");
            let file_hash = compute_data_hash(file_id.as_bytes());
            staging
                .pre_register_file(&file_hash, total_bytes)
                .expect("pre_register_file");
            staging
                .stage_chunks_batch(&[(&chunk_hash, data.as_slice())], &file_hash, 0)
                .await
                .expect("stage_chunks_batch");

            staging
                .register_file(&file_hash, total_bytes, &[(chunk_hash, total_bytes)])
                .expect("register_file");

            if is_dead {
                let removed = staging
                    .unregister_file(&file_hash)
                    .expect("unregister_file");
                assert!(removed, "dead segment file should unregister");
            }
        }

        staging.close().await.expect("close");
    });

    tmp
}

fn bench_sweep_orphans(c: &mut Criterion) {
    let mut group = c.benchmark_group("staging_sweep");
    group.sample_size(10);

    for &dead_pct in &[10, 50, 100] {
        let template = OnceCell::new();

        group.bench_with_input(
            BenchmarkId::new("sweep_orphans", format!("{dead_pct}pct_dead")),
            &dead_pct,
            move |b, _| {
                let runtime = rt();
                b.iter_batched(
                    || {
                        let template =
                            template.get_or_init(|| populate_for_sweep(SEGMENT_COUNT, dead_pct));
                        clone_staging_fixture(template.path())
                    },
                    |tmp| {
                        let path = tmp.path().to_path_buf();
                        runtime.block_on(async {
                            let staging = StagingArea::open(path).await.expect("open");
                            let _ = staging.sweep_orphans().expect("sweep_orphans");
                            staging.close().await.expect("close");
                        });
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_sweep_orphans);
criterion_main!(benches);
