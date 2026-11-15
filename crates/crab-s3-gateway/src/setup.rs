use std::{
    fs,
    io::{Read as _, Write as _},
    path::Path,
};

use fs4::fs_std::FileExt as _;

use crate::{Config, Error, Result, config::valid_bucket_name};

const STATE_DIRECTORY: &str = "/var/lib/crab/gateway";

/// Prepare persistent single-bucket container configuration and return its backend region.
pub async fn setup_container() -> Result<String> {
    let bucket = std::env::var("AWS_BUCKET")
        .map_err(|_| Error::Config("set AWS_BUCKET to an existing S3 bucket name"))?;
    if !valid_bucket_name(&bucket) {
        return Err(Error::Config(
            "AWS_BUCKET must be a lowercase S3 bucket name",
        ));
    }
    let region = match configured_region() {
        Some(region) => region,
        None if crab_storage::provider_options::s3_endpoint_from_env().is_some() => {
            "us-east-1".to_owned()
        }
        None => object_store::aws::resolve_bucket_region(
            &bucket,
            &crab_storage::provider_options::default_client_options(),
        )
        .await
        .map_err(Error::BucketRegion)?,
    };
    // This value crosses the shell entrypoint, so accept only region characters.
    // Failed discovery never silently redirects backend writes to another region.
    if region.is_empty()
        || region.len() > 63
        || !region
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(Error::Config("AWS region must be a non-empty region name"));
    }
    prepare(Path::new(STATE_DIRECTORY), &bucket)?;
    Ok(region)
}

fn configured_region() -> Option<String> {
    ["AWS_REGION", "AWS_DEFAULT_REGION"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.is_empty())
}

fn prepare(directory: &Path, bucket: &str) -> Result<Config> {
    fs::create_dir_all(directory)?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("setup.lock"))?;
    // Serialize the bucket binding with credential creation. Concurrent starts
    // must not bind one private identity to different backing repositories.
    lock.lock_exclusive()?;
    let config_path = directory.join("gateway.toml");
    if config_path.exists() {
        let existing = Config::read(&config_path)?;
        if existing.repositories.len() != 1 || existing.repositories[0].bucket != bucket {
            return Err(Error::Config(
                "gateway state belongs to a different bucket; use a separate Docker volume",
            ));
        }
    }
    let secret_path = directory.join("secret-key");
    if !secret_path.exists() {
        let mut entropy = [0_u8; 32];
        getrandom::fill(&mut entropy)?;
        let secret = blake3::Hash::from_bytes(entropy).to_hex();
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(secret.as_bytes())?;
        file.persist_noclobber(&secret_path)
            .map_err(|error| error.error)?;
    }
    let mut secret = String::new();
    fs::File::open(&secret_path)?
        .take(65)
        .read_to_string(&mut secret)?;
    if secret.len() != 64 || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Error::Config("generated gateway secret is invalid"));
    }
    // One private file owns credential identity. Derived exports can be recreated
    // after interruption without rotating keys or coupling them to AWS identity.
    let access_key = blake3::hash(secret.as_bytes()).to_hex().to_string();
    let mut config: Config = toml::from_str(
        r#"
listen = "0.0.0.0:8080"
management_listen = "127.0.0.1:8081"
[cache]
directory = "/var/lib/crab/cache-volume/cache"
max_bytes = 2147483648
[[credentials]]
access_key = "generated"
secret_key_file = "/unused"
principal = "service-account:owner"
[[repositories]]
name = "repository"
bucket = "configured"
prefix = "repositories/default"
[[repositories.members]]
principal = "service-account:owner"
access = "write"
"#,
    )?;
    config.credentials[0].access_key.clone_from(&access_key);
    config.credentials[0].secret_key_file = secret_path;
    config.repositories[0].bucket = bucket.to_owned();
    config.validate()?;
    write_private(&config_path, toml::to_string(&config)?.as_bytes())?;
    let client_env = format!(
        "export AWS_ACCESS_KEY_ID='{access_key}'\nexport AWS_SECRET_ACCESS_KEY='{secret}'\nexport AWS_SESSION_TOKEN=''\nexport AWS_DEFAULT_REGION='us-east-1'\nexport AWS_REGION='us-east-1'\n"
    );
    write_private(&directory.join("client.env"), client_env.as_bytes())?;
    Ok(config)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let directory = path.parent().ok_or(Error::Config("invalid setup path"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(bytes)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_retains_credentials_and_recreates_client_exports() {
        let directory = tempfile::tempdir().unwrap();
        let first = prepare(directory.path(), "backing-bucket").unwrap();
        let client_path = directory.path().join("client.env");
        let client = fs::read(&client_path).unwrap();
        fs::remove_file(&client_path).unwrap();
        let second = prepare(directory.path(), "backing-bucket").unwrap();
        assert!(
            first.credentials[0].access_key == second.credentials[0].access_key
                && fs::read(client_path).unwrap() == client
        );
    }

    #[test]
    fn setup_uses_canonical_config_and_authentication() {
        let directory = tempfile::tempdir().unwrap();
        prepare(directory.path(), "backing-bucket").unwrap();
        let config = Config::read(&directory.path().join("gateway.toml")).unwrap();
        crate::auth::GatewayAuth::load(&config).unwrap();
        assert!(
            config.repositories[0].name == "repository"
                && config.repositories[0].prefix == "repositories/default"
                && config.repositories[0].bucket == "backing-bucket"
                && config.management_listen.ip().is_loopback()
        );
    }

    #[test]
    fn setup_rejects_retargeting_existing_state() {
        let directory = tempfile::tempdir().unwrap();
        prepare(directory.path(), "first-bucket").unwrap();
        assert!(prepare(directory.path(), "other-bucket").is_err());
    }

    #[test]
    fn concurrent_setups_bind_one_bucket() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path();
        let barrier = &std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = ["first-bucket", "other-bucket"]
                .into_iter()
                .map(|bucket| {
                    scope.spawn(move || {
                        barrier.wait();
                        prepare(directory, bucket)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap().is_ok())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            results.into_iter().filter(|succeeded| *succeeded).count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn setup_keeps_generated_files_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        prepare(directory.path(), "backing-bucket").unwrap();
        for name in ["secret-key", "gateway.toml", "client.env"] {
            let mode = fs::metadata(directory.path().join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }
}
