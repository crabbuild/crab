use std::{
    fs,
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use crab_server::{Config, Error, Result};
use fs4::fs_std::FileExt as _;

pub(super) async fn run() -> Result<()> {
    let bucket = std::env::var("AWS_BUCKET")
        .map_err(|_| Error::Config("set AWS_BUCKET to an existing S3 bucket name"))?;
    validate_bucket(&bucket)?;
    let region = ["AWS_REGION", "AWS_DEFAULT_REGION"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.is_empty());
    let region = match region {
        Some(region) => region,
        None if crab_storage::s3_endpoint_from_env().is_some() => "us-east-1".to_owned(),
        None => object_store::aws::resolve_bucket_region(
            &bucket,
            &crab_storage::default_client_options(),
        )
        .await
        .map_err(Error::BucketRegion)?,
    };
    if region.is_empty()
        || region.len() > 63
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::Config("AWS region must be a non-empty region name"));
    }
    prepare(Path::new("/var/lib/crab/server"), &bucket, &region)
}

fn validate_bucket(bucket: &str) -> Result<()> {
    let bytes = bucket.as_bytes();
    if !(3..=63).contains(&bytes.len())
        || !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        || !bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
        || bucket.contains("..")
    {
        return Err(Error::Config(
            "AWS_BUCKET must be a lowercase S3 bucket name",
        ));
    }
    Ok(())
}

fn prepare(directory: &Path, bucket: &str, region: &str) -> Result<()> {
    fs::create_dir_all(directory)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("setup.lock"))?;
    // Keep the bucket binding and fleet identity under one lock. Concurrent
    // starts must not publish different identities into the same state volume.
    lock.lock_exclusive()?;
    let path = directory.join("server.toml");
    let generated = include_str!("../../deploy/container.server.toml")
        .replace("${AWS_BUCKET}", bucket)
        .replace("${AWS_REGION}", region);
    if path.exists() {
        let previous = Config::read(&path)?;
        let next: Config = toml::from_str(&generated)?;
        if previous.storage.url != next.storage.url {
            return Err(Error::Config(
                "server state belongs to a different bucket; use a separate Docker volume",
            ));
        }
    }
    prepare_identity(&directory.join("peer"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(generated.as_bytes())?;
    file.persist(&path).map_err(|error| error.error)?;
    Config::read(&path)?;
    Ok(())
}

fn prepare_identity(directory: &Path) -> Result<()> {
    if directory.exists() {
        if ["ca.crt", "peer.crt", "peer.key"]
            .iter()
            .any(|name| !directory.join(name).is_file())
        {
            return Err(Error::Config(
                "server peer identity is incomplete; restore the state volume",
            ));
        }
        return Ok(());
    }
    let parent = directory
        .parent()
        .ok_or(Error::Config("invalid peer identity path"))?;
    let temporary = tempfile::tempdir_in(parent)?;
    fs::write(
        temporary.path().join("peer.ext"),
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n",
    )?;
    let commands: &[&[&str]] = &[
        &["genpkey", "-algorithm", "ED25519", "-out", "ca.key"],
        &[
            "req",
            "-x509",
            "-new",
            "-days",
            "3650",
            "-subj",
            "/CN=crab-container-peer-ca",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
            "-key",
            "ca.key",
            "-out",
            "ca.crt",
        ],
        &["genpkey", "-algorithm", "ED25519", "-out", "peer.key"],
        &[
            "req",
            "-new",
            "-subj",
            "/CN=localhost",
            "-key",
            "peer.key",
            "-out",
            "peer.csr",
        ],
        &[
            "x509",
            "-req",
            "-days",
            "3650",
            "-CAcreateserial",
            "-in",
            "peer.csr",
            "-CA",
            "ca.crt",
            "-CAkey",
            "ca.key",
            "-extfile",
            "peer.ext",
            "-out",
            "peer.crt",
        ],
    ];
    for arguments in commands {
        let status = Command::new("openssl")
            .args(*arguments)
            .current_dir(temporary.path())
            .stdout(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(Error::Config(
                "OpenSSL could not generate the private container peer identity",
            ));
        }
    }
    for name in ["ca.key", "ca.srl", "peer.csr", "peer.ext"] {
        fs::remove_file(temporary.path().join(name))?;
    }
    fs::rename(temporary.path(), directory)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_preserves_the_peer_identity() {
        let directory = tempfile::tempdir().unwrap();
        prepare(directory.path(), "first-bucket", "eu-central-1").unwrap();
        let identity = fs::read(directory.path().join("peer/peer.key")).unwrap();
        prepare(directory.path(), "first-bucket", "eu-central-1").unwrap();
        assert_eq!(
            fs::read(directory.path().join("peer/peer.key")).unwrap(),
            identity
        );
    }

    #[test]
    fn setup_rejects_retargeting_its_state_volume() {
        let directory = tempfile::tempdir().unwrap();
        prepare(directory.path(), "first-bucket", "eu-central-1").unwrap();
        assert!(prepare(directory.path(), "other-bucket", "eu-central-1").is_err());
    }

    #[test]
    fn setup_fails_closed_on_partial_identity() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("peer")).unwrap();
        assert!(prepare(directory.path(), "test-bucket", "us-east-1").is_err());
    }

    #[test]
    fn bucket_names_cannot_inject_configuration_or_urls() {
        for name in [
            "bad/bucket",
            "bad\"bucket",
            "bad\nbucket",
            "a..b",
            "UPPER",
            "ab",
        ] {
            assert!(validate_bucket(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn generated_config_retains_region_and_local_trust_boundary() {
        let directory = tempfile::tempdir().unwrap();
        prepare(directory.path(), "test-bucket", "eu-central-1").unwrap();
        let config = Config::read(&directory.path().join("server.toml")).unwrap();
        assert!(
            config.storage.region.as_deref() == Some("eu-central-1")
                && config.listen.ip().is_loopback()
                && config.management_listen.ip().is_loopback()
                && config.auth.is_none()
                && config.cells.durability == crab_server::CellDurabilityMode::Object
        );
    }
}
