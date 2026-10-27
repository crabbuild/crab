use super::*;

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
