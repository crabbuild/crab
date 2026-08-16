//! Opt-in end-to-end test for `crab import --versions on`
//! against a real S3-compatible versioned bucket.
//!
//! Gated behind `CRAB_E2E=1`. Without the env var the test logs a
//! skip notice and returns cleanly so default CI stays hermetic.
//!
//! Required environment when enabled:
//!
//! - standard AWS credential env vars or provider-chain config
//! - `CRAB_E2E_VERSIONED_BUCKET` (falls back to `CRAB_E2E_FLAT_BUCKET`)
//! - `CRAB_E2E_VERSIONED_PREFIX` (optional scratch prefix)
//! - `CRAB_E2E_S3_ENDPOINT` or `AWS_ENDPOINT_URL` for S3-compatible stores
//!
//! The bucket must already have versioning enabled. The test only
//! mutates object keys under its per-run scratch prefix and removes
//! every object version and delete marker it creates.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

#[cfg(feature = "tier-s3")]
mod versioned {
    use std::env;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::time::{SystemTime, UNIX_EPOCH};

    use aws_sdk_s3::types::{BucketVersioningStatus, Delete, ObjectIdentifier};
    use crab_types::pointer::Pointer;
    use serde_json::Value;
    use tokio::time::{Duration, sleep};

    const GATE_ENV: &str = "CRAB_E2E";
    const BUCKET_ENV: &str = "CRAB_E2E_VERSIONED_BUCKET";
    const FLAT_BUCKET_ENV: &str = "CRAB_E2E_FLAT_BUCKET";
    const PREFIX_ENV: &str = "CRAB_E2E_VERSIONED_PREFIX";
    const ENDPOINT_ENV: &str = "CRAB_E2E_S3_ENDPOINT";
    const DEFAULT_PREFIX: &str = "crab-e2e/import-versioned";

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    struct VersionedObject {
        rel_path: &'static str,
        body: &'static [u8],
    }

    struct Workspace {
        root: PathBuf,
        path_env: OsString,
        push_cache: PathBuf,
        hydrate_cache: PathBuf,
        _tmp: tempfile::TempDir,
    }

    fn bin() -> &'static str {
        env!("CARGO_BIN_EXE_crab")
    }

    fn e2e_gate_enabled() -> bool {
        env::var(GATE_ENV)
            .map(|value| !value.is_empty() && value != "0")
            .unwrap_or(false)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn e2e_import_versioned_bucket_multiple_commits() -> TestResult {
        if !e2e_gate_enabled() {
            eprintln!(
                "skipping e2e_import_versioned_bucket_multiple_commits: {GATE_ENV} not set. \
                 Export {GATE_ENV}=1 plus {BUCKET_ENV} and AWS credentials to run."
            );
            return Ok(());
        }

        let bucket = versioned_bucket()?;
        let client = build_s3_client().await;
        ensure_bucket_versioning_enabled(&client, &bucket).await?;

        let base_prefix = env::var(PREFIX_ENV).unwrap_or_else(|_| DEFAULT_PREFIX.to_owned());
        let run_id = run_id("versioned-import");
        let source_prefix = join_key(&base_prefix, &format!("source/{run_id}"));
        let target_prefix = join_key(&base_prefix, &format!("repos/{run_id}"));
        let workspace = Workspace::new()?;

        cleanup_versions(&client, &bucket, &source_prefix).await?;
        cleanup_versions(&client, &bucket, &target_prefix).await?;

        let model_v1 = VersionedObject {
            rel_path: "crab/large-files/model.bin",
            body: b"model version one\n",
        };
        let model_v2 = VersionedObject {
            rel_path: "crab/large-files/model.bin",
            body: b"model version two has different bytes\n",
        };
        let stable = VersionedObject {
            rel_path: "crab/large-files/stable.txt",
            body: b"stable file survives the delete marker\n",
        };

        put_version(&client, &bucket, &source_prefix, &model_v1).await?;
        sleep(Duration::from_millis(1_100)).await;
        put_version(&client, &bucket, &source_prefix, &model_v2).await?;
        sleep(Duration::from_millis(1_100)).await;
        put_version(&client, &bucket, &source_prefix, &stable).await?;
        sleep(Duration::from_millis(1_100)).await;
        delete_current(&client, &bucket, &source_prefix, model_v2.rel_path).await?;

        let source_url = format!("s3://{bucket}/{source_prefix}/");
        let target_url = format!("crab://{bucket}/{target_prefix}");
        let import_dir = workspace.root.join("import-worktree");
        let clone_dir = workspace.root.join("clone");

        let mut import = workspace.crab(&workspace.push_cache);
        import.args([
            "import",
            "--from",
            &source_url,
            "--to",
            &target_url,
            "--into",
        ]);
        import.arg(&import_dir);
        import.args(["--versions", "on", "--window", "0s", "--yes", "--json"]);

        let import_output = run(import, "crab import --versions on")?;
        assert_versioned_import_summary(&import_output, &source_url, &target_url)?;

        let mut clone = workspace.git(&workspace.hydrate_cache);
        clone.args(["clone", &target_url]);
        clone.arg(&clone_dir);
        run(clone, "git clone")?;

        assert_model_history(&workspace, &clone_dir, &[&model_v1, &model_v2])?;
        assert_final_tree_before_hydrate(&clone_dir, &stable)?;

        let mut hydrate = workspace.crab(&workspace.hydrate_cache);
        hydrate.current_dir(&clone_dir).args(["hydrate", "--all"]);
        run(hydrate, "crab hydrate --all")?;

        assert_eq!(fs::read(clone_dir.join(stable.rel_path))?, stable.body);
        assert!(
            !clone_dir.join(model_v2.rel_path).exists(),
            "delete marker should remove model.bin from HEAD"
        );
        assert_git_clean(&workspace, &clone_dir, "after hydrate")?;

        cleanup_versions(&client, &bucket, &target_prefix).await?;
        cleanup_versions(&client, &bucket, &source_prefix).await?;
        Ok(())
    }

    impl Workspace {
        fn new() -> TestResult<Self> {
            let tmp = tempfile::tempdir()?;
            let helper_dir = tmp.path().join("bin");
            fs::create_dir_all(&helper_dir)?;
            install_remote_helper(&helper_dir)?;

            let mut paths = vec![helper_dir];
            if let Some(existing) = env::var_os("PATH") {
                paths.extend(env::split_paths(&existing));
            }
            let path_env = env::join_paths(paths)?;
            let push_cache = tmp.path().join("push-cache");
            let hydrate_cache = tmp.path().join("hydrate-cache");
            fs::create_dir_all(&push_cache)?;
            fs::create_dir_all(&hydrate_cache)?;

            Ok(Self {
                root: tmp.path().to_path_buf(),
                path_env,
                push_cache,
                hydrate_cache,
                _tmp: tmp,
            })
        }

        fn crab(&self, cache: &Path) -> Command {
            let mut command = Command::new(bin());
            self.apply_env(&mut command, cache);
            command
        }

        fn git(&self, cache: &Path) -> Command {
            let mut command = Command::new("git");
            self.apply_env(&mut command, cache);
            command
        }

        fn apply_env(&self, command: &mut Command, cache: &Path) {
            command
                .env("PATH", &self.path_env)
                .env("CRAB_STORAGE_PROVIDER", "s3")
                .env("CRAB_CACHE_DIR", cache)
                .env("AWS_REGION", aws_region())
                .env("AWS_EC2_METADATA_DISABLED", "true");

            if let Some(endpoint) = s3_endpoint() {
                command.env("AWS_ENDPOINT_URL", &endpoint);
                if endpoint.starts_with("http://") {
                    command.env("AWS_ALLOW_HTTP", "true");
                }
                if env_bool("AWS_VIRTUAL_HOSTED_STYLE_REQUEST").is_none() {
                    command.env("AWS_VIRTUAL_HOSTED_STYLE_REQUEST", "false");
                }
            }
        }
    }

    #[cfg(unix)]
    fn install_remote_helper(helper_dir: &Path) -> TestResult {
        std::os::unix::fs::symlink(bin(), helper_dir.join("git-remote-crab"))?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn install_remote_helper(helper_dir: &Path) -> TestResult {
        fs::copy(bin(), helper_dir.join("git-remote-crab"))?;
        Ok(())
    }

    async fn build_s3_client() -> aws_sdk_s3::Client {
        let endpoint = s3_endpoint();
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(aws_region()));
        if let Some(endpoint) = &endpoint {
            loader = loader.endpoint_url(endpoint.clone());
        }
        let config = loader.load().await;
        let mut builder = aws_sdk_s3::config::Builder::from(&config)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest());
        if let Some(endpoint) = endpoint {
            builder = builder.endpoint_url(endpoint);
            builder = builder.force_path_style(true);
        } else if let Some(virtual_hosted) = env_bool("AWS_VIRTUAL_HOSTED_STYLE_REQUEST") {
            builder = builder.force_path_style(!virtual_hosted);
        }
        aws_sdk_s3::Client::from_conf(builder.build())
    }

    async fn ensure_bucket_versioning_enabled(
        client: &aws_sdk_s3::Client,
        bucket: &str,
    ) -> TestResult {
        let status = client.get_bucket_versioning().bucket(bucket).send().await?;
        if status.status() != Some(&BucketVersioningStatus::Enabled) {
            return Err(format!(
                "{bucket} must have versioning enabled for this E2E; current status: {:?}",
                status.status()
            )
            .into());
        }
        Ok(())
    }

    async fn put_version(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        source_prefix: &str,
        object: &VersionedObject,
    ) -> TestResult {
        client
            .put_object()
            .bucket(bucket)
            .key(join_key(source_prefix, object.rel_path))
            .body(aws_sdk_s3::primitives::ByteStream::from_static(object.body))
            .send()
            .await?;
        Ok(())
    }

    async fn delete_current(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        source_prefix: &str,
        rel_path: &str,
    ) -> TestResult {
        client
            .delete_object()
            .bucket(bucket)
            .key(join_key(source_prefix, rel_path))
            .send()
            .await?;
        Ok(())
    }

    async fn cleanup_versions(
        client: &aws_sdk_s3::Client,
        bucket: &str,
        prefix: &str,
    ) -> TestResult {
        let prefix = normalize_prefix(prefix);
        if prefix.is_empty() {
            return Ok(());
        }

        loop {
            let mut objects = Vec::new();
            let page = client
                .list_object_versions()
                .bucket(bucket)
                .prefix(format!("{prefix}/"))
                .send()
                .await?;

            for version in page.versions() {
                if let (Some(key), Some(version_id)) = (version.key(), version.version_id()) {
                    objects.push(
                        ObjectIdentifier::builder()
                            .key(key)
                            .version_id(version_id)
                            .build()?,
                    );
                }
            }
            for marker in page.delete_markers() {
                if let (Some(key), Some(version_id)) = (marker.key(), marker.version_id()) {
                    objects.push(
                        ObjectIdentifier::builder()
                            .key(key)
                            .version_id(version_id)
                            .build()?,
                    );
                }
            }

            if objects.is_empty() {
                break;
            }

            for chunk in objects.chunks(1000) {
                let delete = Delete::builder()
                    .set_objects(Some(chunk.to_vec()))
                    .quiet(true)
                    .build()?;
                client
                    .delete_objects()
                    .bucket(bucket)
                    .delete(delete)
                    .send()
                    .await?;
            }
        }
        Ok(())
    }

    fn assert_versioned_import_summary(
        output: &Output,
        source_url: &str,
        target_url: &str,
    ) -> TestResult {
        let envelope: Value = serde_json::from_slice(&output.stdout)?;
        let data = envelope
            .get("data")
            .ok_or("import JSON summary missing data field")?;

        assert_eq!(envelope["schema"], "import.summary");
        assert_eq!(data["source_url"], source_url);
        assert_eq!(data["target_url"], target_url);
        assert_eq!(data["versioning"], "versioned");
        assert_eq!(data["versions_imported"].as_u64(), Some(4));
        assert_eq!(data["files_imported"].as_u64(), Some(1));
        assert_eq!(data["same_bucket"].as_bool(), Some(true));
        assert!(
            data["commits_created"].as_u64().unwrap_or_default() >= 3,
            "versioned import should produce a multi-commit history"
        );
        Ok(())
    }

    fn assert_model_history(
        workspace: &Workspace,
        repo: &Path,
        versions: &[&VersionedObject],
    ) -> TestResult {
        let mut log = workspace.git(&workspace.hydrate_cache);
        log.current_dir(repo).args([
            "log",
            "--reverse",
            "--format=%H",
            "--",
            versions[0].rel_path,
        ]);
        let output = run(log, "git log model history")?;
        let commits = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            commits.len(),
            versions.len() + 1,
            "model history should include two versions plus one delete"
        );

        for (commit, expected) in commits.iter().zip(versions.iter()) {
            let mut show = workspace.git(&workspace.hydrate_cache);
            show.current_dir(repo)
                .args(["show", &format!("{commit}:{}", expected.rel_path)]);
            let pointer_bytes = run(show, "git show model pointer")?.stdout;
            let pointer = Pointer::parse(&pointer_bytes)?;
            assert_eq!(pointer.size, expected.body.len() as u64);
            assert_eq!(pointer.file_hash, *blake3::hash(expected.body).as_bytes());
        }

        let deleted_commit = commits.last().expect("delete commit exists");
        let mut exists = workspace.git(&workspace.hydrate_cache);
        exists.current_dir(repo).args([
            "cat-file",
            "-e",
            &format!("{deleted_commit}:{}", versions[0].rel_path),
        ]);
        let output = exists.output()?;
        assert!(
            !output.status.success(),
            "delete commit should remove {}",
            versions[0].rel_path
        );
        Ok(())
    }

    fn assert_final_tree_before_hydrate(repo: &Path, stable: &VersionedObject) -> TestResult {
        let stable_bytes = fs::read(repo.join(stable.rel_path))?;
        let stable_pointer = Pointer::parse(&stable_bytes)?;
        assert_eq!(stable_pointer.size, stable.body.len() as u64);
        assert_eq!(
            stable_pointer.file_hash,
            *blake3::hash(stable.body).as_bytes()
        );
        assert!(
            !repo.join("crab/large-files/model.bin").exists(),
            "deleted key should be absent before hydrate too"
        );
        Ok(())
    }

    fn assert_git_clean(workspace: &Workspace, repo: &Path, label: &str) -> TestResult {
        let mut status = workspace.git(&workspace.hydrate_cache);
        status.current_dir(repo).args(["status", "--porcelain=v1"]);
        let output = run(status, &format!("git status {label}"))?;
        assert!(
            output.stdout.is_empty(),
            "git status dirty {label}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        Ok(())
    }

    fn versioned_bucket() -> TestResult<String> {
        env::var(BUCKET_ENV)
            .or_else(|_| env::var(FLAT_BUCKET_ENV))
            .map_err(|_| format!("{BUCKET_ENV} or {FLAT_BUCKET_ENV} must be set").into())
    }

    fn aws_region() -> String {
        env::var("AWS_REGION")
            .or_else(|_| env::var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|_| "us-east-1".to_owned())
    }

    fn s3_endpoint() -> Option<String> {
        env_value(ENDPOINT_ENV)
            .or_else(|| env_value("AWS_ENDPOINT_URL"))
            .or_else(|| env_value("AWS_ENDPOINT"))
            .or_else(|| env_value("ENDPOINT_URL"))
            .or_else(|| env_value("ENDPOINT"))
    }

    fn env_value(key: &str) -> Option<String> {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn env_bool(key: &str) -> Option<bool> {
        let value = env_value(key)?.to_ascii_lowercase();
        match value.as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        }
    }

    fn normalize_prefix(prefix: &str) -> String {
        prefix.trim_matches('/').to_owned()
    }

    fn join_key(prefix: &str, suffix: &str) -> String {
        let prefix = normalize_prefix(prefix);
        let suffix = suffix.trim_matches('/');
        if prefix.is_empty() {
            suffix.to_owned()
        } else if suffix.is_empty() {
            prefix
        } else {
            format!("{prefix}/{suffix}")
        }
    }

    fn run_id(label: &str) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before UNIX_EPOCH")
            .as_millis();
        format!("{label}-{millis}-{}", std::process::id())
    }

    fn run(mut command: Command, label: &str) -> TestResult<Output> {
        let output = command.output()?;
        if !output.status.success() {
            panic!(
                "{label} failed with status {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(output)
    }
}

#[cfg(not(feature = "tier-s3"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_import_versioned_bucket_multiple_commits() {
    eprintln!("skipping e2e_import_versioned_bucket_multiple_commits: tier-s3 disabled");
}
