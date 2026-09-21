use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    Digest, QualificationMatrixManifest, QualificationMetric, QualificationOwnership,
    QualificationProfile, QualificationReceipt, QualificationRunArtifact, QualificationRunner,
    QualificationWorkload, validate_cluster_receipt,
};
use ed25519_dalek::SigningKey;
use rand::Rng;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("qualification receipt: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("emit") => {
            let output = required(&mut args, "output")?;
            let source = required(&mut args, "source revision")?;
            let image = parse_digest(&required(&mut args, "image digest")?)?;
            let artifact = required(&mut args, "artifact")?;
            let provider = args.next().unwrap_or_else(|| "github-actions".into());
            let workload = args.next().unwrap_or_else(|| "cell-runtime-release".into());
            let fault = args.next().unwrap_or_else(|| "none".into());
            let profile = match args.next() {
                Some(path) => QualificationProfile::decode(
                    &fs::read(path).map_err(|error| format!("read profile: {error}"))?,
                )
                .map_err(|error| error.to_string())?,
                None => QualificationProfile::pr_contract(),
            };
            if args.next().is_some() {
                return Err(usage());
            }
            let artifact =
                fs::read(&artifact).map_err(|error| format!("read artifact: {error}"))?;
            let workload_seed = if workload == "primitives" {
                primitive_workload_seed(&artifact, &profile)?
            } else {
                0
            };
            let now = unix_millis()?;
            let mut key_bytes = [0_u8; 32];
            rand::rng().fill(&mut key_bytes);
            let mut metrics = vec![
                QualificationMetric::new(
                    "artifact_bytes".into(),
                    artifact.len() as u64,
                    "bytes".into(),
                )
                .map_err(|error| error.to_string())?,
            ];
            if profile == QualificationProfile::pr_contract() {
                for (name, value, unit) in [
                    ("cells", profile.minimum_cells(), "cells"),
                    ("operations", profile.minimum_operations(), "operations"),
                    ("duration_secs", profile.minimum_duration_secs(), "seconds"),
                    ("p99_latency_ms", 1, "ms"),
                ] {
                    metrics.push(
                        QualificationMetric::new(name.into(), value, unit.into())
                            .map_err(|error| error.to_string())?,
                    );
                }
            }
            let receipt = QualificationRunner::new(SigningKey::from_bytes(&key_bytes))
                .emit_with_profile_and_evidence(
                    &profile,
                    source,
                    image,
                    provider,
                    workload,
                    fault.clone(),
                    metrics,
                    &artifact,
                    true,
                    (
                        "rustc".into(),
                        "release".into(),
                        "published-image".into(),
                        workload_seed,
                        0,
                        0,
                        false,
                    ),
                    now,
                    now,
                    fault.as_bytes(),
                    vec![Digest::from_bytes(*blake3::hash(&artifact).as_bytes())],
                    Vec::<QualificationOwnership>::new(),
                )
                .map_err(|error| error.to_string())?;
            let encoded = receipt.encode().map_err(|error| error.to_string())?;
            fs::write(output, encoded).map_err(|error| format!("write receipt: {error}"))?;
            Ok(())
        }
        Some("profile") => {
            let output = required(&mut args, "output")?;
            let tier = args.next().unwrap_or_else(|| "pr-contract".into());
            if args.next().is_some() {
                return Err(usage());
            }
            let profile = match tier.as_str() {
                "pr-contract" => QualificationProfile::pr_contract(),
                "local-provider" => QualificationProfile::local_provider(),
                "scale" => QualificationProfile::scale(),
                "fault" => QualificationProfile::fault(),
                "fault-s3" => QualificationProfile::fault_s3(),
                "fault-gcs" => QualificationProfile::fault_gcs(),
                "fault-azure" => QualificationProfile::fault_azure(),
                "provider" => QualificationProfile::provider(),
                "provider-s3" => QualificationProfile::provider_s3(),
                "provider-gcs" => QualificationProfile::provider_gcs(),
                "provider-azure" => QualificationProfile::provider_azure(),
                "compatibility" => QualificationProfile::compatibility(),
                _ => {
                    return Err(
                        "profile must be pr-contract, local-provider, scale, fault, fault-s3, fault-gcs, fault-azure, provider, provider-s3, provider-gcs, provider-azure, or compatibility".into(),
                    );
                }
            };
            fs::write(output, profile.encode().map_err(|error| error.to_string())?)
                .map_err(|error| format!("write profile: {error}"))?;
            Ok(())
        }
        Some("workload") => {
            let output = required(&mut args, "output")?;
            let profile_path = required(&mut args, "profile")?;
            let seed = parse_u64(&required(&mut args, "seed")?, "seed")?;
            let profile = QualificationProfile::decode(
                &fs::read(profile_path).map_err(|error| format!("read profile: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            let workload = match (args.next(), args.next(), args.next()) {
                (None, None, None) => QualificationWorkload::generate(&profile, seed),
                (Some(cells), Some(operations), Some(duration_secs)) => {
                    QualificationWorkload::generate_with_size(
                        &profile,
                        seed,
                        parse_u64(&cells, "cells")?,
                        parse_u64(&operations, "operations")?,
                        parse_u64(&duration_secs, "duration_secs")?,
                    )
                }
                _ => return Err(usage()),
            }
            .map_err(|error| error.to_string())?;
            fs::write(
                output,
                workload.encode().map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("write workload: {error}"))?;
            Ok(())
        }
        Some("verify-workload") => {
            let workload_path = required(&mut args, "workload")?;
            let profile_path = required(&mut args, "profile")?;
            if args.next().is_some() {
                return Err(usage());
            }
            let workload = QualificationWorkload::decode(
                &fs::read(workload_path).map_err(|error| format!("read workload: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            let profile = QualificationProfile::decode(
                &fs::read(profile_path).map_err(|error| format!("read profile: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            workload
                .verify_for_profile(&profile)
                .map_err(|error| error.to_string())
        }
        Some("verify") => {
            let receipt_path = required(&mut args, "receipt")?;
            let source = required(&mut args, "source revision")?;
            let image = parse_digest(&required(&mut args, "image digest")?)?;
            let artifact = required(&mut args, "artifact")?;
            let profile = match args.next() {
                Some(path) => Some(
                    QualificationProfile::decode(
                        &fs::read(path).map_err(|error| format!("read profile: {error}"))?,
                    )
                    .map_err(|error| error.to_string())?,
                ),
                None => None,
            };
            let trusted_signer = args.next().map(|value| parse_signer(&value)).transpose()?;
            if args.next().is_some() {
                return Err(usage());
            }
            let requires_freshness = profile
                .as_ref()
                .is_some_and(QualificationProfile::requires_protected_evidence);
            require_trusted_signer(profile.as_ref(), trusted_signer.as_ref())?;
            let receipt = QualificationReceipt::decode(
                &fs::read(receipt_path).map_err(|error| format!("read receipt: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            if !receipt.passed() {
                return Err("qualification receipt is not passed".into());
            }
            let artifact = fs::read(artifact).map_err(|error| format!("read artifact: {error}"))?;
            let result = match profile {
                Some(profile) => match trusted_signer {
                    Some(trusted_signer) => receipt
                        .verify_for_profile_with_signer(
                            &source,
                            image,
                            &profile,
                            &[&artifact],
                            trusted_signer,
                        )
                        .map_err(|error| error.to_string()),
                    None => receipt
                        .verify_for_profile(&source, image, &profile, &[&artifact])
                        .map_err(|error| error.to_string()),
                },
                None => match trusted_signer {
                    Some(trusted_signer) => receipt
                        .verify_for_trusted_signer(&source, image, &artifact, trusted_signer)
                        .map_err(|error| error.to_string()),
                    None => receipt
                        .verify_for(&source, image, &artifact)
                        .map_err(|error| error.to_string()),
                },
            };
            result?;
            if requires_freshness {
                receipt
                    .verify_fresh_at(unix_millis()?)
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        Some("verify-matrix") => verify_matrix(&mut args),
        Some("validate-cluster") => validate_cluster(&mut args),
        _ => Err(usage()),
    }
}

fn primitive_workload_seed(artifact: &[u8], profile: &QualificationProfile) -> Result<u64, String> {
    if let Ok(workload) = QualificationWorkload::decode(artifact) {
        workload
            .verify_for_profile(profile)
            .map_err(|error| format!("verify primitive workload: {error}"))?;
        return Ok(workload.seed());
    }
    let run = QualificationRunArtifact::decode(artifact)
        .map_err(|error| format!("decode primitive workload or run artifact: {error}"))?;
    run.verify_for_profile(profile)
        .map_err(|error| format!("verify primitive run artifact: {error}"))?;
    Ok(run.workload().seed())
}

fn validate_cluster(args: &mut impl Iterator<Item = String>) -> Result<(), String> {
    let receipt_path = required(args, "cluster receipt")?;
    let source = required(args, "source revision")?;
    let image = parse_digest(&required(args, "image digest")?)?;
    let mode = args.next().unwrap_or_else(|| "source-only".into());
    if args.next().is_some() || !matches!(mode.as_str(), "release" | "source-only") {
        return Err(usage());
    }
    let receipt =
        fs::read(receipt_path).map_err(|error| format!("read cluster receipt: {error}"))?;
    validate_cluster_receipt(&receipt, &source, image, mode == "release")
        .map_err(|error| error.to_string())
}

fn verify_matrix(args: &mut impl Iterator<Item = String>) -> Result<(), String> {
    let manifest_path = PathBuf::from(required(args, "matrix manifest")?);
    let source = required(args, "source revision")?;
    let image = parse_digest(&required(args, "image digest")?)?;
    let profile = match args.next() {
        Some(path) => Some(
            QualificationProfile::decode(
                &fs::read(path).map_err(|error| format!("read profile: {error}"))?,
            )
            .map_err(|error| error.to_string())?,
        ),
        None => None,
    };
    let trusted_signer = args.next().map(|value| parse_signer(&value)).transpose()?;
    if args.next().is_some() {
        return Err(usage());
    }
    require_trusted_signer(profile.as_ref(), trusted_signer.as_ref())?;
    let manifest = QualificationMatrixManifest::decode(
        &fs::read(&manifest_path).map_err(|error| format!("read matrix manifest: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    let base = manifest_base(&manifest_path);
    let mut receipts = Vec::with_capacity(manifest.entries().len());
    let mut artifacts = Vec::with_capacity(manifest.entries().len());
    for entry in manifest.entries() {
        let receipt_path = resolve_manifest_path(base, entry.receipt())?;
        receipts.push(
            QualificationReceipt::decode(
                &fs::read(receipt_path).map_err(|error| format!("read matrix receipt: {error}"))?,
            )
            .map_err(|error| error.to_string())?,
        );
        let mut row = Vec::with_capacity(entry.artifacts().len());
        for artifact in entry.artifacts() {
            let artifact_path = resolve_manifest_path(base, artifact)?;
            row.push(
                fs::read(artifact_path)
                    .map_err(|error| format!("read matrix artifact: {error}"))?,
            );
        }
        artifacts.push(row);
    }
    let artifact_views = artifacts
        .iter()
        .map(|row| row.iter().map(Vec::as_slice).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    let evidence = manifest
        .entries()
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            (
                entry.workload(),
                &receipts[index],
                artifact_views[index].as_slice(),
            )
        })
        .collect::<Vec<_>>();
    match (profile, trusted_signer) {
        (Some(profile), Some(trusted_signer)) => {
            if profile.requires_protected_evidence() {
                QualificationReceipt::verify_matrix_for_profile_with_signer_fresh_at(
                    &source,
                    image,
                    &profile,
                    &evidence,
                    trusted_signer,
                    unix_millis()?,
                )
                .map_err(|error| error.to_string())?;
            } else {
                QualificationReceipt::verify_matrix_for_profile_with_signer(
                    &source,
                    image,
                    &profile,
                    &evidence,
                    trusted_signer,
                )
                .map_err(|error| error.to_string())?;
            }
        }
        (Some(profile), None) => {
            QualificationReceipt::verify_matrix_for_profile(&source, image, &profile, &evidence)
                .map_err(|error| error.to_string())?;
        }
        (None, Some(_)) => return Err("a trusted signer requires a profile".into()),
        (None, None) => {
            QualificationReceipt::verify_matrix(&source, image, &evidence)
                .map_err(|error| error.to_string())?;
        }
    }
    println!("qualification matrix verified");
    Ok(())
}

fn resolve_manifest_path(base: &Path, value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err("matrix paths must be relative and stay within the manifest directory".into());
    }
    let canonical_base = fs::canonicalize(base)
        .map_err(|error| format!("resolve matrix manifest directory: {error}"))?;
    let mut candidate = canonical_base.clone();
    for component in path.components() {
        candidate.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|error| format!("inspect matrix artifact path: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("matrix paths must not be symlinks".into());
        }
    }
    let resolved = fs::canonicalize(&candidate)
        .map_err(|error| format!("resolve matrix artifact path: {error}"))?;
    if !resolved.starts_with(&canonical_base) {
        return Err("matrix paths must stay within the manifest directory".into());
    }
    Ok(resolved)
}

fn require_trusted_signer(
    profile: Option<&QualificationProfile>,
    trusted_signer: Option<&[u8; 32]>,
) -> Result<(), String> {
    if profile.is_some_and(QualificationProfile::requires_protected_evidence)
        && trusted_signer.is_none()
    {
        return Err("protected qualification profiles require a trusted signer".into());
    }
    Ok(())
}

fn manifest_base(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn required(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("missing {name}\n{}", usage()))
}

fn parse_digest(value: &str) -> Result<Digest, String> {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("image digest must be 64 lowercase hexadecimal characters".into());
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex(pair[0])? << 4) | hex(pair[1])?;
    }
    Ok(Digest::from_bytes(bytes))
}

fn parse_u64(value: &str, name: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|_| format!("{name} must be an unsigned integer"))
}

fn parse_signer(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("trusted signer must be 64 lowercase hexadecimal characters".into());
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex(pair[0])? << 4) | hex(pair[1])?;
    }
    if bytes.iter().all(|byte| *byte == 0) {
        return Err("trusted signer must not be zero".into());
    }
    Ok(bytes)
}

fn hex(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err("image digest must use lowercase hexadecimal characters".into()),
    }
}

fn unix_millis() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock: {error}"))
        .and_then(|duration| {
            u64::try_from(duration.as_millis()).map_err(|_| "timestamp overflow".into())
        })
}

fn usage() -> String {
    "usage: qualification_receipt profile <output> [pr-contract|local-provider|scale|fault|fault-s3|fault-gcs|fault-azure|provider|provider-s3|provider-gcs|provider-azure|compatibility]\n       qualification_receipt workload <output> <profile.json> <seed> [cells operations duration_secs]\n       qualification_receipt verify-workload <workload.json> <profile.json>\n       qualification_receipt emit <output> <source> <image-digest> <artifact> [provider workload fault profile.json]\n       qualification_receipt verify <receipt> <source> <image-digest> <artifact> [profile.json [trusted-signer-hex]]\n       qualification_receipt verify-matrix <manifest> <source> <image-digest> [profile.json [trusted-signer-hex]]\n       qualification_receipt validate-cluster <receipt> <source> <image-digest> [release|source-only]".into()
}

#[cfg(test)]
mod tests {
    use super::{manifest_base, require_trusted_signer, resolve_manifest_path};
    use crab_cell_runtime::QualificationProfile;
    use std::{fs, path::Path};

    #[test]
    fn relative_manifest_uses_the_current_directory() {
        assert_eq!(
            manifest_base(Path::new("qualification-matrix.json")),
            Path::new(".")
        );
    }

    #[test]
    fn nested_manifest_uses_its_parent_directory() {
        assert_eq!(
            manifest_base(Path::new("evidence/qualification-matrix.json")),
            Path::new("evidence")
        );
    }

    #[test]
    fn protected_profiles_require_a_pinned_signer() {
        let protected = QualificationProfile::scale();
        assert!(require_trusted_signer(Some(&protected), None).is_err());
        assert!(require_trusted_signer(Some(&QualificationProfile::pr_contract()), None).is_ok());
        assert!(require_trusted_signer(None, None).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn matrix_paths_reject_symlinks() {
        let directory = tempfile::tempdir().expect("matrix directory");
        fs::write(directory.path().join("artifact.json"), b"artifact").expect("artifact");
        std::os::unix::fs::symlink(
            directory.path().join("artifact.json"),
            directory.path().join("linked.json"),
        )
        .expect("symlink");
        fs::create_dir(directory.path().join("nested")).expect("nested directory");
        std::os::unix::fs::symlink(
            directory.path().join("nested"),
            directory.path().join("linked-directory"),
        )
        .expect("directory symlink");
        fs::write(directory.path().join("nested/artifact.json"), b"artifact")
            .expect("nested artifact");
        assert!(resolve_manifest_path(directory.path(), "linked.json").is_err());
        assert!(resolve_manifest_path(directory.path(), "linked-directory/artifact.json").is_err());
        assert!(resolve_manifest_path(directory.path(), "artifact.json").is_ok());
    }
}
