use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crab_storage::{
    Store,
    cdn::{
        CdnAccess, CdnConsumer, CdnCredential, CdnCredentials, CdnEdge, CdnPolicy, CdnProfile,
        CdnRoute, CdnTransport, CdnVisibility,
    },
};

use super::{Access, CdnBinding, config_error};
use crate::core::error::Result;

impl CdnBinding {
    pub(super) fn validate_r2(&mut self) -> Result<()> {
        if self.access != Access::Private
            || self.export_origin.is_some()
            || self.publication.is_some()
            || self.approval_sha256.is_some()
            || !self
                .viewer_token_file
                .as_ref()
                .is_some_and(|path| path.is_absolute())
        {
            return Err(config_error(
                "Cloudflare R2 requires a private binding and an absolute viewer_token_file",
            ));
        }
        let base = crab_auth::cdn::canonical_viewer_base(&self.repo_prefix, &self.base_url)
            .ok_or_else(|| config_error("invalid Cloudflare repository prefix or HTTPS base"))?;
        if self.repo_prefix.split('/').any(|part| part == ".crab")
            || base["https://".len()..].contains('/')
            || base.ends_with(".r2.cloudflarestorage.com")
            || base.ends_with(".r2.dev")
        {
            return Err(config_error(
                "Cloudflare CDN requires a separate Worker hostname without a path",
            ));
        }
        self.base_url = base.to_owned();
        Ok(())
    }

    pub(super) fn attach_r2(
        &self,
        store: Store,
        token_file: &Path,
        timeout: Duration,
        consumer: CdnConsumer,
    ) -> Result<Store> {
        if store.storage_scope().is_some() {
            return Err(config_error(
                "Cloudflare CDN requires a dedicated native R2 bucket",
            ));
        }
        if !(CdnPolicy {
            edge: CdnEdge::Cloudflare,
            consumer,
            profile: CdnProfile::Direct,
            visibility: CdnVisibility::Private,
        })
        .enabled()
        {
            return Ok(store);
        }
        let target = store
            .target_identity()
            .copied()
            .ok_or_else(|| config_error("Cloudflare CDN requires an exact R2 target"))?;
        let route = CdnRoute::new(
            target,
            self.repo_prefix.clone(),
            Some(crab_storage::GLOBAL_PREFIX.to_owned()),
            CdnAccess::Private(Arc::new(CloudflareViewer(token_file.to_owned()))),
            CdnTransport::new(&self.base_url)?,
            timeout,
        )
        .for_consumer(consumer, CdnProfile::Direct);
        Ok(store.with_cdn_route(route)?)
    }
}

struct CloudflareViewer(PathBuf);

#[async_trait::async_trait]
impl CdnCredentials for CloudflareViewer {
    async fn credential(
        &self,
        _key: &object_store::path::Path,
        rejected: Option<u64>,
    ) -> crab_storage::Result<Option<CdnCredential>> {
        use tokio::io::AsyncReadExt as _;

        let invalid = || crab_storage::StorageError::Forbidden {
            path: "invalid Cloudflare viewer token file".into(),
        };
        let metadata = tokio::fs::symlink_metadata(&self.0)
            .await
            .map_err(|_| invalid())?;
        if !metadata.is_file() || metadata.len() > 45 {
            return Err(invalid());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(invalid());
            }
        }
        let mut token = String::new();
        tokio::fs::File::open(&self.0)
            .await
            .map_err(|_| invalid())?
            .take(46)
            .read_to_string(&mut token)
            .await
            .map_err(|_| invalid())?;
        let token = token.trim_end_matches(['\r', '\n']);
        if token.len() != 43
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(invalid());
        }
        let hash = blake3::hash(token.as_bytes());
        let mut generation = [0; 8];
        generation.copy_from_slice(&hash.as_bytes()[..8]);
        let generation = u64::from_le_bytes(generation);
        Ok((rejected != Some(generation))
            .then(|| CdnCredential::bearer(generation, token.to_owned())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r2_requires_a_worker_and_never_requests_a_crab_auth_grant() {
        let token_path = toml::Value::String(
            std::env::temp_dir()
                .join("viewer-token")
                .display()
                .to_string(),
        );
        let parse = |base: &str, prefix: &str| -> CdnBinding {
            toml::from_str(&format!(
                "origin = \"https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/bucket\"\nrepo_prefix = \"{prefix}\"\nbase_url = \"{base}\"\nviewer_token_file = {token_path}"
            )).unwrap()
        };
        let mut entries = [parse("https://cdn.example.com/", "org/repo")];
        CdnBinding::validate_all(&mut entries).unwrap();
        assert_eq!(entries[0].base_url, "https://cdn.example.com");
        assert!(
            CdnBinding::private_request(&entries, "bucket", "org/repo")
                .unwrap()
                .is_none()
        );
        for (base, prefix) in [
            ("https://cdn.example.com/subpath", "org/repo"),
            ("https://public.r2.dev", "org/repo"),
            ("https://cdn.example.com", ".crab"),
        ] {
            assert!(CdnBinding::validate_all(&mut [parse(base, prefix)]).is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn r2_viewer_retries_only_with_a_replaced_private_token() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all("a".repeat(43).as_bytes()).unwrap();
        let viewer = CloudflareViewer(file.path().to_owned());
        let key = object_store::path::Path::from(".crab/xorbs");
        let hash = blake3::hash("a".repeat(43).as_bytes());
        let generation = u64::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap());
        assert!(
            viewer
                .credential(&key, Some(generation))
                .await
                .unwrap()
                .is_none()
        );
        std::fs::write(file.path(), "b".repeat(43)).unwrap();
        assert!(
            viewer
                .credential(&key, Some(generation))
                .await
                .unwrap()
                .is_some()
        );
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(viewer.credential(&key, None).await.is_err());
    }
}
