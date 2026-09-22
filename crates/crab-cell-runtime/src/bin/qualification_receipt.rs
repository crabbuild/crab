use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    Digest, QUALIFICATION_MATRIX_ROWS, QualificationExecutionEvidence, QualificationMatrixEntry,
    QualificationMatrixManifest, QualificationMetric, QualificationOwnership, QualificationProfile,
    QualificationReceipt, QualificationRunArtifact, QualificationRunner, QualificationWorkload,
    validate_cluster_receipt,
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
            if profile.requires_protected_evidence() {
                return Err(
                    "emit cannot create protected evidence; use bind-protected with a measured run artifact"
                        .into(),
                );
            }
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
        Some("bind-protected") => bind_protected(&mut args),
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
        Some("manifest") => {
            let output = PathBuf::from(required(&mut args, "manifest output")?);
            let evidence_dir = PathBuf::from(required(&mut args, "evidence directory")?);
            if args.next().is_some() {
                return Err(usage());
            }
            require_manifest_output(&output, &evidence_dir)?;
            let manifest = build_manifest(&evidence_dir)?;
            fs::write(
                output,
                manifest.encode().map_err(|error| error.to_string())?,
            )
            .map_err(|error| format!("write matrix manifest: {error}"))?;
            Ok(())
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

fn bind_protected(args: &mut impl Iterator<Item = String>) -> Result<(), String> {
    let output = PathBuf::from(required(args, "output")?);
    let source = required(args, "source revision")?;
    let image = parse_digest(&required(args, "image digest")?)?;
    let profile = QualificationProfile::decode(&read_regular_file(
        Path::new(&required(args, "profile")?),
        "profile",
    )?)
    .map_err(|error| error.to_string())?;
    if !profile.requires_protected_evidence() {
        return Err("bind-protected requires a protected qualification profile".into());
    }
    let evidence = QualificationExecutionEvidence::decode(&read_regular_file(
        Path::new(&required(args, "execution evidence")?),
        "execution evidence",
    )?)
    .map_err(|error| error.to_string())?;
    let signing_key = read_signing_key(Path::new(&required(args, "signing key file")?))?;
    let run_bytes = read_regular_file(Path::new(&required(args, "run artifact")?), "run artifact")?;
    let run = QualificationRunArtifact::decode(&run_bytes)
        .map_err(|error| format!("decode run artifact: {error}"))?;
    let workload_bytes = read_regular_file(
        Path::new(&required(args, "workload artifact")?),
        "workload artifact",
    )?;
    let mut artifact_bytes = vec![run_bytes, workload_bytes];
    for path in args {
        artifact_bytes.push(read_regular_file(Path::new(&path), "raw artifact")?);
    }
    let artifact_refs = artifact_bytes.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let receipt = QualificationRunner::new(signing_key)
        .emit_protected_run(&profile, source, image, evidence, &run, &artifact_refs)
        .map_err(|error| error.to_string())?;
    write_regular_file(
        &output,
        &receipt.encode().map_err(|error| error.to_string())?,
        "protected receipt",
    )
}

fn read_regular_file(path: &Path, field: &str) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("{field}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{field} must be a regular, non-symlink file"));
    }
    fs::read(path).map_err(|error| format!("read {field}: {error}"))
}

fn write_regular_file(path: &Path, bytes: &[u8], field: &str) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(format!("{field} must be a regular, non-symlink file"));
    }
    fs::write(path, bytes).map_err(|error| format!("write {field}: {error}"))
}

fn read_signing_key(path: &Path) -> Result<SigningKey, String> {
    let bytes = read_regular_file(path, "signing key")?;
    let key_bytes = if bytes.len() == 32 {
        bytes
    } else {
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "signing key must be 32 raw bytes or 64 lowercase hex characters")?
            .trim();
        if text.len() != 64 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("signing key must be 32 raw bytes or 64 lowercase hex characters".into());
        }
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| Ok((hex(pair[0])? << 4) | hex(pair[1])?))
            .collect::<Result<Vec<_>, String>>()?
    };
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "signing key must contain exactly 32 bytes".to_owned())?;
    if key_bytes.iter().all(|byte| *byte == 0) {
        return Err("signing key must not be zero".into());
    }
    Ok(SigningKey::from_bytes(&key_bytes))
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

fn build_manifest(evidence_dir: &Path) -> Result<QualificationMatrixManifest, String> {
    require_directory(evidence_dir, "evidence directory")?;
    let receipts_dir = evidence_dir.join("receipts");
    let artifacts_dir = evidence_dir.join("artifacts");
    require_directory(&receipts_dir, "matrix receipts directory")?;
    require_directory(&artifacts_dir, "matrix artifacts directory")?;

    let entries = QUALIFICATION_MATRIX_ROWS
        .iter()
        .map(|workload| {
            let receipt_name = format!("{workload}.json");
            require_file(&receipts_dir.join(&receipt_name), "matrix receipt")?;
            let workload_dir = artifacts_dir.join(workload);
            require_directory(&workload_dir, "matrix workload artifact directory")?;
            let mut artifact_names = fs::read_dir(&workload_dir)
                .map_err(|error| format!("read matrix workload artifacts: {error}"))?
                .map(|entry| {
                    let entry = entry.map_err(|error| format!("read matrix artifact: {error}"))?;
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| "matrix artifact name is not UTF-8".to_owned())?;
                    validate_component(&name, "matrix artifact name")?;
                    require_file(&entry.path(), "matrix artifact")?;
                    Ok(name)
                })
                .collect::<Result<Vec<_>, String>>()?;
            artifact_names.sort_unstable();
            if artifact_names.is_empty() {
                return Err("matrix workload has no artifacts".into());
            }
            let artifacts = artifact_names
                .into_iter()
                .map(|name| format!("artifacts/{workload}/{name}"))
                .collect();
            QualificationMatrixEntry::new(
                (*workload).to_owned(),
                format!("receipts/{receipt_name}"),
                artifacts,
            )
            .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, String>>()?;
    QualificationMatrixManifest::new(entries).map_err(|error| error.to_string())
}

fn require_manifest_output(output: &Path, evidence_dir: &Path) -> Result<(), String> {
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let evidence_root = fs::canonicalize(evidence_dir)
        .map_err(|error| format!("canonicalize evidence directory: {error}"))?;
    let output_parent = fs::canonicalize(parent)
        .map_err(|error| format!("canonicalize matrix manifest parent: {error}"))?;
    if output_parent != evidence_root {
        return Err("matrix manifest output must be directly under the evidence directory".into());
    }
    if let Ok(metadata) = fs::symlink_metadata(output)
        && metadata.file_type().is_symlink()
    {
        return Err("matrix manifest output must not be a symlink".into());
    }
    Ok(())
}

fn require_directory(path: &Path, field: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("{field}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{field} must be a real directory"));
    }
    Ok(())
}

fn require_file(path: &Path, field: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| format!("{field}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{field} must be a real file"));
    }
    Ok(())
}

fn validate_component(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || !value.is_ascii()
        || value.bytes().any(|byte| byte.is_ascii_control())
        || matches!(value, "." | "..")
    {
        return Err(format!("{field} is invalid"));
    }
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
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
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
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
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
    "usage: qualification_receipt profile <output> [pr-contract|local-provider|scale|fault|fault-s3|fault-gcs|fault-azure|provider|provider-s3|provider-gcs|provider-azure|compatibility]\n       qualification_receipt workload <output> <profile.json> <seed> [cells operations duration_secs]\n       qualification_receipt verify-workload <workload.json> <profile.json>\n       qualification_receipt manifest <output> <evidence-dir>\n       qualification_receipt emit <output> <source> <image-digest> <artifact> [provider workload fault profile.json]\n       qualification_receipt bind-protected <output> <source> <image-digest> <profile.json> <execution-evidence.json> <signing-key-file> <run-artifact.json> <workload.json> [raw-artifact ...]\n       qualification_receipt verify <receipt> <source> <image-digest> <artifact> [profile.json [trusted-signer-hex]]\n       qualification_receipt verify-matrix <manifest> <source> <image-digest> [profile.json [trusted-signer-hex]]\n       qualification_receipt validate-cluster <receipt> <source> <image-digest> [release|source-only]".into()
}

#[cfg(test)]
mod tests {
    use super::{
        bind_protected, build_manifest, manifest_base, read_signing_key, require_manifest_output,
        require_trusted_signer, resolve_manifest_path,
    };
    use crab_cell_runtime::{
        Digest, QUALIFICATION_MATRIX_ROWS, QualificationExecution, QualificationExecutionEvidence,
        QualificationOperation, QualificationOperationExecutor, QualificationOwnership,
        QualificationProfile, QualificationProviderEvidence, QualificationReceipt,
        QualificationWorkload,
    };
    use ed25519_dalek::Signer;
    use std::{fs, future::Future, path::Path, pin::Pin, time::Duration};

    struct BinderExecutor;

    impl QualificationOperationExecutor for BinderExecutor {
        type Future<'a> = Pin<
            Box<dyn Future<Output = crab_cell_runtime::Result<QualificationExecution>> + Send + 'a>,
        >;

        fn execute<'a>(&'a mut self, _operation: QualificationOperation) -> Self::Future<'a> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(125)).await;
                Ok(QualificationExecution::acknowledged(true))
            })
        }
    }

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

    #[test]
    fn signing_key_reader_accepts_hex_and_rejects_symlinks() {
        let directory = tempfile::tempdir().expect("key directory");
        let key_path = directory.path().join("key");
        fs::write(
            &key_path,
            "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
        )
        .expect("key");
        let key = read_signing_key(&key_path).expect("hex key");
        assert_eq!(
            key.to_bytes(),
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25, 26, 27, 28, 29, 30, 31, 32,
            ]
        );
        let message = b"qualification";
        assert!(key.sign(message).to_bytes().iter().any(|byte| *byte != 0));

        #[cfg(unix)]
        {
            let linked = directory.path().join("linked-key");
            std::os::unix::fs::symlink(&key_path, &linked).expect("key symlink");
            assert!(read_signing_key(&linked).is_err());
        }
    }

    #[tokio::test]
    async fn bind_protected_cli_binds_and_verifies_a_measured_run() {
        let directory = tempfile::tempdir().expect("binder directory");
        let mut profile_value = serde_json::to_value(
            QualificationProfile::new("cli-binder".into(), 1, 8, 1, 5_000).expect("binder profile"),
        )
        .expect("binder profile value");
        profile_value["provider"] = serde_json::Value::String("rustfs".into());
        let profile: QualificationProfile =
            serde_json::from_value(profile_value).expect("named binder profile");
        let workload = QualificationWorkload::generate_with_size(&profile, 31, 1, 8, 1)
            .expect("binder workload");
        let mut executor = BinderExecutor;
        let summary = workload.run(&mut executor).await.expect("binder run");
        let elapsed_ms = u64::try_from(summary.elapsed().as_millis())
            .expect("binder elapsed duration")
            .max(1);
        let resources = [
            crab_cell_runtime::QualificationMetric::new(
                "peak_rss_bytes".into(),
                19,
                "bytes".into(),
            )
            .expect("RSS metric"),
            crab_cell_runtime::QualificationMetric::new(
                "peak_local_disk_bytes".into(),
                29,
                "bytes".into(),
            )
            .expect("disk metric"),
            crab_cell_runtime::QualificationMetric::new(
                "peak_file_descriptors".into(),
                39,
                "count".into(),
            )
            .expect("FD metric"),
            crab_cell_runtime::QualificationMetric::new("bucket_calls".into(), 49, "count".into())
                .expect("bucket metric"),
        ];
        let run = summary
            .artifact_with_resource_metrics(&workload, &resources)
            .expect("binder run artifact");
        let profile_path = directory.path().join("profile.json");
        let evidence_path = directory.path().join("execution-evidence.json");
        let key_path = directory.path().join("signing-key");
        let run_path = directory.path().join("run-artifact.json");
        let workload_path = directory.path().join("workload.json");
        let provider_path = directory.path().join("provider-evidence.json");
        let output_path = directory.path().join("receipt.json");
        fs::write(&profile_path, profile.encode().expect("profile encoding")).expect("profile");
        fs::write(
            &evidence_path,
            QualificationExecutionEvidence {
                provider: "rustfs".into(),
                workload: "primitives".into(),
                fault: "none".into(),
                toolchain: "rustc-test".into(),
                execution_profile: "release".into(),
                topology: "three-process".into(),
                started_at_ms: 1,
                finished_at_ms: 1u64.saturating_add(elapsed_ms),
                fault_schedule: b"none".to_vec(),
                ownership: vec![QualificationOwnership::new(
                    1,
                    1,
                    Digest::from_bytes([7; 32]),
                )],
                dirty: false,
            }
            .encode()
            .expect("evidence encoding"),
        )
        .expect("evidence");
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[91; 32]);
        fs::write(&key_path, signing_key.to_bytes()).expect("signing key");
        let run_bytes = run.encode().expect("run encoding");
        let workload_bytes = workload.encode().expect("workload encoding");
        let provider_bytes =
            QualificationProviderEvidence::new(&profile, workload.seed(), true, true, true)
                .expect("provider evidence")
                .encode()
                .expect("provider evidence encoding");
        fs::write(&run_path, &run_bytes).expect("run artifact");
        fs::write(&workload_path, &workload_bytes).expect("workload artifact");
        fs::write(&provider_path, &provider_bytes).expect("provider evidence");

        let mut args = vec![
            output_path.display().to_string(),
            "cli-source".into(),
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            profile_path.display().to_string(),
            evidence_path.display().to_string(),
            key_path.display().to_string(),
            run_path.display().to_string(),
            workload_path.display().to_string(),
            provider_path.display().to_string(),
        ]
        .into_iter();
        bind_protected(&mut args).expect("bind protected receipt");

        let receipt = QualificationReceipt::decode(&fs::read(&output_path).expect("receipt"))
            .expect("receipt decoding");
        receipt
            .verify_for_profile_with_signer(
                "cli-source",
                Digest::from_bytes([171; 32]),
                &profile,
                &[&run_bytes, &workload_bytes, &provider_bytes],
                signing_key.verifying_key().to_bytes(),
            )
            .expect_err("wrong image must be rejected");
        receipt
            .verify_for_profile_with_signer(
                "cli-source",
                Digest::from_bytes([170; 32]),
                &profile,
                &[&run_bytes, &workload_bytes, &provider_bytes],
                signing_key.verifying_key().to_bytes(),
            )
            .expect("receipt verification");
    }

    #[test]
    fn manifest_builder_emits_canonical_rows_and_rejects_invalid_output_or_missing_files() {
        let directory = tempfile::tempdir().expect("matrix directory");
        fs::create_dir(directory.path().join("receipts")).expect("receipts");
        fs::create_dir(directory.path().join("artifacts")).expect("artifacts");
        for workload in QUALIFICATION_MATRIX_ROWS {
            fs::write(
                directory
                    .path()
                    .join("receipts")
                    .join(format!("{workload}.json")),
                b"receipt",
            )
            .expect("receipt");
            let workload_dir = directory.path().join("artifacts").join(workload);
            fs::create_dir(&workload_dir).expect("workload artifacts");
            fs::write(workload_dir.join("z.json"), b"z").expect("z artifact");
            fs::write(workload_dir.join("a.json"), b"a").expect("a artifact");
        }

        let manifest = build_manifest(directory.path()).expect("canonical manifest");
        assert_eq!(
            manifest
                .entries()
                .iter()
                .map(|entry| entry.workload())
                .collect::<Vec<_>>(),
            QUALIFICATION_MATRIX_ROWS
        );
        assert_eq!(
            manifest.entries()[0].artifacts(),
            &[
                "artifacts/protocol/a.json".to_owned(),
                "artifacts/protocol/z.json".to_owned()
            ]
        );

        assert!(
            require_manifest_output(
                &directory.path().join("nested/qualification-matrix.json"),
                directory.path()
            )
            .is_err()
        );
        fs::remove_file(directory.path().join("receipts/protocol.json")).expect("remove receipt");
        assert!(build_manifest(directory.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_output_rejects_symlinks() {
        let directory = tempfile::tempdir().expect("matrix directory");
        let target = directory.path().join("target.json");
        let output = directory.path().join("qualification-matrix.json");
        fs::write(&target, b"existing").expect("target");
        std::os::unix::fs::symlink(&target, &output).expect("manifest symlink");
        assert!(require_manifest_output(&output, directory.path()).is_err());
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
