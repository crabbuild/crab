//! Read one published fixture blob through the normal SDK snapshot API.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
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
    clone_tools: Option<CloneTools>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloneTools {
    git: PathBuf,
    crab: PathBuf,
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
        let mut report = timing.finish()?;
        tracing::debug!(target: "crab_cdn_probe::arm_boundary", parent: None, phase = "primary_finish");
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
        let local = repository.local()?;
        let oid = local.snapshot("HEAD").await?.commit_id();
        if oid != ObjectId::from_hex(&self.expected_oid)? {
            return Err("SDK clone checked out a different commit".into());
        }
        let mut bytes = Vec::new();
        std::fs::File::open(destination.join(PAYLOAD))?
            .take(self.expected_bytes as u64 + 1)
            .read_to_end(&mut bytes)?;
        let (payload, blob) = self.verified_payload(&bytes)?;
        let mut report = timing.finish()?;
        tracing::debug!(target: "crab_cdn_probe::arm_boundary", parent: None, phase = "primary_finish");
        report["commit"] = serde_json::json!(oid.to_string());
        report["payload"] = payload;
        report["warm_local"] = serde_json::Value::Null;
        report["clone_packs"] =
            serde_json::json!(installed_blob_packs(local.common_directory(), blob)?);
        Ok(report)
    }
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
        return Err("SDK clone did not install a Git pack containing the fixture blob".into());
    }
    Ok(packs)
}

struct Timing {
    started_at: u64,
    clock: Instant,
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
    let builder = if let Some(tools) = &request.clone_tools {
        builder.local(crab_sdk::local::Options::new(crab_sdk::local::Tools::new(
            &tools.git,
            &tools.crab,
        )?))
    } else {
        builder
    };
    let client = builder.build()?;
    let read = if request.clone_tools.is_some() {
        request.clone_read(&client).await
    } else {
        request.read(&client).await
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
            eprintln!("SDK CDN probe could not verify the published snapshot");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
