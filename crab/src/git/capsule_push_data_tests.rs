use super::*;
use std::io::{Read, Seek, Write};

use crab_staging::recipe::{ChunkingPolicyId, FileRecipe};
use crab_staging::{StagingArea, StagingAreaReadOnly};
use crab_types::pointer::Pointer;
use crab_xet::chunker::GearChunker;
use proptest::prelude::*;
use rand::{RngCore, SeedableRng, rngs::StdRng};

static POINTER_CASES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

struct PointerScenario {
    staging: Arc<StagingAreaReadOnly>,
    publication: PolicyFixture,
    files: Vec<(Pointer, tempfile::NamedTempFile)>,
    // Release test memory admission after staged and origin owners drop.
    _admission: tokio::sync::SemaphorePermit<'static>,
}

impl PointerScenario {
    async fn new(spec: &[(usize, u64)]) -> Self {
        let admission = POINTER_CASES.acquire().await.unwrap();
        let publication = PolicyFixture::new("refs/heads/main").await;
        let staging_root = publication.repository.path().join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let mut files = Vec::new();
        for (index, (count, seed)) in spec.iter().enumerate() {
            let name = format!("pointer-{index}.bin");
            let file = stage_pointer_file(&staging, &name, *count, *seed).await;
            eprintln!(
                "prepared_pointer_file chunks={count} bytes={} seed={seed}",
                file.0.size
            );
            std::fs::write(
                publication.repository.path().join(&name),
                file.0.serialize(),
            )
            .unwrap();
            git(publication.repository.path(), &["add", &name]);
            files.push(file);
        }
        staging.close().await.unwrap();
        git(
            publication.repository.path(),
            &["commit", "-m", "pointer corpus"],
        );
        let staging = Arc::new(StagingAreaReadOnly::open(staging_root).await.unwrap());
        for (index, (pointer, _)) in files.iter().enumerate() {
            let hash = MerkleHash::from(pointer.file_hash);
            assert_eq!(staging.chunks_for_file(&hash).unwrap().len(), spec[index].0);
        }
        Self {
            staging,
            publication,
            files,
            _admission: admission,
        }
    }

    async fn publish(&self) -> Result<PushResult> {
        let config = PushConfig {
            git_dir: Some(self.publication.repository.path().join(".git")),
            ..PushConfig::default()
        };
        run(
            &config,
            &[policy_spec("HEAD", "refs/heads/main")],
            &self.publication.store,
            &self.publication.router,
            None,
            &[],
            Some(&self.staging),
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .map(|(result, _)| result)
    }
}

async fn stage_pointer_file(
    staging: &StagingArea,
    name: &str,
    target_chunks: usize,
    seed: u64,
) -> (Pointer, tempfile::NamedTempFile) {
    let mut expected = tempfile::NamedTempFile::new().unwrap();
    let mut rng = StdRng::seed_from_u64(seed);
    let mut buffer = vec![0_u8; 128 * 1024];
    let mut chunker = GearChunker::new();
    let mut hasher = blake3::Hasher::new();
    let mut count = 0;
    let mut recipe_chunks = Vec::with_capacity(target_chunks);
    let generation = std::time::Instant::now();
    // End at a real CDC boundary so the corpus exercises an exact chunk count,
    // without keeping a multi-GiB source and its chunk copies in the test heap.
    while count < target_chunks {
        rng.fill_bytes(&mut buffer);
        for chunk in chunker.feed(&buffer) {
            expected.write_all(&chunk.data).unwrap();
            hasher.update(&chunk.data);
            recipe_chunks.push((chunk.hash, chunk.data.len() as u64));
            count += 1;
            if count == target_chunks {
                break;
            }
        }
    }
    eprintln!(
        "fixture_generation_s={:.3}",
        generation.elapsed().as_secs_f64()
    );
    let staging_started = std::time::Instant::now();
    let size = expected.as_file().metadata().unwrap().len();
    let file_hash = *hasher.finalize().as_bytes();
    let file_merkle = MerkleHash::from(file_hash);
    staging.pre_register_file(&file_merkle, size).unwrap();
    expected.rewind().unwrap();
    let mut pending = Vec::new();
    let mut offset = 0_u64;
    for (hash, length) in &recipe_chunks {
        let mut data = vec![0_u8; usize::try_from(*length).unwrap()];
        expected.read_exact(&mut data).unwrap();
        pending.push((*hash, Bytes::from(data)));
        if pending.len() == 512 {
            let refs = pending
                .iter()
                .map(|(hash, data)| (hash, data.as_ref()))
                .collect::<Vec<_>>();
            staging
                .stage_chunks_batch(&refs, &file_merkle, offset)
                .await
                .unwrap();
            offset += pending.len() as u64;
            pending.clear();
        }
    }
    if !pending.is_empty() {
        let refs = pending
            .iter()
            .map(|(hash, data)| (hash, data.as_ref()))
            .collect::<Vec<_>>();
        staging
            .stage_chunks_batch(&refs, &file_merkle, offset)
            .await
            .unwrap();
    }
    assert_eq!(expected.read(&mut buffer[..1]).unwrap(), 0);
    assert_eq!(recipe_chunks.len(), target_chunks);
    staging.flush_pending().await.unwrap();
    let recipe = FileRecipe::from_staged_chunks(
        ChunkingPolicyId::XetGearV1_64KiB,
        file_merkle,
        size,
        &recipe_chunks,
    )
    .unwrap();
    staging
        .publish_verified_recipe_lease(Path::new(name), &recipe)
        .unwrap();
    eprintln!(
        "fixture_staging_s={:.3}",
        staging_started.elapsed().as_secs_f64()
    );
    let pointer = Pointer {
        file_hash,
        size,
        shard_hint: None,
    };
    (pointer, expected)
}

async fn assert_pointer_roundtrip(spec: &[(usize, u64)]) {
    let mut scenario = PointerScenario::new(spec).await;
    let publication = std::time::Instant::now();
    assert!(scenario.publish().await.unwrap().all_ok());
    eprintln!(
        "pointer_publication_s={:.3}",
        publication.elapsed().as_secs_f64()
    );
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: 16 * 1024 * 1024,
        max_frontier_bytes: 16 * 1024 * 1024,
    };
    let view = crab_read::capsule_protocol::open_view(&scenario.publication.layout, limits)
        .await
        .unwrap();
    let head = git(
        scenario.publication.repository.path(),
        &["rev-parse", "HEAD"],
    );
    assert_eq!(
        view.refs(),
        &BTreeMap::from([("refs/heads/main".to_owned(), head)])
    );
    let catalog = view.pointer_catalog().await.unwrap();
    assert_eq!(catalog.files().len(), scenario.files.len());
    let output = tempfile::tempdir().unwrap();
    let cache = Arc::new(crate::cache::LocalCache::new(output.path().join("cache")));
    let caching = crab_cache_store::CachingStore::new_with_local_cache(
        scenario.publication.store.clone(),
        &crate::core::config::CacheConfig::default(),
        cache,
    )
    .unwrap();
    let hydrator = crate::read::build_cli_hydrator(
        caching,
        scenario.publication.router.clone(),
        &crate::core::config::Config::default(),
    )
    .unwrap();
    let mut actual_buffer = vec![0_u8; 128 * 1024];
    let mut expected_buffer = vec![0_u8; 128 * 1024];
    for (index, (pointer, expected)) in scenario.files.iter_mut().enumerate() {
        let hash = MerkleHash::from(pointer.file_hash).hex();
        let entry = catalog.files().get(&hash).unwrap();
        assert_eq!(entry.size(), pointer.size);
        assert!(catalog.shards().contains_key(entry.shard_hash()));
        let destination = output.path().join(format!("result-{index}"));
        let hydration = std::time::Instant::now();
        let bytes = hydrator
            .reconstruct_from_pointer_to_path(&pointer.serialize(), &destination)
            .await
            .unwrap();
        eprintln!(
            "pointer_hydration_s={:.3}",
            hydration.elapsed().as_secs_f64()
        );
        assert_eq!(bytes, pointer.size);
        let mut actual = std::fs::File::open(destination).unwrap();
        expected.rewind().unwrap();
        let mut remaining = pointer.size;
        while remaining > 0 {
            let length = usize::try_from(remaining.min(actual_buffer.len() as u64)).unwrap();
            expected.read_exact(&mut expected_buffer[..length]).unwrap();
            actual.read_exact(&mut actual_buffer[..length]).unwrap();
            assert_eq!(actual_buffer[..length], expected_buffer[..length]);
            remaining -= length as u64;
        }
        assert_eq!(actual.read(&mut actual_buffer[..1]).unwrap(), 0);
        assert_eq!(expected.read(&mut expected_buffer[..1]).unwrap(), 0);
        eprintln!(
            "verified_pointer_file chunks={} bytes={} seed={} hash={hash}",
            spec[index].0, pointer.size, spec[index].1
        );
    }
}

#[tokio::test]
async fn pointer_file_roundtrip_at_2k_chunks() {
    assert_pointer_roundtrip(&[(2_000, 0x2c0_0000)]).await;
}

#[tokio::test]
async fn pointer_file_roundtrip_at_5k_chunks() {
    assert_pointer_roundtrip(&[(5_000, 0x5c0_0000)]).await;
}

#[tokio::test]
async fn pointer_recipe_size_mismatch_cannot_publish() {
    let scenario = PointerScenario::new(&[(2, 7)]).await;
    let mut pointer = scenario.files[0].0.clone();
    pointer.size += 1;
    std::fs::write(
        scenario.publication.repository.path().join("pointer-0.bin"),
        pointer.serialize(),
    )
    .unwrap();
    git(
        scenario.publication.repository.path(),
        &["add", "pointer-0.bin"],
    );
    git(
        scenario.publication.repository.path(),
        &["commit", "-m", "wrong declared size"],
    );
    let before = crab_metadata::capsule_protocol::load_root(&scenario.publication.layout)
        .await
        .unwrap();
    assert!(matches!(
        scenario.publish().await,
        Err(CrabError::StagingCorrupt(_))
    ));
    let after = crab_metadata::capsule_protocol::load_root(&scenario.publication.layout)
        .await
        .unwrap();
    assert_eq!(before.record().digest(), after.record().digest());
    assert!(scenario.publication.refs().await.is_empty());
}

#[tokio::test]
#[ignore = "multi-GiB reconstruction corpus; run in the dedicated qualification environment"]
async fn pointer_files_roundtrip_at_large_chunk_counts() {
    for (count, seed) in [
        (22_000, 0x22c_0000),
        (27_000, 0x27c_0000),
        (50_000, 0x50c_0000),
    ] {
        assert_pointer_roundtrip(&[(count, seed)]).await;
    }
}

#[tokio::test]
#[ignore = "multi-GiB reconstruction corpus; run in the dedicated qualification environment"]
async fn five_pointer_files_roundtrip_across_chunk_boundaries() {
    assert_pointer_roundtrip(&[
        (100, 0x100_0000),
        (2_000, 0x200_0000),
        (5_000, 0x500_0000),
        (22_000, 0x220_0000),
        (27_000, 0x270_0000),
    ])
    .await;
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn pointer_single_file_property((count, seed) in (prop::sample::select(vec![1_000usize, 2_000, 5_000, 10_000]), any::<u64>())) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(assert_pointer_roundtrip(&[(count, seed)]));
    }

    #[test]
    #[ignore = "large randomized reconstruction corpus; run in the dedicated qualification environment"]
    fn pointer_large_file_property((count, seed) in (prop::sample::select(vec![2_000usize, 5_000, 22_000, 27_000]), any::<u64>())) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(assert_pointer_roundtrip(&[(count, seed)]));
    }

    #[test]
    fn pointer_small_file_property((count, seed) in (prop::sample::select(vec![100usize, 500, 1_000, 2_000, 4_000]), any::<u64>())) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(assert_pointer_roundtrip(&[(count, seed)]));
    }

    #[test]
    fn pointer_catalog_covers_every_batch_file((count, seed, batch) in (prop::sample::select(vec![500usize, 1_000, 2_000, 4_000]), any::<u64>(), any::<bool>())) {
        let spec = if batch { vec![(count, seed), (count, seed.wrapping_add(1)), (count, seed.wrapping_add(2))] } else { vec![(count, seed)] };
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
            .block_on(assert_pointer_roundtrip(&spec));
    }
}

#[tokio::test]
async fn aggregate_over_pack_limit_publishes_one_complete_capsule() {
    let fixture = PolicyFixture::new("refs/heads/main").await;
    let mut files = Vec::new();
    for index in 0..4_u64 {
        let name = format!("blob-{index}.bin");
        let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ index;
        let bytes = (0..700 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect::<Vec<_>>();
        std::fs::write(fixture.repository.path().join(&name), &bytes).unwrap();
        files.push((name, bytes));
    }
    git(fixture.repository.path(), &["add", "."]);
    git(
        fixture.repository.path(),
        &["commit", "-m", "bounded pack set"],
    );
    let head = git(fixture.repository.path(), &["rev-parse", "HEAD"]);
    let config = PushConfig {
        receive_max_input_size: 1024 * 1024,
        ..PushConfig::default()
    };
    let result = fixture
        .push(&[policy_spec("HEAD", "refs/heads/main")], config.clone())
        .await
        .unwrap();
    assert!(result.all_ok());
    let view = crab_read::capsule_protocol::open_view(
        &fixture.layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert_eq!(
        view.refs(),
        &BTreeMap::from([("refs/heads/main".to_owned(), head.clone())])
    );
    assert_eq!(view.capsule_count().unwrap(), 1);
    let pack_sizes = view
        .capsules()
        .iter()
        .flat_map(|capsule| {
            capsule
                .git_packs()
                .iter()
                .map(|pack| capsule.section_bytes(pack.pack_section()).unwrap().len() as u64)
        })
        .chain(
            view.capsule_controls()
                .iter()
                .flat_map(|capsule| capsule.git_packs().iter().map(|pack| pack.pack().length())),
        )
        .collect::<Vec<_>>();
    assert!(pack_sizes.len() > 1, "fixture must publish multiple packs");
    assert!(
        pack_sizes
            .iter()
            .all(|size| *size <= config.receive_max_input_size)
    );

    let destination = tempfile::tempdir().unwrap();
    git(destination.path(), &["init", "--bare"]);
    crab_read::capsule_protocol::install_git_packs_from_store(
        &view,
        &fixture.layout,
        destination.path(),
        16 * 1024 * 1024,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    git(
        destination.path(),
        &["update-ref", "refs/heads/main", &head],
    );
    git(
        destination.path(),
        &["symbolic-ref", "HEAD", "refs/heads/main"],
    );
    git(destination.path(), &["fsck", "--full", "--strict"]);
    for (name, expected) in files {
        let selector = format!("HEAD:{name}");
        let output = Command::new("git")
            .args(["show", &selector])
            .current_dir(destination.path())
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, expected);
    }
}
