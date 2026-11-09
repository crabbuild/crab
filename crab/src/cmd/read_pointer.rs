//! Bounded pointer reads for the matching desktop native runtime.

use std::ops::Range;

use clap::Args;
use crab_git::PointerKind;
use crab_lfs::LfsReadSession;
use crab_read::PointerReader;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::core::{
    config::Config,
    error::{CrabError, Result, check_cancelled},
};
use crate::git::url::{ObjectUrl, UrlForm};
use crate::git::worktree::WorktreeContext;
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use std::sync::Arc;

const MAX_PREVIEW_METADATA_BYTES: u64 = 128 * 1024 * 1024;

/// Parameters for the private desktop range-read protocol.
#[derive(Debug, Args)]
pub struct ReadPointerArgs {
    #[arg(long, default_value_t = 0)]
    pub offset: u64,
    #[arg(long, default_value_t = 0)]
    pub length: u32,
    #[arg(long)]
    pub inspect: bool,
    #[arg(long)]
    pub expected_remote: Option<String>,
}

/// Read a pointer from stdin and emit verified range bytes after cleanup.
pub async fn run(args: &ReadPointerArgs, cancel: &CancellationToken) -> Result<()> {
    if args.length > 8 * 1024 * 1024 {
        return Err(invalid("range length exceeds 8 MiB"));
    }
    let mut bytes = Vec::new();
    tokio::io::stdin()
        .take(crab_git::MAX_LFS_POINTER_SIZE as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    let pointer = crab_git::classify(&bytes);
    let size = match &pointer {
        PointerKind::Crab(pointer) => pointer.size,
        PointerKind::Lfs(pointer) if pointer.extensions.is_empty() => pointer.size,
        PointerKind::Lfs(_) => {
            return Err(invalid("LFS extensions require a smudge implementation"));
        }
        PointerKind::NotAPointer => {
            return Err(invalid("stdin is not a supported content pointer"));
        }
    };
    check_cancelled(cancel)?;
    if args.inspect {
        let config = Config::resolve_local()?;
        let output = serde_json::to_vec(&serde_json::json!({ "remote_url": config.remote_url }))
            .map_err(std::io::Error::other)?;
        tokio::io::stdout().write_all(&output).await?;
        return Ok(());
    }
    let expected: Option<Option<String>> = args
        .expected_remote
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(std::io::Error::other)?;
    let start = args.offset.min(size);
    let end = start.saturating_add(u64::from(args.length)).min(size);
    let output = read(&pointer, start..end, expected.as_ref(), cancel).await?;
    check_cancelled(cancel)?;
    tokio::io::stdout().write_all(&output).await?;
    Ok(())
}

async fn read(
    pointer: &PointerKind,
    range: Range<u64>,
    expected_remote: Option<&Option<String>>,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    if range.is_empty() {
        return Ok(Vec::new());
    }
    let context = WorktreeContext::resolve()?;
    let staging = context.shared_staging_dir();
    let git_dir = context.per_worktree_git_dir.clone();
    let storage = tokio::task::spawn_blocking(move || crab_lfs::local::storage_from_git(&git_dir))
        .await
        .map_err(std::io::Error::other)??;
    let local = PointerReader::new(None, Some(staging.clone()), Some(storage.clone()));
    let session = LfsReadSession::default();
    let mut lookup = None;
    let result = async {
        // Unpublished ownership is verified locally before remote discovery.
        // Absence alone selects the store; corrupt selected local bytes fail.
        match pointer {
            PointerKind::Crab(pointer) => {
                if let Some(_pin) = local.pin_file(pointer).await? {
                    return Ok(local
                        .read_range(pointer, range, cancel)
                        .await?
                        .bytes
                        .to_vec());
                }
            }
            PointerKind::Lfs(pointer) => {
                if let Some(bytes) = session
                    .read_local_range(&storage, &pointer.oid, pointer.size, range.clone(), cancel)
                    .await?
                {
                    return Ok(bytes.to_vec());
                }
            }
            PointerKind::NotAPointer => return Err(invalid("not a content pointer")),
        }
        let config = Config::resolve_local()?;
        if expected_remote.is_some_and(|expected| expected != &config.remote_url) {
            return Err(invalid("content repository changed; reopen the preview"));
        }
        let (hydrator, index) = remote_capture(&config, cancel).await?;
        lookup = Some(index);
        let reader = PointerReader::new(Some(hydrator), Some(staging), Some(storage));
        let output = match pointer {
            PointerKind::Crab(pointer) => reader.read_range(pointer, range, cancel).await?,
            PointerKind::Lfs(pointer) => {
                reader
                    .read_lfs_range(pointer, range, cancel, &session)
                    .await?
            }
            PointerKind::NotAPointer => return Err(invalid("not a content pointer")),
        };
        Ok(output.bytes.to_vec())
    }
    .await;
    session.close().await;
    // Native reconstruction must finish before closing its captured lookup.
    let closed = match lookup {
        Some(lookup) => lookup.close().await.map_err(CrabError::from),
        None => Ok(()),
    };
    result.and_then(|bytes| {
        closed?;
        Ok(bytes)
    })
}

async fn remote_capture(
    config: &Config,
    cancel: &CancellationToken,
) -> Result<(Arc<crab_read::ShardHydrator>, SharedFileIndexLookup)> {
    let url = config
        .remote_url
        .as_deref()
        .ok_or_else(|| invalid("no content remote configured"))?;
    let object = ObjectUrl::parse(url)?;
    let layout = if object.form == UrlForm::Crab {
        let cache = crab_auth::token_cache::expand_token_cache_path(&config.auth.token_cache_path);
        let locator = crab_auth_store::ManagedRepositoryResolver::new(cache).classify(url)?;
        match locator {
            crab_git::RepositoryLocator::Direct(repository) => {
                let parsed = crate::git::url::CrabUrl {
                    bucket: repository.bucket,
                    repo_path: repository.repo_prefix,
                };
                crate::replication::select_read_store(config, &parsed, "read-pointer", cancel)
                    .await?
                    .router
            }
            managed @ crab_git::RepositoryLocator::Managed(_) => {
                let resolved = crate::auth::build_repository_store(
                    config,
                    managed,
                    crab_auth::TransferOperation::Hydrate,
                    cancel,
                )
                .await?;
                crate::storage::StoreLayout::new(resolved.store, resolved.repository_prefix)
            }
        }
    } else {
        let resolved =
            crate::storage::resolve_object_url_store(&object, config, "read-pointer", cancel)
                .await?;
        crate::storage::StoreLayout::new(resolved.store, resolved.prefix)
    };
    let availability = crate::cmd::hydrate_restore::build_restore_availability(
        config,
        layout.store(),
        layout.repo_prefix(),
        config.hydrate.auto_restore,
    )
    .await?;
    let shared = crab_storage::StoreLayout::with_global_prefix(
        layout.store().as_storage().clone(),
        layout.repo_prefix().to_owned(),
        layout.global_prefix().to_owned(),
    );
    let cache = crab_cache_store::CachingStore::new(shared.store().clone(), &config.cache)?;
    let hydrator = crate::read::build_shared_hydrator(cache, layout, config)?;
    let hydrator = match availability {
        Some(availability) => hydrator.with_availability(availability),
        None => hydrator,
    };
    let maximum = if config.uploadpack_max_egress_bytes == 0 {
        MAX_PREVIEW_METADATA_BYTES
    } else {
        config
            .uploadpack_max_egress_bytes
            .min(MAX_PREVIEW_METADATA_BYTES)
    };
    // Match SDK read-only catalog ownership. Mounts separately retain their
    // long-lived views; a preview must not require writing remote lease objects.
    let view = crab_read::capsule_protocol::open_view(
        &shared,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: maximum,
            max_frontier_bytes: maximum,
        },
    )
    .await?;
    let catalog = view.pointer_catalog_lookup(&shared, maximum)?;
    let lookup = SharedFileIndexLookup::for_pointer_catalog(shared, catalog);
    Ok((
        Arc::new(hydrator.with_file_index_lookup(lookup.clone())),
        lookup,
    ))
}

fn invalid(message: &str) -> CrabError {
    CrabError::Configuration {
        key: message.into(),
        origin: "desktop pointer read".into(),
    }
}
