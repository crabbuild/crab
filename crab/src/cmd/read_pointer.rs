//! Owned pointer reads for the matching native runtime.

use std::{ops::Range, path::PathBuf, sync::Arc};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use crab_lfs::{LfsReadCache, LfsReadSession};
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use crab_read::PointerReader;
use crab_types::pointer_read::{Request, Response};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::core::{
    config::Config,
    error::{CrabError, Result, check_cancelled},
};
use crate::git::url::{ObjectUrl, UrlForm};
use crate::git::worktree::WorktreeContext;

const MAX_PREVIEW_METADATA_BYTES: u64 = 128 * 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_RANGE_BYTES: u32 = 8 * 1024 * 1024;

/// Serve one pointer until close, EOF or cancellation; drain before acknowledging close.
pub async fn run(cancel: &CancellationToken) -> Result<()> {
    let mut input = Input {
        reader: BufReader::new(tokio::io::stdin()),
        bytes: Vec::new(),
    };
    let first = tokio::select! {
        () = cancel.cancelled() => return Err(CrabError::Cancelled),
        first = input.next() => first?,
    };
    let Some(Request::Open {
        pointer_b64,
        dehydrate_path,
    }) = first
    else {
        return Err(invalid("the first reader command must open a pointer"));
    };
    let bytes = STANDARD
        .decode(pointer_b64)
        .map_err(std::io::Error::other)?;
    let mut output = tokio::io::stdout();
    if let Some(path) = dehydrate_path
        && super::dehydrate::build_profile_protection(&std::env::current_dir()?, false)
            .is_some_and(|protected| protected.is_match(&path))
    {
        emit(
            &mut output,
            Response::Failed {
                code: "prefetch_protected".into(),
                message: "The always prefetch profile keeps this file hydrated.".into(),
            },
        )
        .await?;
        return emit(&mut output, Response::Closed).await;
    }
    let opening_cancel = cancel.child_token();
    let opening = Session::open(&bytes, &opening_cancel);
    tokio::pin!(opening);
    let mut interruption = None;
    let opened = tokio::select! {
        biased;
        result = &mut opening => result,
        () = cancel.cancelled() => {
            opening_cancel.cancel();
            interruption = Some(Ok(None));
            opening.await
        }
        command = input.next() => {
            opening_cancel.cancel();
            interruption = Some(command);
            opening.await
        }
    };
    if let Some(command) = interruption {
        // Closing before Ready must drain lock admission and metadata work too.
        // Otherwise a cancelled tab can leave a native child waiting on Git clean.
        match opened {
            Ok(session) => session.close().await?,
            Err(error) if cancelled(&error, &opening_cancel) => {}
            Err(error) => return Err(error),
        }
        match command? {
            None | Some(Request::Close | Request::Cancel) => {
                return emit(&mut output, Response::Closed).await;
            }
            _ => return Err(invalid("only cancel or close may interrupt reader opening")),
        }
    }
    let mut session = opened?;
    let result = async {
        emit(
            &mut output,
            Response::Ready {
                size: session.size(),
                remote_url: session.config.remote_url.clone(),
            },
        )
        .await?;
        serve(&mut session, &mut input, &mut output, cancel).await
    }
    .await;
    let closed = session.close().await;
    let terminal = result.and(closed);
    match terminal {
        Ok(()) => emit(&mut output, Response::Closed).await,
        Err(error) => emit(&mut output, failure(&error)).await,
    }
}

async fn serve<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    session: &mut Session,
    input: &mut Input<R>,
    output: &mut W,
    cancel: &CancellationToken,
) -> Result<()> {
    loop {
        let command = tokio::select! {
            () = cancel.cancelled() => break,
            command = input.next() => command?,
        };
        let Some(command) = command else {
            break;
        };
        match command {
            Request::Open { .. } => return Err(invalid("the reader already owns a pointer")),
            Request::Close => break,
            // A completed range can race its cancellation command. Its data or
            // failure already acknowledges completion; do not enqueue another reply.
            Request::Cancel => continue,
            query @ (Request::Range { .. } | Request::Chunks { .. }) => {
                let token = cancel.child_token();
                let work = session.query(query, &token);
                tokio::pin!(work);
                let mut stop = false;
                let result = tokio::select! {
                    biased;
                    result = &mut work => result,
                    () = cancel.cancelled() => {
                        token.cancel();
                        stop = true;
                        work.await
                    }
                    command = input.next() => {
                        token.cancel();
                        let result = work.await;
                        match command? {
                            None | Some(Request::Close) => stop = true,
                            Some(Request::Cancel) => {},
                            _ => return Err(invalid("only cancel or close may interrupt a query")),
                        }
                        result
                    }
                };
                if stop {
                    // Cancellation is advisory; preserve a real integrity/transport
                    // failure after the in-flight worker has fully drained.
                    if let Err(error) = result {
                        if !cancelled(&error, &token) {
                            return Err(error);
                        }
                    }
                    break;
                }
                let reply = match result {
                    Ok(reply) => reply,
                    Err(error) if cancelled(&error, &token) => Response::Cancelled,
                    Err(error) => failure(&error),
                };
                emit(output, reply).await?;
            }
        }
    }
    Ok(())
}

enum Pointer {
    Crab(crab_types::pointer::Pointer),
    Lfs(crab_git::LfsPointer),
}

struct Session {
    pointer: Pointer,
    config: Config,
    local: PointerReader,
    staging: PathBuf,
    storage: PathBuf,
    pin: Option<crab_staging::StagedFilePin>,
    proofs: LfsReadCache,
    remote: Option<PointerReader>,
    lookup: Option<SharedFileIndexLookup>,
}

impl Session {
    async fn open(bytes: &[u8], cancel: &CancellationToken) -> Result<Self> {
        if bytes.len() > crab_git::MAX_LFS_POINTER_SIZE {
            return Err(invalid("pointer exceeds its size limit"));
        }
        let first = bytes
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or_default();
        let crab =
            std::str::from_utf8(first).is_ok_and(crab_types::pointer::is_supported_version_line);
        let pointer = if crab {
            Pointer::Crab(crab_types::pointer::Pointer::parse(bytes)?)
        } else {
            Pointer::Lfs(crab_git::LfsPointer::parse(bytes)?)
        };
        if matches!(&pointer, Pointer::Lfs(pointer) if !pointer.extensions.is_empty()) {
            return Err(invalid("LFS extensions require a smudge implementation"));
        }
        let config = Config::resolve_local()?;
        let context = WorktreeContext::resolve_from_path(&std::env::current_dir()?)?;
        let staging = context.shared_staging_dir();
        let git_dir = context.per_worktree_git_dir;
        let storage =
            tokio::task::spawn_blocking(move || crab_lfs::local::storage_from_git(&git_dir))
                .await
                .map_err(std::io::Error::other)??;
        let local = PointerReader::new(None, Some(staging.clone()), Some(storage.clone()));
        let pin = match &pointer {
            Pointer::Crab(pointer) => local.pin_file_with_cancel(pointer, cancel).await?,
            _ => None,
        };
        Ok(Self {
            pointer,
            config,
            local,
            staging,
            storage,
            pin,
            proofs: LfsReadCache::default(),
            remote: None,
            lookup: None,
        })
    }

    fn size(&self) -> u64 {
        match &self.pointer {
            Pointer::Crab(pointer) => pointer.size,
            Pointer::Lfs(pointer) => pointer.size,
        }
    }

    async fn query(&mut self, request: Request, cancel: &CancellationToken) -> Result<Response> {
        let session = LfsReadSession::new(self.proofs.clone());
        let result = self.query_inner(request, cancel, &session).await;
        // A cancelled remote future can leave an admitted SHA-256 job. Drain
        // this query's work before acknowledging it, while keeping checked sources.
        session.close().await;
        result
    }

    async fn query_inner(
        &mut self,
        request: Request,
        cancel: &CancellationToken,
        session: &LfsReadSession,
    ) -> Result<Response> {
        match request {
            Request::Range { offset, length } => {
                let (offset, bytes) = self.read(offset, length, cancel, session).await?;
                Ok(Response::Data {
                    offset,
                    bytes_b64: STANDARD.encode(bytes),
                })
            }
            Request::Chunks { start, limit } => {
                if !(1..=512).contains(&limit) {
                    return Err(invalid("chunk page limit must be in 1..=512"));
                }
                let pointer = match &self.pointer {
                    Pointer::Crab(pointer) => pointer.clone(),
                    Pointer::Lfs(_) => {
                        return Err(invalid("Git LFS objects do not have Crab chunk recipes"));
                    }
                };
                let page = if self.pin.is_some() {
                    self.local
                        .chunk_page(&pointer, start, limit, cancel)
                        .await?
                } else {
                    self.ensure_remote(cancel).await?;
                    self.remote
                        .as_ref()
                        .ok_or_else(|| invalid("missing captured reader"))?
                        .chunk_page(&pointer, start, limit, cancel)
                        .await?
                };
                check_cancelled(cancel)?;
                Ok(Response::Chunks { page: page.into() })
            }
            Request::Open { .. } | Request::Cancel | Request::Close => {
                Err(invalid("reader control command cannot be queried"))
            }
        }
    }

    async fn ensure_remote(&mut self, cancel: &CancellationToken) -> Result<()> {
        if self.remote.is_none() {
            let (hydrator, lookup) = remote_capture(&self.config, cancel).await?;
            self.lookup = Some(lookup);
            self.remote = Some(PointerReader::new(
                Some(hydrator),
                Some(self.staging.clone()),
                Some(self.storage.clone()),
            ));
        }
        Ok(())
    }

    async fn read(
        &mut self,
        offset: u64,
        length: u32,
        cancel: &CancellationToken,
        session: &LfsReadSession,
    ) -> Result<(u64, Vec<u8>)> {
        if length > MAX_RANGE_BYTES {
            return Err(invalid("range length exceeds 8 MiB"));
        }
        check_cancelled(cancel)?;
        let start = offset.min(self.size());
        let end = start.saturating_add(u64::from(length)).min(self.size());
        if start == end {
            return Ok((start, Vec::new()));
        }
        let bytes = self.range(start..end, cancel, session).await?;
        if bytes.len() as u64 != end - start {
            return Err(invalid("pointer reader returned an incomplete range"));
        }
        check_cancelled(cancel)?;
        Ok((start, bytes))
    }

    async fn range(
        &mut self,
        range: Range<u64>,
        cancel: &CancellationToken,
        session: &LfsReadSession,
    ) -> Result<Vec<u8>> {
        match &self.pointer {
            Pointer::Crab(pointer) if self.pin.is_some() => {
                return Ok(self
                    .local
                    .read_range(pointer, range, cancel)
                    .await?
                    .bytes
                    .to_vec());
            }
            Pointer::Lfs(pointer) => {
                if let Some(bytes) = session
                    .read_local_range(
                        &self.storage,
                        &pointer.oid,
                        pointer.size,
                        range.clone(),
                        cancel,
                    )
                    .await?
                {
                    return Ok(bytes.to_vec());
                }
            }
            _ => {}
        }
        self.ensure_remote(cancel).await?;
        let reader = self
            .remote
            .as_ref()
            .ok_or_else(|| invalid("missing captured reader"))?;
        let bytes = match &self.pointer {
            Pointer::Crab(pointer) => reader.read_range(pointer, range, cancel).await?,
            Pointer::Lfs(pointer) => {
                reader
                    .read_lfs_range(pointer, range, cancel, session)
                    .await?
            }
        };
        Ok(bytes.bytes.to_vec())
    }

    async fn close(self) -> Result<()> {
        if let Some(lookup) = self.lookup {
            lookup.close().await?;
        }
        Ok(())
    }
}

struct Input<R> {
    reader: R,
    bytes: Vec<u8>,
}

impl<R: AsyncBufRead + Unpin> Input<R> {
    async fn next(&mut self) -> Result<Option<Request>> {
        loop {
            let chunk = self.reader.fill_buf().await?;
            if chunk.is_empty() {
                return if self.bytes.is_empty() {
                    Ok(None)
                } else {
                    Err(invalid("incomplete reader command"))
                };
            }
            let length = chunk
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(chunk.len(), |at| at + 1);
            if self.bytes.len() + length > MAX_REQUEST_BYTES {
                return Err(invalid("reader command exceeds 16 KiB"));
            }
            let complete = chunk[length - 1] == b'\n';
            self.bytes.extend_from_slice(&chunk[..length]);
            self.reader.consume(length);
            // The buffer belongs to Input, so selecting away from next() loses no
            // partial frame while a range races its cancel/close command.
            if complete {
                let bytes = std::mem::take(&mut self.bytes);
                return serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|error| std::io::Error::other(error).into());
            }
        }
    }
}

async fn emit(output: &mut (impl AsyncWrite + Unpin), response: Response) -> Result<()> {
    let mut bytes = serde_json::to_vec(&response).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}

fn failure(error: &CrabError) -> Response {
    Response::Failed {
        code: crate::core::error_catalog::error_code(error).into(),
        message: error.to_string().chars().take(4096).collect(),
    }
}

fn cancelled(error: &CrabError, token: &CancellationToken) -> bool {
    matches!(error, CrabError::Cancelled)
        || (token.is_cancelled()
            && matches!(error, CrabError::Io(source) if source.kind() == std::io::ErrorKind::Interrupted))
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
