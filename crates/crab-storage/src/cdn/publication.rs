use std::{collections::BTreeMap, io::Read as _};

use crab_types::storage::public_cdn_export_is_isolated;
use object_store::path::Path;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::{CdnPublicationFile, CdnReadFamily};
use crate::{Result, StorageError, layout::StoreLayout};

const MAX_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct Publication {
    pub(crate) source_origin: String,
    pub(crate) source_placement: Placement,
    pub(crate) approval_digest: sha2::digest::Output<Sha256>,
    pub(crate) sizes: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    source_origin: String,
    source_placement: Placement,
    export_origin: String,
    export_placement: Placement,
    repository_prefix: String,
    base_url: String,
    deployment: String,
    inventory_sha256: String,
    sealed_at_unix_ms: u64,
    objects: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Placement {
    pub(crate) resource: String,
    pub(crate) location: String,
}

impl Placement {
    pub(crate) fn account_or_project(&self) -> Option<&str> {
        self.resource
            .split('/')
            .nth(1)
            .filter(|owner| !owner.is_empty())
    }

    fn matches(&self, origin: &str) -> bool {
        let parts = self.resource.split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            ["accounts", account, "buckets", bucket] => {
                account.len() == 12
                    && account.bytes().all(|byte| byte.is_ascii_digit())
                    && origin.strip_prefix("s3://") == Some(*bucket)
                    && !["cn-", "us-gov-", "us-iso", "eu-iso", "eusc-"]
                        .iter()
                        .any(|prefix| self.location.starts_with(prefix))
                    && self.location.rsplit_once('-').is_some_and(|(name, zone)| {
                        !zone.is_empty()
                            && zone.bytes().all(|byte| byte.is_ascii_digit())
                            && name.split('-').count() >= 2
                            && name.split('-').all(|part| {
                                !part.is_empty()
                                    && part.bytes().all(|byte| byte.is_ascii_lowercase())
                            })
                    })
            }
            ["projects", project, "buckets", bucket] => {
                !project.is_empty()
                    && !project.starts_with('0')
                    && project.bytes().all(|byte| byte.is_ascii_digit())
                    && origin.strip_prefix("gs://") == Some(*bucket)
                    && (1..=40).contains(&self.location.len())
                    && self.location.bytes().all(|byte| {
                        byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            }
            [
                "",
                "subscriptions",
                subscription,
                "resourcegroups",
                group,
                "providers",
                "microsoft.storage",
                "storageaccounts",
                account,
                "blobservices",
                "default",
                "containers",
                container,
            ] => {
                matches!(
                    subscription
                        .split('-')
                        .map(str::len)
                        .collect::<Vec<_>>()
                        .as_slice(),
                    [8, 4, 4, 4, 12]
                ) && subscription
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f' | b'-'))
                    && !matches!(*group, "." | "..")
                    && (1..=90).contains(&group.len())
                    && group.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || b"_.()-".contains(&byte)
                    })
                    && origin == format!("https://{account}.blob.core.windows.net/{container}")
                    && (1..=40).contains(&self.location.len())
                    && self
                        .location
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            }
            _ => false,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    size: u64,
    sha256: String,
}

impl Publication {
    pub(super) fn load(
        file: &CdnPublicationFile,
        layout: &StoreLayout<()>,
        base: &str,
        export: &str,
    ) -> Result<Self> {
        if !file.path.is_absolute() || !canonical_digest(&file.sha256) {
            return Err(invalid(
                "an absolute receipt path and lowercase SHA-256 are required",
            ));
        }
        if !file.path.symlink_metadata()?.is_file() {
            return Err(invalid("receipt must be a regular file"));
        }
        let input = std::fs::File::open(&file.path)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(invalid("receipt must be a regular file of at most 8 MiB"));
        }
        let mut bytes = Vec::new();
        input.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        let approval_digest = Sha256::digest(&bytes);
        if bytes.len() as u64 > MAX_BYTES || format!("{approval_digest:x}") != file.sha256 {
            return Err(invalid("receipt differs from the approved SHA-256"));
        }
        let receipt: Receipt =
            serde_json::from_slice(&bytes).map_err(|source| StorageError::ObjectStore {
                source: object_store::Error::Generic {
                    store: "CDN publication receipt",
                    source: Box::new(source),
                },
            })?;
        if receipt.schema != "crab.cdn-export-publication.v1"
            || receipt.export_origin != export
            || !receipt.source_placement.matches(&receipt.source_origin)
            || !receipt.export_placement.matches(&receipt.export_origin)
            || !public_cdn_export_is_isolated(&receipt.source_origin, export)
            || receipt.repository_prefix != layout.repo_prefix()
            || receipt.base_url != base
            || receipt.deployment.is_empty()
            || receipt.deployment.chars().any(char::is_control)
            || !canonical_digest(&receipt.inventory_sha256)
            || receipt.sealed_at_unix_ms == 0
            || receipt.objects.is_empty()
        {
            return Err(invalid(
                "receipt does not describe this public export and viewer binding",
            ));
        }
        let mut sizes = BTreeMap::new();
        for entry in receipt.objects {
            let key = format!("{}/{path}", receipt.repository_prefix, path = entry.path);
            let path =
                Path::parse(&key).map_err(|_| invalid("receipt contains an invalid object key"))?;
            let family = layout.cdn_read_family(&path);
            if key != path.as_ref()
                || !family.is_some_and(|family| !family.global())
                || !canonical_digest(&entry.sha256)
                || (family == Some(CdnReadFamily::Lfs)
                    && key.rsplit('/').next() != Some(entry.sha256.as_str()))
                || sizes.insert(key, entry.size).is_some()
            {
                return Err(invalid(
                    "receipt contains an invalid or duplicate immutable object",
                ));
            }
        }
        Ok(Self {
            source_origin: receipt.source_origin,
            source_placement: receipt.source_placement,
            approval_digest,
            sizes,
        })
    }
}

pub(super) fn invalid(reason: &'static str) -> StorageError {
    StorageError::InvalidStaticEnvTarget {
        target: "CDN publication receipt".into(),
        reason: reason.into(),
    }
}

fn canonical_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Write an operator receipt fixture for one immutable key.
#[cfg(any(test, feature = "test-support"))]
pub fn write_publication_fixture(
    path: std::path::PathBuf,
    source: &str,
    export: &str,
    prefix: &str,
    base: &str,
    relative_key: &str,
    size: u64,
) -> Result<CdnPublicationFile> {
    fn fixture_placement(origin: &str) -> serde_json::Value {
        let (resource, location) = if let Some(name) = origin.strip_prefix("s3://") {
            (format!("accounts/123456789012/buckets/{name}"), "us-east-1")
        } else if let Some(name) = origin.strip_prefix("gs://") {
            (format!("projects/123456789012/buckets/{name}"), "US")
        } else {
            let (account, container) = origin
                .strip_prefix("https://")
                .and_then(|url| url.split_once(".blob.core.windows.net/"))
                .unwrap_or_default();
            (
                format!(
                    "/subscriptions/11111111-2222-3333-4444-555555555555/resourcegroups/fixture/providers/microsoft.storage/storageaccounts/{account}/blobservices/default/containers/{container}"
                ),
                "westus2",
            )
        };
        serde_json::json!({"resource": resource, "location": location})
    }
    let receipt = serde_json::json!({
        "schema": "crab.cdn-export-publication.v1",
        "source_origin": source,
        "source_placement": fixture_placement(source),
        "export_origin": export,
        "export_placement": fixture_placement(export),
        "repository_prefix": prefix,
        "base_url": base,
        "deployment": "fixture",
        "inventory_sha256": "b".repeat(64),
        "sealed_at_unix_ms": 1,
        "objects": [{
            "path": relative_key,
            "size": size,
            "sha256": relative_key.rsplit('/').next().unwrap_or_default(),
        }],
    })
    .to_string();
    std::fs::write(&path, &receipt)?;
    Ok(CdnPublicationFile {
        path,
        sha256: format!("{:x}", Sha256::digest(receipt.as_bytes())),
    })
}
