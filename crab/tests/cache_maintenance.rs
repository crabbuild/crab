//! Actual maintenance commands must use private payload ownership rules.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crab::cache::{CacheKey, LocalCache, XetChunkCacheHandle};
use crab_xet::hash::compute_data_hash;
use crab_xet::xorb::builder::{RunId, XorbBuilder};
use crab_xet::xorb::format::Chunk;
use crab_xet::xorb::parser::XorbParser;
use xet_client::cas_types::{ChunkRange, Key};

const MAINTENANCE_COMMANDS: &[&[&str]] = &[
    &["cache", "clean"],
    &["optimize", "cache", "clean"],
    &["cache", "verify"],
    &["optimize", "cache", "verify"],
    &["prune"],
    &["prune", "--dry-run"],
    &["optimize", "cache", "prune"],
    &["optimize", "cache", "prune", "--dry-run"],
];

fn command_output(directory: &Path, root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crab"))
        .args(args)
        .current_dir(directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("CRAB_CACHE_DIR", root)
        .output()
        .unwrap()
}

fn command(directory: &Path, root: &Path, args: &[&str]) -> Vec<u8> {
    let output = command_output(directory, root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn unlimited_config_reports_no_cap_and_explicit_cap_can_be_restored() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing-cache");
    for setting in [None, Some("1048576"), Some("unlimited"), Some("0")] {
        if let Some(value) = setting {
            command(
                temp.path(),
                &root,
                &["config", "set", "cache.max_bytes", value],
            );
        }
        let output = command(temp.path(), &root, &["cache", "stats", "--json"]);
        let data = stats_data(&output);
        match setting {
            Some("1048576") => assert_eq!(data["budget_bytes"], 1_048_576),
            Some("0") => assert_eq!(data["budget_bytes"], 0),
            _ => assert!(data["budget_bytes"].is_null()),
        }
        assert!(!root.exists());
    }
}

#[tokio::test]
async fn unlimited_prune_retains_objects_until_user_sets_a_cap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let cache = LocalCache::new(root.clone());
    let key = CacheKey::Chunk(compute_data_hash(b"data"));
    cache.put(&key, b"data").await.unwrap();
    assert_eq!(cache.max_bytes(), None);
    for args in [&["prune"][..], &["optimize", "cache", "prune"][..]] {
        command(temp.path(), &root, args);
        let hit = cache
            .get_or_fetch(&key, || async {
                Err(crab_cache::CacheError::Internal(
                    "unexpected cache miss".into(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(hit.as_ref(), b"data");
    }
    command(
        temp.path(),
        &root,
        &["config", "set", "cache.max_bytes", "0"],
    );
    command(temp.path(), &root, &["prune"]);
    assert_eq!(cache.stats().await.unwrap().chunk_count, 0);
}

#[test]
fn filter_process_creates_a_private_cache_under_public_umask() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing-cache");
    let mut protocol = Vec::new();
    for lines in [
        &["git-filter-client\n", "version=2\n"][..],
        &["capability=clean\n", "capability=smudge\n"][..],
    ] {
        for line in lines {
            protocol.extend_from_slice(format!("{:04x}{line}", line.len() + 4).as_bytes());
        }
        protocol.extend_from_slice(b"0000");
    }
    // The real filter saves session state even after a smudge-only/empty Git
    // session. Start with a missing root: precreating it hides the producer bug.
    for _ in 0..2 {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "umask 022; exec \"$@\"", "sh"])
            .arg(env!("CARGO_BIN_EXE_crab"))
            .arg("filter-process")
            .current_dir(temp.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("CRAB_CACHE_DIR", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&protocol).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("git-filter-server"));
        for (path, mode) in [
            (root.clone(), 0o700),
            (root.join("hints"), 0o700),
            (root.join("hints/clean-bloom.bin"), 0o600),
        ] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
        assert!(
            LocalCache::new(root.clone())
                .read_clean_bloom()
                .unwrap()
                .is_some()
        );
    }
    command(
        temp.path(),
        &root,
        &["optimize", "cache", "prune", "--json"],
    );
    command(temp.path(), &root, &["cache", "clean"]);
    assert!(!root.join("hints/clean-bloom.bin").exists());
}

#[tokio::test]
async fn maintenance_commands_reject_checkout_roots_without_touching_payloads() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    let cache = LocalCache::new(root.clone());
    let hash = compute_data_hash(b"data");
    let key = CacheKey::Chunk(hash);
    cache.put(&key, b"data").await.unwrap();
    let hex = hash.hex();
    let payload = root.join("chunks").join(&hex[..2]).join(&hex);
    // A canonical name is not proof that a broad user directory is a cache.
    // Corrupt content also makes the verify command's destructive path eligible.
    std::fs::write(&payload, b"bad!").unwrap();
    let sentinel = root.join("user-notes");
    std::fs::write(&sentinel, b"keep").unwrap();
    for (directory, relative_root) in [
        (root.clone(), Path::new(".")),
        (root.join("nested"), Path::new("..")),
    ] {
        let config = directory.join(crab::core::config::REPO_CONFIG_REL);
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(config, "[cache]\nmax_bytes = 1\n").unwrap();
        for selected_root in [root.as_path(), relative_root] {
            for args in MAINTENANCE_COMMANDS {
                let output = command_output(&directory, selected_root, args);
                assert!(
                    !output.status.success(),
                    "{args:?} accepted checkout root {selected_root:?}"
                );
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains("cache directory is unsafe for cleanup"),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(std::fs::read(&payload).unwrap(), b"bad!");
                assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
            }
        }
    }
}

#[test]
fn maintenance_commands_leave_missing_roots_missing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing");
    for args in MAINTENANCE_COMMANDS {
        command(temp.path(), &root, args);
        assert!(!root.exists(), "{args:?} created a cache root");
    }
}

const STATS_COMMANDS: &[&[&str]] = &[
    &["cache", "stats"],
    &["optimize", "cache", "stats"],
    &["cache", "stats", "--json"],
    &["optimize", "cache", "stats", "--json"],
];

fn stats_data(stdout: &[u8]) -> serde_json::Value {
    // Parsing the entire stdout also rejects a second trailing envelope.
    let envelope: serde_json::Value = serde_json::from_slice(stdout).unwrap();
    assert_eq!(envelope["schema"], "cache.stats");
    assert_eq!(envelope["version"], "1.0");
    assert!(envelope.get("error").is_none());
    envelope["data"].clone()
}

fn family_columns<'a>(stdout: &'a str, family: &str) -> Vec<&'a str> {
    stdout
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|columns| columns.first() == Some(&family))
        .unwrap()
}

#[test]
fn stats_commands_leave_missing_roots_and_range_directories_missing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("missing");
    for args in STATS_COMMANDS {
        command(temp.path(), &root, args);
        assert!(!root.exists(), "{args:?} created a cache root");
    }
    crab_cache::ensure_private_cache_directory(&root).unwrap();
    for args in STATS_COMMANDS {
        command(temp.path(), &root, args);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}

fn cache_tree(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    use std::os::unix::fs::MetadataExt as _;

    let mut tree = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        // Access time is not a mutation contract: a read-only stat/read can
        // update atime. Preserve contents, identity, mode, length and mtime.
        let mut state = format!(
            "{}:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        )
        .into_bytes();
        if metadata.is_file() {
            state.extend(std::fs::read(&path).unwrap());
        } else if metadata.is_dir() {
            pending.extend(
                std::fs::read_dir(&path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        }
        tree.insert(path.strip_prefix(root).unwrap().to_owned(), state);
    }
    tree
}

#[tokio::test]
async fn stats_commands_count_chunk_payloads_without_mutating_cache_state() {
    use std::os::unix::fs::PermissionsExt as _;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let cache = LocalCache::new(root.clone());
    cache
        .put(&CacheKey::Chunk(compute_data_hash(b"data")), b"data")
        .await
        .unwrap();
    cache
        .put(&CacheKey::Shard(compute_data_hash(b"shard")), b"shard")
        .await
        .unwrap();
    // Unrelated index bodies are counted but never opened or repaired.
    std::fs::write(root.join("shard-hints.db"), b"invalid sqlite").unwrap();
    std::fs::set_permissions(
        root.join("shard-hints.db"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let before = cache_tree(&root);
    for args in STATS_COMMANDS {
        let output = command(temp.path(), &root, args);
        if args.contains(&"--json") {
            let data = stats_data(&output);
            assert_eq!(data["families"]["chunk"]["logical_bytes"], 4);
            assert_eq!(data["families"]["shard"]["logical_bytes"], 5);
            assert_eq!(data["families"]["shard-hint"]["logical_bytes"], 14);
            assert_eq!(data["catalog"]["state"], "readable");
            assert_eq!(data["scan_complete"], true);
        } else {
            let stdout = String::from_utf8(output).unwrap();
            let columns = family_columns(&stdout, "chunk");
            assert_eq!(
                (&columns[1..4], columns[5]),
                (&["1", "0", "4"][..], "inspected")
            );
        }
        assert_eq!(cache_tree(&root), before, "{args:?} mutated cache state");
    }
}

#[tokio::test]
async fn stats_commands_report_healthy_groups_when_the_other_is_unsafe() {
    use std::os::unix::fs::PermissionsExt as _;

    for unsafe_family in ["ranges", "shards"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let cache = LocalCache::new(root.clone());
        let shard_hash = compute_data_hash(b"shard");
        cache
            .put(&CacheKey::Shard(shard_hash), b"shard")
            .await
            .unwrap();
        let range_root = root.join("chunks");
        let range_cache = XetChunkCacheHandle::open(&range_root, 1024 * 1024).unwrap();
        let key = Key {
            prefix: crab_cache::xet_chunk_cache::CHUNK_HASH_PREFIX.into(),
            hash: (*blake3::hash(b"data").as_bytes()).into(),
        };
        range_cache
            .cache
            .put(&key, &ChunkRange::new(0, 1), &[0, 4], b"data")
            .await
            .unwrap();
        let unsafe_path = if unsafe_family == "ranges" {
            let mut encoded = key.hash.as_bytes().to_vec();
            encoded.extend_from_slice(key.prefix.as_bytes());
            let encoded: String = encoded.iter().map(|byte| format!("{byte:02x}")).collect();
            range_root
                .join(format!("r-{}", &encoded[..2]))
                .join(encoded)
        } else {
            let hex = shard_hash.hex();
            root.join("shards").join(&hex[..2]).join(hex)
        };
        std::fs::set_permissions(&unsafe_path, std::fs::Permissions::from_mode(0o777)).unwrap();
        let before = cache_tree(&root);
        for args in STATS_COMMANDS {
            let output = command_output(temp.path(), &root, args);
            assert!(
                !output.status.success(),
                "{args:?} hid unsafe {unsafe_family}"
            );
            let (healthy, bytes, unsafe_name) = if unsafe_family == "ranges" {
                ("shard", 5, "decoded-range")
            } else {
                ("decoded-range", 16, "shard")
            };
            if args.contains(&"--json") {
                let data = stats_data(&output.stdout);
                assert_eq!(data["families"][healthy]["logical_bytes"], bytes);
                assert_eq!(data["families"][healthy]["complete"], true);
                assert_eq!(data["families"][unsafe_name]["complete"], false);
                assert_eq!(data["scan_complete"], false);
            } else {
                let stdout = String::from_utf8(output.stdout).unwrap();
                assert_eq!(family_columns(&stdout, healthy)[3], bytes.to_string());
                assert_eq!(family_columns(&stdout, healthy)[5], "inspected");
                assert_eq!(family_columns(&stdout, unsafe_name)[5], "partial");
            }
            assert_eq!(cache_tree(&root), before, "{args:?} repaired unsafe state");
        }
    }
}

#[test]
fn stats_commands_reject_invalid_configuration_without_creating_a_cache() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join(crab::core::config::REPO_CONFIG_REL);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[cache]\nmax_bytes = \"invalid\"\n").unwrap();
    let root = temp.path().join("missing");
    for args in STATS_COMMANDS {
        let output = command_output(temp.path(), &root, args);
        assert!(
            !output.status.success(),
            "{args:?} ignored invalid configuration"
        );
        assert!(!root.exists());
        if args.contains(&"--json") {
            let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(envelope["schema"], "cache.stats");
            assert!(envelope.get("error").is_some());
            assert!(envelope.get("data").is_none());
        }
    }
}

#[tokio::test]
async fn stats_commands_do_not_inspect_ranges_through_an_aliased_cache_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let range_cache = XetChunkCacheHandle::open(root.join("chunks"), 1024 * 1024).unwrap();
    let key = Key {
        prefix: crab_cache::xet_chunk_cache::CHUNK_HASH_PREFIX.into(),
        hash: (*blake3::hash(b"data").as_bytes()).into(),
    };
    range_cache
        .cache
        .put(&key, &ChunkRange::new(0, 1), &[0, 4], b"data")
        .await
        .unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();
    let before = cache_tree(&root);
    for args in STATS_COMMANDS {
        let output = command_output(temp.path(), &alias, args);
        assert!(!output.status.success());
        if args.contains(&"--json") {
            let data = stats_data(&output.stdout);
            assert_eq!(data["root_state"], "unavailable");
            assert_eq!(data["observed"]["logical_bytes"], 0);
            assert_eq!(data["scan_complete"], false);
            assert_eq!(data["over_budget"], false);
        } else {
            let stdout = String::from_utf8(output.stdout).unwrap();
            assert!(stdout.contains("(Unavailable)"), "{stdout}");
            assert!(stdout.contains("partial scan; lower bounds"), "{stdout}");
        }
        assert_eq!(cache_tree(&root), before);
    }
}

#[tokio::test]
async fn doctor_reports_missing_unsafe_corrupt_and_over_budget_cache_without_mutation() {
    use std::os::unix::fs::PermissionsExt as _;

    for state in ["missing", "unsafe", "corrupt", "over-budget"] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        if state != "missing" {
            LocalCache::new(root.clone())
                .put(&CacheKey::Shard(compute_data_hash(b"data")), b"data")
                .await
                .unwrap();
        }
        match state {
            "unsafe" => std::fs::set_permissions(
                root.join("shards"),
                std::fs::Permissions::from_mode(0o777),
            )
            .unwrap(),
            "corrupt" => std::fs::write(root.join(".catalog.sqlite"), b"invalid sqlite").unwrap(),
            "over-budget" => {
                let config = temp.path().join(crab::core::config::REPO_CONFIG_REL);
                std::fs::create_dir_all(config.parent().unwrap()).unwrap();
                std::fs::write(config, "[cache]\nmax_bytes = 1\n").unwrap();
            }
            _ => {}
        }
        let before = root.exists().then(|| cache_tree(&root));
        let output = command(temp.path(), &root, &["doctor", "--json"]);
        let envelope: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let checks = envelope["data"]["checks"].as_array().unwrap();
        let (name, status, detail) = match state {
            "missing" => ("local cache", "ok", "not yet created"),
            "unsafe" => ("cache family", "fail", "owner-only permissions"),
            "corrupt" => ("cache family", "warn", "preserve the affected database"),
            _ => ("cache budget", "warn", "exceeds the effective budget"),
        };
        assert!(
            checks.iter().any(|check| check["name"] == name
                && check["status"] == status
                && check["detail"].as_str().unwrap().contains(detail)),
            "{state}: {}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(
            root.exists().then(|| cache_tree(&root)),
            before,
            "doctor mutated {state} cache"
        );
    }
}

#[tokio::test]
async fn object_maintenance_commands_preserve_unknown_state_and_remote_proofs() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let config = temp.path().join(crab::core::config::REPO_CONFIG_REL);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[cache]\nmax_bytes = 1\n").unwrap();
    let cache = LocalCache::new(root.clone());
    let hash = compute_data_hash(b"data");
    let mut builder = XorbBuilder::new();
    builder
        .push(&Chunk::new(bytes::Bytes::from_static(b"data")), RunId(0))
        .unwrap();
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let digest = XorbParser::parse(xorb.bytes.clone())
        .unwrap()
        .payload_digest();
    let objects = [
        ("chunks", hash, CacheKey::Chunk(hash), b"data".as_slice()),
        ("shards", hash, CacheKey::Shard(hash), b"data".as_slice()),
        (
            "xorbs",
            xorb.hash,
            CacheKey::Xorb(xorb.hash),
            xorb.bytes.as_ref(),
        ),
    ];
    for (_, _, key, data) in &objects {
        cache.put(key, data).await.unwrap();
    }
    assert!(
        cache
            .record_remote_xorb_proof(
                &xorb.hash,
                &digest,
                xorb.bytes.len() as u64,
                Some("origin-etag"),
                None
            )
            .unwrap()
    );
    let mut retained = Vec::new();
    for (family, hash, _, _) in &objects {
        let hex = hash.hex();
        let parent = root.join(family).join(&hex[..2]);
        std::fs::write(parent.join(hex), b"corrupt").unwrap();
        let sentinel = parent.join("notes.txt");
        std::fs::write(&sentinel, b"sentinel").unwrap();
        retained.push(sentinel);
    }
    let output = command(temp.path(), &root, &["cache", "verify"]);
    assert_eq!(
        String::from_utf8(output).unwrap().trim(),
        "Checked 3 cache object(s): 0 valid, 3 corrupt evicted"
    );
    for (_, _, key, data) in &objects {
        assert!(!cache.contains(key).await);
        cache.put(key, data).await.unwrap();
    }
    let expected_bytes = 8 + xorb.bytes.len() as u64;
    let preview: serde_json::Value = serde_json::from_slice(&command(
        temp.path(),
        &root,
        &["prune", "--dry-run", "--json"],
    ))
    .unwrap();
    assert_eq!(preview["data"]["objects_pruned"], 3);
    assert_eq!(preview["data"]["bytes_freed"], expected_bytes);
    for (_, _, key, _) in &objects {
        assert!(cache.contains(key).await);
    }
    let applied: serde_json::Value =
        serde_json::from_slice(&command(temp.path(), &root, &["prune", "--json"])).unwrap();
    assert_eq!(applied["data"]["objects_pruned"], 3);
    assert_eq!(applied["data"]["bytes_freed"], expected_bytes);
    for (_, _, key, _) in &objects {
        assert!(!cache.contains(key).await);
    }
    assert!(
        cache
            .remote_xorb_proof_matches(
                &xorb.hash,
                &digest,
                xorb.bytes.len() as u64,
                Some("origin-etag"),
                None
            )
            .unwrap()
    );
    for path in retained {
        assert_eq!(std::fs::read(path).unwrap(), b"sentinel");
    }
}

#[tokio::test]
async fn verify_and_prune_commands_preserve_non_range_owners() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cache");
    let config = temp.path().join(crab::core::config::REPO_CONFIG_REL);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "[cache]\nmax_bytes = 1\n").unwrap();
    let range_root = root.join("chunks");
    let cache = XetChunkCacheHandle::open(&range_root, 1024 * 1024).unwrap();
    let key = Key {
        prefix: crab_cache::xet_chunk_cache::CHUNK_HASH_PREFIX.into(),
        hash: (*blake3::hash(b"good").as_bytes()).into(),
    };
    let range = ChunkRange::new(0, 1);
    cache
        .cache
        .put(&key, &range, &[0, 4], b"bad!")
        .await
        .unwrap();
    let mut encoded = key.hash.as_bytes().to_vec();
    encoded.extend_from_slice(key.prefix.as_bytes());
    let encoded: String = encoded.iter().map(|byte| format!("{byte:02x}")).collect();
    let entry_dir = range_root
        .join(format!("r-{}", &encoded[..2]))
        .join(encoded);
    let payload = std::fs::read_dir(&entry_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let retained = [entry_dir.join("notes.txt"), entry_dir.join(".tmp-sentinel")];
    for path in &retained {
        std::fs::write(path, b"sentinel").unwrap();
    }
    let report = command(temp.path(), &root, &["cache", "verify"]);
    assert_eq!(
        String::from_utf8(report).unwrap().trim(),
        "Checked 1 cache object(s): 0 valid, 1 corrupt evicted"
    );
    assert!(!payload.exists());

    cache
        .cache
        .put(&key, &range, &[0, 4], b"good")
        .await
        .unwrap();
    let preview: serde_json::Value = serde_json::from_slice(&command(
        temp.path(),
        &root,
        &["prune", "--dry-run", "--json"],
    ))
    .unwrap();
    assert_eq!(preview["data"]["objects_pruned"], 1);
    assert_eq!(preview["data"]["bytes_freed"], 16);
    assert!(cache.cache.get(&key, &range).await.unwrap().is_some());
    let applied: serde_json::Value =
        serde_json::from_slice(&command(temp.path(), &root, &["prune", "--json"])).unwrap();
    assert_eq!(applied["data"]["objects_pruned"], 1);
    assert_eq!(applied["data"]["bytes_freed"], 16);
    assert!(cache.cache.get(&key, &range).await.unwrap().is_none());
    for path in retained {
        assert_eq!(std::fs::read(path).unwrap(), b"sentinel");
    }
}
