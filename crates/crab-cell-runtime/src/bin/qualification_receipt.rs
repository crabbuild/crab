use std::{
    env, fs,
    path::{Component, Path, PathBuf},
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::{
    Digest, QualificationMatrixManifest, QualificationMetric, QualificationOwnership,
    QualificationReceipt, QualificationRunner,
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
            if args.next().is_some() {
                return Err(usage());
            }
            let artifact =
                fs::read(&artifact).map_err(|error| format!("read artifact: {error}"))?;
            let now = unix_millis()?;
            let mut key_bytes = [0_u8; 32];
            rand::rng().fill(&mut key_bytes);
            let receipt = QualificationRunner::new(SigningKey::from_bytes(&key_bytes))
                .emit_with_evidence(
                    source,
                    image,
                    provider,
                    workload,
                    fault.clone(),
                    vec![
                        QualificationMetric::new(
                            "artifact_bytes".into(),
                            artifact.len() as u64,
                            "bytes".into(),
                        )
                        .map_err(|error| error.to_string())?,
                    ],
                    &artifact,
                    true,
                    (
                        "rustc".into(),
                        "release".into(),
                        "published-image".into(),
                        0,
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
        Some("verify") => {
            let receipt_path = required(&mut args, "receipt")?;
            let source = required(&mut args, "source revision")?;
            let image = parse_digest(&required(&mut args, "image digest")?)?;
            let artifact = required(&mut args, "artifact")?;
            if args.next().is_some() {
                return Err(usage());
            }
            let receipt = QualificationReceipt::decode(
                &fs::read(receipt_path).map_err(|error| format!("read receipt: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            if !receipt.passed() {
                return Err("qualification receipt is not passed".into());
            }
            let artifact = fs::read(artifact).map_err(|error| format!("read artifact: {error}"))?;
            receipt
                .verify_for(&source, image, &artifact)
                .map_err(|error| error.to_string())
        }
        Some("verify-matrix") => verify_matrix(&mut args),
        _ => Err(usage()),
    }
}

fn verify_matrix(args: &mut impl Iterator<Item = String>) -> Result<(), String> {
    let manifest_path = PathBuf::from(required(args, "matrix manifest")?);
    let source = required(args, "source revision")?;
    let image = parse_digest(&required(args, "image digest")?)?;
    if args.next().is_some() {
        return Err(usage());
    }
    let manifest = QualificationMatrixManifest::decode(
        &fs::read(&manifest_path).map_err(|error| format!("read matrix manifest: {error}"))?,
    )
    .map_err(|error| error.to_string())?;
    let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
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
    QualificationReceipt::verify_matrix(&source, image, &evidence)
        .map_err(|error| error.to_string())?;
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
    let candidate = canonical_base.join(path);
    let resolved = fs::canonicalize(&candidate)
        .map_err(|error| format!("resolve matrix artifact path: {error}"))?;
    if !resolved.starts_with(&canonical_base) {
        return Err("matrix paths must stay within the manifest directory".into());
    }
    Ok(resolved)
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
    "usage: qualification_receipt emit <output> <source> <image-digest> <artifact> [provider workload fault]\n       qualification_receipt verify <receipt> <source> <image-digest> <artifact>\n       qualification_receipt verify-matrix <manifest> <source> <image-digest>".into()
}
