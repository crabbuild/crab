//! Collect one committed v2 ref observation from an approved native fixture.

use std::collections::BTreeSet;
use std::process::ExitCode;

use crab::core::config::{Config, StorageProvider};
use crab::core::error::{CrabError, Result};
use crab_git::CrabUrl;
use crab_metadata::capsule_protocol::{
    CapsuleRefHead, MAX_ROOT_BYTES, capsule_ref_name_key, load_root, materialize_visible_ref_heads,
};
use crab_storage::{StorageProviderKind, Store, StoreLayout};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;

fn invalid(reason: &str) -> CrabError {
    CrabError::Configuration {
        key: "native CDN fixture ref".into(),
        origin: reason.into(),
    }
}

fn digest(bytes: impl AsRef<[u8]>) -> String {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    crab_git::lfs_pointer::hex_encode(&sha256)
}

async fn native_store(origin: &str, prefix: &str) -> Result<Store> {
    let url = Url::parse(origin).map_err(|_| invalid("invalid native origin"))?;
    let host = url
        .host_str()
        .filter(|_| {
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.port().is_none()
        })
        .ok_or_else(|| invalid("native origin must be a bare provider address"))?;
    let store = match url.scheme() {
        "s3" if url.path().is_empty() => {
            // Use the normal AWS SDK profile chain, without reading workspace
            // configuration or attaching a cache, replica, or CDN binding.
            let mut config = Config::default();
            config.auth.storage_provider = StorageProvider::S3;
            crab::auth::build_store(
                &config,
                CrabUrl {
                    bucket: host.into(),
                    repo_path: prefix.into(),
                },
                "fetch",
                &CancellationToken::new(),
            )
            .await?
            .into_storage()
        }
        "gs" if url.path().is_empty() => {
            crab_storage::build_static_env_store(host, StorageProviderKind::Gcs)?
        }
        "https" => {
            let account = host
                .strip_suffix(".blob.core.windows.net")
                .ok_or_else(|| invalid("invalid native Azure account"))?;
            let container = url.path().strip_prefix('/').unwrap_or_default();
            if container.is_empty() || container.contains('/') {
                return Err(invalid("native Azure origin must name a container"));
            }
            crab_storage::build_static_env_azure_account_container_store(account, container)?
        }
        _ => return Err(invalid("unknown native provider origin")),
    };
    if store.native_cdn_origin() != Some(origin) {
        return Err(invalid(
            "resolved endpoint differs from the approved native origin",
        ));
    }
    Ok(store)
}

async fn observe(layout: &StoreLayout<Store>, origin: &str, reference: &str) -> Result<Value> {
    let before = load_root(layout).await?;
    let root = before.record().root();
    let path = layout.capsule_ref_head_path(&capsule_ref_name_key(reference));
    let (body, revision) = layout
        .store()
        .get_with_etag_bounded(&path, MAX_ROOT_BYTES)
        .await?;
    let head = CapsuleRefHead::decode(&body)?;
    let after = load_root(layout).await?;
    if before.record().digest() != after.record().digest()
        || before.etag() != after.etag()
        || root.gc_fence().is_some()
    {
        return Err(invalid("root authority changed or is fenced"));
    }
    if head.ref_name() != reference
        || head.ref_epoch() != root.ref_epoch()
        || head.prepared_activation_id().is_some()
    {
        return Err(invalid(
            "ref head does not belong to the current single-ref publication",
        ));
    }
    let visible = materialize_visible_ref_heads(root, &[head], &BTreeSet::new())?;
    let transaction = visible
        .transactions
        .get(reference)
        .ok_or_else(|| invalid("ref head has no committed transaction"))?;
    if revision.e_tag.as_deref().is_none_or(str::is_empty)
        && revision.version.as_deref().is_none_or(str::is_empty)
    {
        return Err(invalid(
            "native provider did not return a ref-head revision",
        ));
    }
    let revision = serde_json::to_vec(&(revision.e_tag, revision.version))
        .map_err(|error| CrabError::Io(std::io::Error::other(error)))?;
    Ok(json!({
        "probe_source_sha": env!("CRAB_BUILD_SOURCE_SHA"),
        "origin_sha256": digest(origin),
        "prefix_sha256": digest(format!("{}/", layout.repo_prefix())),
        "ref_sha256": digest(reference),
        "repository_sha256": digest(root.repository_id()),
        "epoch_sha256": digest(root.ref_epoch()),
        "root_digest": before.record().digest(),
        "head_sha256": digest(body),
        "revision_sha256": digest(revision),
        "transaction_id": transaction,
        "oid": visible.refs.get(reference),
    }))
}

async fn run() -> Result<Value> {
    let args: Vec<_> = std::env::args_os()
        .skip(1)
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| invalid("expected UTF-8 fixture arguments"))
        })
        .collect::<Result<_>>()?;
    let [origin, prefix, reference] = args.as_slice() else {
        return Err(invalid(
            "expected the native origin, fixture prefix, and Git ref",
        ));
    };
    let isolated = prefix.strip_prefix("crab-cdn-qualification/");
    if !isolated.is_some_and(|name| {
        (1..=80).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    }) || !reference
        .strip_prefix("refs/heads/crab-cdn-qualification-")
        .is_some_and(|name| {
            (1..=64).contains(&name.len())
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        })
    {
        return Err(invalid(
            "expected an isolated qualification prefix and leased test ref",
        ));
    }
    let layout = StoreLayout::new(native_store(origin, prefix).await?, prefix.clone());
    observe(&layout, origin, reference).await
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // Provider error sources may contain request details.
            eprintln!("native ref observation failed ({})", error.code());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_metadata::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleTransaction};
    use crab_write::capsule_protocol::{initialize, publish};
    use object_store::memory::InMemory;

    use super::*;

    #[tokio::test]
    async fn observes_the_written_ref_and_its_committed_tombstone() {
        let store = Store::new(Arc::new(InMemory::new()));
        let layout = StoreLayout::new(store, "crab-cdn-qualification/ref-probe".into());
        let reference = "refs/heads/crab-cdn-qualification-123-2-abcdefabcdef";
        let origin = "s3://fixture";
        let mut base = initialize(&layout, &"4".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let oid = "7".repeat(40);

        assert!(observe(&layout, origin, reference).await.is_err());
        let mut samples = Vec::new();
        for (old, new) in [(None, Some(oid.clone())), (Some(oid), None)] {
            let edit = CapsuleRefEdit::new(reference, old, new.clone(), None);
            let transaction = CapsuleTransaction::new(base.record().digest(), vec![edit]).unwrap();
            let capsule = Capsule::build(&transaction, vec![], vec![]).unwrap();
            base = publish(&layout, base, &transaction, capsule).await.unwrap();
            let observed = observe(&layout, origin, reference).await.unwrap();
            assert_eq!(observed["oid"], json!(new));
            samples.push(observed);
        }
        assert_eq!(samples[0]["epoch_sha256"], samples[1]["epoch_sha256"]);
        assert_ne!(samples[0]["revision_sha256"], samples[1]["revision_sha256"]);
        assert_ne!(samples[0]["transaction_id"], samples[1]["transaction_id"]);
    }
}
