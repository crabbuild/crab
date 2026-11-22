use std::{collections::BTreeMap, io::Read as _};

use object_store::path::Path;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::{CdnPublicationFile, CdnReadFamily};
use crate::{Result, StorageError, layout::StoreLayout};

const MAX_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct Publication {
    pub(crate) source_origin: String,
    pub(crate) sizes: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: String,
    source_origin: String,
    export_origin: String,
    repository_prefix: String,
    base_url: String,
    deployment: String,
    inventory_sha256: String,
    objects: Vec<Entry>,
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
        if bytes.len() as u64 > MAX_BYTES || format!("{:x}", Sha256::digest(&bytes)) != file.sha256
        {
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
            || receipt.source_origin == export
            || receipt.repository_prefix != layout.repo_prefix()
            || receipt.base_url != base
            || receipt.deployment.is_empty()
            || receipt.deployment.chars().any(char::is_control)
            || !canonical_digest(&receipt.inventory_sha256)
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
    let receipt = serde_json::json!({
        "schema": "crab.cdn-export-publication.v1",
        "source_origin": source,
        "export_origin": export,
        "repository_prefix": prefix,
        "base_url": base,
        "deployment": "fixture",
        "inventory_sha256": "b".repeat(64),
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
