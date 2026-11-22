//! Verify fixture content through the ordinary SDK read and worktree APIs.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crab_sdk::remote::Snapshot;
use crab_sdk::storage::{CdnIssuerOptions, DirectCdnOptions, DirectStoreOptions};
use crab_sdk::{Client, GitPath, ObjectId, OpenOptions, RepositoryLocator, Revision};
use crab_storage::cdn::CdnPublicationFile;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

type Failure = Box<dyn std::error::Error + Send + Sync>;

const PAYLOAD: &str = "cdn-qualification.bin";
const MAX_REQUEST_BYTES: u64 = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    origin: String,
    repository_prefix: String,
    base_url: String,
    branch: String,
    expected_oid: String,
    expected_sha256: String,
    expected_bytes: usize,
    binding: Binding,
    repeat_snapshot: bool,
    public_transition: bool,
    #[serde(default)]
    worktree: Option<Worktree>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Worktree {
    git: PathBuf,
    crab: PathBuf,
    fetch_remote: Option<String>,
    hydration: Option<Hydration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Hydration {
    remote: String,
    pointer: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "access", deny_unknown_fields)]
enum Binding {
    Public {
        export_origin: String,
        publication: Option<CdnPublicationFile>,
    },
    Private {
        endpoint: String,
        oidc_issuer: String,
        oidc_client_id: String,
        token_cache_directory: PathBuf,
        approval_sha256: Option<String>,
    },
}

impl Request {
    fn store(&self) -> Result<DirectStoreOptions, Failure> {
        if let Some(bucket) = self.origin.strip_prefix("s3://") {
            return Ok(DirectStoreOptions::s3_from_env(bucket)?);
        }
        if let Some(bucket) = self.origin.strip_prefix("gs://") {
            return Ok(DirectStoreOptions::gcs_from_env(bucket)?);
        }
        let (account, container) = self
            .origin
            .strip_prefix("https://")
            .and_then(|value| value.split_once(".blob.core.windows.net/"))
            .ok_or("SDK qualification needs a canonical cloud origin")?;
        Ok(DirectStoreOptions::azure_from_env(account, container)?)
    }

    fn cdn(&self) -> crab_sdk::Result<DirectCdnOptions> {
        match &self.binding {
            Binding::Public {
                export_origin,
                publication,
            } => DirectCdnOptions::public_export(
                &self.origin,
                &self.repository_prefix,
                export_origin,
                &self.base_url,
                publication.clone(),
            ),
            Binding::Private {
                endpoint,
                oidc_issuer,
                oidc_client_id,
                token_cache_directory,
                approval_sha256,
            } => DirectCdnOptions::private_export(
                &self.origin,
                &self.repository_prefix,
                &self.base_url,
                CdnIssuerOptions::new(
                    endpoint,
                    oidc_issuer,
                    oidc_client_id,
                    token_cache_directory,
                )?,
                approval_sha256.as_deref(),
            ),
        }
    }

    async fn read(&self, client: &Client) -> Result<serde_json::Value, Failure> {
        let timing = Timing::start()?;
        let locator = RepositoryLocator::new(&self.repository_prefix)?;
        let expected_oid = ObjectId::from_hex(&self.expected_oid)?;
        let repository = client.open(OpenOptions::remote(locator)).await?;
        let snapshot = repository
            .remote()?
            .snapshot(Revision::branch(&self.branch)?)
            .await?;
        let oid = snapshot.commit_id()?;
        if oid != expected_oid {
            return Err("SDK snapshot selected a different published commit".into());
        }
        let payload = self.read_blob(&snapshot).await?;
        let mut report = primary_finish(timing)?;
        report["commit"] = serde_json::json!(oid.to_string());
        report["payload"] = payload.clone();
        report["warm_local"] = serde_json::Value::Null;
        if self.repeat_snapshot {
            tracing::debug!(target: "crab_cdn_probe::arm_boundary", parent: None, phase = "local_start");
            let timing = Timing::start()?;
            if self.read_blob(&snapshot).await? != payload {
                return Err("SDK cache returned a different fixture blob".into());
            }
            report["warm_local"] = timing.finish()?;
            tracing::debug!(target: "crab_cdn_probe::arm_boundary", parent: None, phase = "local_finish");
        }
        Ok(report)
    }

    async fn read_blob(&self, snapshot: &Snapshot) -> Result<serde_json::Value, Failure> {
        let bytes = snapshot.read_blob(GitPath::new(PAYLOAD)?).await?;
        self.verified_payload(&bytes).map(|(payload, _)| payload)
    }

    fn verified_payload(
        &self,
        bytes: &[u8],
    ) -> Result<(serde_json::Value, gix_hash::ObjectId), Failure> {
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        if bytes.len() != self.expected_bytes || sha256 != self.expected_sha256 {
            return Err("SDK read returned different published bytes".into());
        }
        let mut git = gix_hash::hasher(gix_hash::Kind::Sha1);
        git.update(format!("blob {}\0", bytes.len()).as_bytes());
        git.update(bytes);
        let blob = git.try_finalize()?;
        let payload = serde_json::json!({
            "sha256": sha256,
            "bytes": bytes.len(),
            "git_blob_oid_sha256": format!("{:x}", Sha256::digest(blob.as_bytes())),
        });
        Ok((payload, blob))
    }

    async fn clone_read(&self, client: &Client) -> Result<serde_json::Value, Failure> {
        use crab_sdk::local::CloneOptions;

        let scratch = tempfile::tempdir()?;
        let destination = scratch.path().join("clone");
        let timing = Timing::start()?;
        let repository = client
            .clone_local(
                RepositoryLocator::new(&self.repository_prefix)?,
                destination.clone(),
                CloneOptions::default().with_branch(&self.branch)?,
            )
            .await?;
        let report = primary_finish(timing)?;
        self.worktree_report(repository.local()?, report).await
    }

    async fn fetch_read(
        &self,
        client: &Client,
        git: &Path,
        remote: &str,
    ) -> Result<serde_json::Value, Failure> {
        use crab_sdk::local::{CheckoutOptions, FetchOptions};

        let scratch = tempfile::tempdir()?;
        initialize_worktree(git, scratch.path(), remote, &[])?;
        let repository = client.open(OpenOptions::local(scratch.path())).await?;
        let local = repository.local()?;
        let previous_head = local.snapshot("HEAD").await?.commit_id();
        let target = ObjectId::from_hex(&self.expected_oid)?;
        let tracking_ref = format!("refs/remotes/origin/{}", self.branch);
        if previous_head == target {
            return Err("SDK fetch fixture already has the published commit at HEAD".into());
        }
        let timing = Timing::start()?;
        let outcome = local.fetch(FetchOptions::default()).await?;
        let report = primary_finish(timing)?;
        if outcome.head() != Some(previous_head)
            || local.snapshot("HEAD").await?.commit_id() != previous_head
            || local.snapshot(&tracking_ref).await?.commit_id() != target
            || local.path().join(PAYLOAD).try_exists()?
        {
            return Err("SDK fetch did not publish the remote-tracking ref while preserving HEAD and the worktree".into());
        }
        local
            .checkout(&tracking_ref, CheckoutOptions::default())
            .await?;
        let mut report = self.worktree_report(local, report).await?;
        report["previous_head"] = serde_json::json!(previous_head.to_string());
        report["head_after_fetch"] = serde_json::json!(outcome.head().map(|oid| oid.to_string()));
        Ok(report)
    }

    async fn hydrate(
        &self,
        client: &Client,
        git: &Path,
        hydration: &Hydration,
    ) -> Result<serde_json::Value, Failure> {
        let pointer = crab_types::pointer::Pointer::parse(hydration.pointer.as_bytes())?;
        if pointer.size != self.expected_bytes as u64 {
            return Err("SDK hydration fixture size differs from the pointer".into());
        }
        let scratch = tempfile::tempdir()?;
        let config = format!(
            "[remote]\nurl = {}\n",
            serde_json::to_string(&hydration.remote)?
        );
        initialize_worktree(
            git,
            scratch.path(),
            &hydration.remote,
            &[
                ("crab.toml", config.as_bytes()),
                (
                    ".gitattributes",
                    b"cdn-qualification.bin filter=crab -text\n",
                ),
                (PAYLOAD, hydration.pointer.as_bytes()),
            ],
        )?;
        client.configure_local(scratch.path()).await?;
        let repository = client.open(OpenOptions::local(scratch.path())).await?;
        let timing = Timing::start()?;
        repository
            .local()?
            .hydrate(vec![PathBuf::from(PAYLOAD)])
            .await?;
        let mut report = primary_finish(timing)?;
        let mut file = std::fs::File::open(scratch.path().join(PAYLOAD))?
            .take(pointer.size.checked_add(1).ok_or("invalid fixture size")?);
        let mut digest = Sha256::new();
        let size = std::io::copy(&mut file, &mut digest)?;
        let sha256 = format!("{:x}", digest.finalize());
        if size != pointer.size || sha256 != self.expected_sha256 {
            return Err("SDK hydration wrote different fixture bytes".into());
        }
        report["payload"] = serde_json::json!({"sha256": sha256, "bytes": size});
        Ok(report)
    }

    async fn worktree_report(
        &self,
        local: crab_sdk::local::Local<'_>,
        mut report: serde_json::Value,
    ) -> Result<serde_json::Value, Failure> {
        let oid = local.snapshot("HEAD").await?.commit_id();
        if oid != ObjectId::from_hex(&self.expected_oid)? {
            return Err("SDK checked out a different commit".into());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(local.path().join(PAYLOAD))?
            .take(self.expected_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        let (payload, blob) = self.verified_payload(&bytes)?;
        report["commit"] = serde_json::json!(oid.to_string());
        report["payload"] = payload;
        report["warm_local"] = serde_json::Value::Null;
        report["installed_packs"] =
            serde_json::json!(installed_blob_packs(local.common_directory(), blob)?);
        Ok(report)
    }
}

fn initialize_worktree(
    git: &Path,
    root: &Path,
    remote: &str,
    files: &[(&str, &[u8])],
) -> Result<(), Failure> {
    // Seed the local fixture without borrowing remote objects, refs, or Git configuration.
    let global = root.join("empty.gitconfig");
    std::fs::File::create(&global)?;
    let run = |args: &[&str]| -> Result<(), Failure> {
        let mut command = Command::new(git);
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(key);
            }
        }
        let output = command
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err("cannot initialize the isolated SDK worktree".into());
        }
        Ok(())
    };
    run(&[
        "init",
        "--quiet",
        "--template=",
        "--object-format=sha1",
        "--initial-branch=local",
    ])?;
    for (path, content) in files {
        std::fs::write(root.join(path), content)?;
        run(&["add", "--", path])?;
    }
    run(&[
        "-c",
        "user.name=Crab CDN qualification",
        "-c",
        "user.email=qualification@example.invalid",
        "commit",
        "--allow-empty",
        "--no-gpg-sign",
        "-m",
        "Local SDK fetch baseline",
    ])?;
    run(&["remote", "add", "origin", remote])?;
    std::fs::remove_file(global)?;
    Ok(())
}

fn installed_blob_packs(
    git_dir: &std::path::Path,
    blob: gix_hash::ObjectId,
) -> Result<std::collections::BTreeSet<String>, Failure> {
    let mut packs = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(git_dir.join("objects").join("pack"))? {
        let index = entry?.path();
        if index.extension().and_then(|ext| ext.to_str()) != Some("idx") {
            continue;
        }
        let (oids, _) =
            crab_git::pack_locator::sorted_object_ids_from_index_bytes(&std::fs::read(&index)?)?;
        if oids.binary_search(&blob).is_ok() {
            let pack = std::fs::File::open(index.with_extension("pack"))?;
            let digest = blake3::Hasher::new().update_reader(pack)?.finalize();
            packs.insert(digest.to_hex().to_string());
        }
    }
    if packs.is_empty() {
        return Err("SDK did not install a Git pack containing the fixture blob".into());
    }
    Ok(packs)
}

struct Timing {
    started_at: u64,
    clock: Instant,
}

fn primary_finish(timing: Timing) -> Result<serde_json::Value, Failure> {
    let report = timing.finish()?;
    tracing::debug!(target: "crab_cdn_probe::arm_boundary", parent: None, phase = "primary_finish");
    Ok(report)
}

impl Timing {
    fn start() -> Result<Self, Failure> {
        Ok(Self {
            started_at: unix_ms()?,
            clock: Instant::now(),
        })
    }

    fn finish(self) -> Result<serde_json::Value, Failure> {
        let elapsed = u64::try_from(self.clock.elapsed().as_millis())?;
        Ok(serde_json::json!({
            "started_at_unix_ms": self.started_at,
            "finished_at_unix_ms": unix_ms()?,
            "elapsed_ms": elapsed.max(1),
        }))
    }
}

fn unix_ms() -> Result<u64, Failure> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

fn build_info() -> Result<serde_json::Value, Failure> {
    let source = option_env!("CRAB_CDN_PROBE_SOURCE_SHA")
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("SDK probe requires its source commit at build time")?;
    Ok(serde_json::json!({
        "source_sha": source,
        "target_triple": env!("CRAB_SDK_BUILD_TARGET"),
        "cargo_profile": env!("CRAB_SDK_BUILD_PROFILE"),
        "cargo_features": env!("CRAB_SDK_BUILD_FEATURES"),
        "qualification_feature": crab_storage::cdn::qualification_enabled(),
        "release_policy_sha256": crab_storage::cdn::release_policy_sha256(),
    }))
}

async fn run() -> Result<(), Failure> {
    let build = build_info()?;
    let mut args = std::env::args_os().skip(1);
    if let Some(argument) = args.next() {
        if argument == "--build-info" && args.next().is_none() {
            writeln!(std::io::stdout(), "{build}")?;
            return Ok(());
        }
        return Err("SDK probe only accepts --build-info".into());
    }
    let mut body = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut body)?;
    if body.len() as u64 > MAX_REQUEST_BYTES {
        return Err("SDK probe request exceeds its limit".into());
    }
    let request: Request = serde_json::from_slice(&body)?;
    let mut filter = String::from(
        "off,crab_cdn_probe::arm_boundary=debug,crab_sdk::cdn::read_target=debug,crab_storage::cdn::transfer=debug,crab_storage::native_read=debug,crab_storage::cdn::gcs_viewer_log=trace",
    );
    if request.public_transition {
        filter.push_str(",crab_storage::cdn::public_transition=debug");
    }
    if request
        .worktree
        .as_ref()
        .is_some_and(|worktree| worktree.hydration.is_some())
    {
        filter.push_str(",crab_sdk::cdn::child=debug");
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .json()
        .with_current_span(false)
        .with_span_list(false)
        .try_init()?;
    let builder = Client::builder()
        .direct_store(request.store()?)
        .direct_cdn(request.cdn()?);
    let builder = if let Some(tools) = &request.worktree {
        builder.local(crab_sdk::local::Options::new(crab_sdk::local::Tools::new(
            &tools.git,
            &tools.crab,
        )?))
    } else {
        builder
    };
    let client = builder.build()?;
    let read = match &request.worktree {
        Some(worktree) => match (&worktree.fetch_remote, &worktree.hydration) {
            (Some(remote), None) => request.fetch_read(&client, &worktree.git, remote).await,
            (None, Some(hydration)) => request.hydrate(&client, &worktree.git, hydration).await,
            (None, None) => request.clone_read(&client).await,
            _ => Err("SDK probe requires one worktree action".into()),
        },
        None => request.read(&client).await,
    };
    let close = client.close().await;
    let mut report = read?;
    close?;
    report["build"] = build;
    writeln!(std::io::stdout(), "{report}")?;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("SDK CDN probe could not verify the fixture content");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn hydration_fixture_reads_the_file_written_by_the_child() {
        use std::os::unix::fs::PermissionsExt as _;
        let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
            .unwrap()
            .canonicalize()
            .unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let executable = scratch.path().join("crab");
        let content = b"hydrated fixture bytes";
        std::fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
set -eu
case "$1" in
    sdk-capabilities) printf '%s\n' '{}' ;;
    hydrate)
        test "$3" = cdn-qualification.bin
        "$CRAB_INTERNAL_GIT_EXECUTABLE" ls-files --error-unmatch "$3" >/dev/null
        grep -q '^file-hash ' "$3"
        printf %s 'hydrated fixture bytes' > "$3"
        ;;
    *) exit 64 ;;
esac
"#,
                crab_remote::local::CURRENT_CAPABILITIES_JSON
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let client = Client::builder()
            .direct_store(DirectStoreOptions::filesystem(scratch.path()).unwrap())
            .local(crab_sdk::local::Options::new(
                crab_sdk::local::Tools::new(&git, executable).unwrap(),
            ))
            .build()
            .unwrap();
        let request: Request = serde_json::from_value(serde_json::json!({
            "origin": "s3://fixture-bucket", "repository_prefix": "fixture", "base_url": "https://edge.example.test",
            "branch": "", "expected_oid": "", "expected_sha256": format!("{:x}", Sha256::digest(content)),
            "expected_bytes": content.len(), "repeat_snapshot": false, "public_transition": false,
            "binding": {"access": "public", "export_origin": "s3://export-bucket"},
        })).unwrap();
        let result = request
            .hydrate(
                &client,
                &git,
                &Hydration {
                    remote: "crab-sdk:fixture".to_owned(),
                    pointer: format!(
                        "version https://crab.build/spec/v1\nfile-hash {}\nsize {}\n",
                        "a".repeat(64),
                        content.len()
                    ),
                },
            )
            .await
            .unwrap();
        assert_eq!(result["payload"]["sha256"], request.expected_sha256);
        client.close().await.unwrap();
    }

    #[test]
    fn fetch_arms_start_with_identical_local_heads_and_no_tracking_ref() {
        let remote = "crab://qualification-bucket/repository";
        let mut heads = Vec::new();
        for _ in 0..2 {
            let worktree = tempfile::tempdir().unwrap();
            initialize_worktree(Path::new("git"), worktree.path(), remote, &[]).unwrap();
            let git = |args: &[&str]| {
                let result = Command::new("git")
                    .current_dir(worktree.path())
                    .args(args)
                    .output()
                    .unwrap();
                assert!(result.status.success(), "{args:?}");
                String::from_utf8(result.stdout).unwrap().trim().to_owned()
            };
            heads.push(git(&["rev-parse", "HEAD"]));
            assert_eq!(
                git(&["config", "--local", "--get", "remote.origin.url"]),
                remote
            );
            assert!(git(&["for-each-ref", "refs/remotes"]).is_empty());
            assert!(git(&["status", "--porcelain"]).is_empty());
        }
        assert_eq!(heads[0], heads[1]);
    }

    #[test]
    fn installed_pack_lookup_reads_real_git_indexes_and_hashes_the_pack() {
        let worktree = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let result = std::process::Command::new("git")
                .current_dir(worktree.path())
                .args(args)
                .output()
                .unwrap();
            assert!(result.status.success(), "{args:?}");
            String::from_utf8(result.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "--quiet", "--object-format=sha1"]);
        std::fs::write(worktree.path().join(PAYLOAD), b"checked out bytes").unwrap();
        git(&["add", "--", PAYLOAD]);
        git(&[
            "-c",
            "user.name=SDK test",
            "-c",
            "user.email=sdk@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "inspect installed pack",
        ]);
        git(&["repack", "-a", "-d"]);
        let git_dir = worktree.path().join(".git");
        let oid = gix_hash::ObjectId::from_hex(git(&["hash-object", PAYLOAD]).as_bytes()).unwrap();
        let packs = installed_blob_packs(&git_dir, oid).unwrap();
        let pack = std::fs::read_dir(git_dir.join("objects/pack"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "pack")
            })
            .unwrap();
        let digest = blake3::hash(&std::fs::read(pack).unwrap())
            .to_hex()
            .to_string();
        assert_eq!(packs, [digest].into_iter().collect());
        assert!(installed_blob_packs(&git_dir, gix_hash::Kind::Sha1.null()).is_err());
    }
}
