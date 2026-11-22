use super::*;
use tokio::io::AsyncWriteExt as _;

#[derive(Debug, Clone)]
pub(super) enum GitPackBody {
    Resident(Bytes),
    File {
        path: PathBuf,
        length: u64,
        // The blocking installer owns this lifetime, including after its await is dropped.
        _owner: Arc<tempfile::TempDir>,
    },
}

impl GitPackBody {
    pub(super) fn len(&self) -> u64 {
        match self {
            Self::Resident(bytes) => bytes.len() as u64,
            Self::File { length, .. } => *length,
        }
    }

    pub(super) fn materialize(&self, directory: &Path) -> Result<PathBuf> {
        match self {
            Self::Resident(bytes) => {
                let path = directory.join("pack.pack");
                std::fs::write(&path, bytes)?;
                Ok(path)
            }
            Self::File { path, .. } => Ok(path.clone()),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SectionKind {
    Pack,
    Index,
    ReverseIndex,
    Locator,
}

#[derive(Default)]
pub(super) struct LayeredPayloadRead {
    packs: BTreeMap<usize, GitPackBody>,
    sidecars: BTreeMap<(usize, SectionKind), Bytes>,
    pub(super) bytes: u64,
}

impl LayeredPayloadRead {
    pub(super) fn pack(&self, member: usize) -> Result<GitPackBody> {
        self.packs
            .get(&member)
            .cloned()
            .ok_or_else(|| ReadError::internal("layered member has no staged pack body"))
    }

    pub(super) fn sidecar(&self, member: usize, kind: SectionKind) -> Result<Bytes> {
        self.sidecars
            .get(&(member, kind))
            .cloned()
            .ok_or_else(|| ReadError::internal("layered member has no staged sidecar"))
    }
}

struct Section {
    member: usize,
    kind: SectionKind,
    range: PackRange,
}

struct Window {
    path: object_store::path::Path,
    range: std::ops::Range<u64>,
    source_size: u64,
    sections: Vec<Section>,
}

enum Output {
    Pack(tokio::io::BufWriter<tokio::fs::File>),
    Sidecar(Vec<u8>),
}

impl Output {
    async fn flush(&mut self) -> Result<()> {
        if let Self::Pack(file) = self {
            file.flush().await?;
        }
        Ok(())
    }
}

pub(super) async fn fetch_layered_payload_windows(
    store: &Store,
    members: &[LayeredPayloadMemberRead],
    windows: &[LayeredPayloadWindow],
    mode: LayeredPayloadWindowMode,
    directory: &Arc<tempfile::TempDir>,
    cancel: &CancellationToken,
) -> Result<LayeredPayloadRead> {
    // Own descriptors before suspension so server-spawned readers remain Send.
    let requests = windows
        .iter()
        .map(|window| {
            let first = window
                .member_indices
                .first()
                .and_then(|index| members.get(*index))
                .ok_or_else(|| ReadError::internal("layered payload window has no member"))?;
            let mut sections = Vec::new();
            for index in &window.member_indices {
                let read = &members[*index];
                let member = &read.member.member;
                for (kind, range) in [
                    (SectionKind::Pack, member.pack()),
                    (SectionKind::Index, member.index()),
                    (SectionKind::ReverseIndex, member.reverse_index()),
                    (SectionKind::Locator, member.locator()),
                ] {
                    let selected = match mode {
                        LayeredPayloadWindowMode::Full => {
                            kind != SectionKind::Pack || !read.complete_local
                        }
                        LayeredPayloadWindowMode::Sidecars => kind != SectionKind::Pack,
                        LayeredPayloadWindowMode::Packs => {
                            kind == SectionKind::Pack && !read.complete_local
                        }
                    };
                    if selected {
                        sections.push(Section {
                            member: *index,
                            kind,
                            range: range.clone(),
                        });
                    }
                }
            }
            sections.sort_unstable_by_key(|section| section.range.offset());
            Ok(Window {
                path: window.path.clone(),
                range: window.range.clone(),
                source_size: first.member.source_size,
                sections,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let cancellation = cancel.child_token();
    let _cancel_on_drop = cancellation.clone().drop_guard();
    let mut pending = futures_util::stream::iter(requests.into_iter().map(|window| {
        let store = store.clone();
        let directory = directory.clone();
        let cancel = cancellation.clone();
        async move {
            if cancel.is_cancelled() {
                return Err(ReadError::Cancelled);
            }
            // A dropped caller cancels the child token but detaches this owner;
            // the worker keeps staging alive until filesystem writes drain.
            tokio::spawn(async move { read_window(&store, window, directory, &cancel).await })
                .await
                .map_err(|error| ReadError::Io(std::io::Error::other(error)))?
        }
    }))
    .buffer_unordered(LAYERED_SIDECAR_READ_CONCURRENCY);
    let mut result = LayeredPayloadRead {
        bytes: layered_payload_window_bytes(windows)?,
        ..LayeredPayloadRead::default()
    };
    let mut failure = None;
    // Cancellation must drain every active filesystem write before callers can
    // remove staging. New futures observe the child token before origin I/O.
    while let Some(next) = pending.next().await {
        match next {
            Ok(read) if failure.is_none() => {
                result.packs.extend(read.packs);
                result.sidecars.extend(read.sidecars);
            }
            Err(error) if failure.is_none() => {
                failure = Some(error);
                cancellation.cancel();
            }
            _ => {}
        }
    }
    match failure {
        Some(error) => Err(error),
        None if cancel.is_cancelled() => Err(ReadError::Cancelled),
        None => Ok(result),
    }
}

async fn read_window(
    store: &Store,
    window: Window,
    directory: Arc<tempfile::TempDir>,
    cancel: &CancellationToken,
) -> Result<LayeredPayloadRead> {
    let edge = store.cdn_attempt(&window.path).filter(|edge| {
        edge.admits_range_batches(window.source_size, window.range.end - window.range.start)
    });
    let transfer = async |cancel: &CancellationToken| {
        if let Some(edge) = &edge {
            let body = edge
                .range_batches(window.source_size, window.range.clone())?
                .flat_map(|batch| {
                    let chunks = match batch {
                        Ok(Some(chunks)) => {
                            chunks.into_iter().map(|bytes| Ok(Some(bytes))).collect()
                        }
                        Ok(None) => vec![Ok(None)],
                        Err(error) => vec![Err(error)],
                    };
                    futures_util::stream::iter(chunks)
                });
            match read_window_body(&window, &directory, body, cancel).await {
                Ok(Some(read)) => return Ok(read),
                Ok(None) | Err(ReadError::CorruptObject { .. }) => {
                    // A rejected attempt has drained its private output. The
                    // origin restarts the entire window and rewrites its files.
                    tracing::debug!("restarting layered payload window at the selected origin");
                }
                Err(error) => return Err(error),
            }
        }
        read_origin_window(store, &window, &directory, cancel).await
    };
    match &edge {
        Some(edge) => edge.with_file_deadline(cancel, transfer).await,
        None => transfer(cancel).await,
    }
}

async fn read_origin_window(
    store: &Store,
    window: &Window,
    directory: &Arc<tempfile::TempDir>,
    cancel: &CancellationToken,
) -> Result<LayeredPayloadRead> {
    if cancel.is_cancelled() {
        return Err(ReadError::Cancelled);
    }
    let (meta, actual, body) = store
        .get_parallel_range_stream(&window.path, window.range.clone(), None, cancel)
        .await
        .map_err(|error| match error {
            crab_storage::StorageError::Cancelled => ReadError::Cancelled,
            other => other.into(),
        })?;
    if actual != window.range || meta.size != window.source_size {
        return Err(corrupt_path(
            window.path.to_string(),
            "layered source size or range differs from its descriptor",
        ));
    }
    read_window_body(window, directory, body.map(|chunk| chunk.map(Some)), cancel)
        .await?
        .ok_or_else(|| ReadError::internal("origin payload stream ended before verification"))
}

async fn read_window_body(
    window: &Window,
    directory: &Arc<tempfile::TempDir>,
    body: impl futures_util::Stream<Item = crab_storage::Result<Option<Bytes>>>,
    cancel: &CancellationToken,
) -> Result<Option<LayeredPayloadRead>> {
    tokio::pin!(body);
    let mut position = window.range.start;
    let mut section_index = 0;
    let mut output = None;
    let mut hasher = blake3::Hasher::new();
    let mut read = LayeredPayloadRead::default();
    let result = async {
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ReadError::Cancelled),
                next = body.next() => next,
            };
            let Some(chunk) = next else { break };
            let Some(chunk) = chunk.map_err(|error| match error {
                crab_storage::StorageError::Cancelled => ReadError::Cancelled,
                other => other.into(),
            })?
            else {
                return Ok(false);
            };
            let mut bytes = chunk.as_ref();
            while !bytes.is_empty() {
                let Some(section) = window.sections.get(section_index) else {
                    position = position.checked_add(bytes.len() as u64).ok_or_else(|| {
                        corrupt_path("capsule Git pack", "window position overflowed")
                    })?;
                    break;
                };
                if position < section.range.offset() {
                    let skip = (section.range.offset() - position).min(bytes.len() as u64) as usize;
                    position += skip as u64;
                    bytes = &bytes[skip..];
                    continue;
                }
                let end = section
                    .range
                    .offset()
                    .checked_add(section.range.length())
                    .ok_or_else(|| corrupt_path("capsule Git pack", "section end overflowed"))?;
                if position >= end {
                    return Err(corrupt_path(
                        "capsule Git pack",
                        "layered sections overlap or are empty",
                    ));
                }
                if output.is_none() {
                    output = Some(if section.kind == SectionKind::Pack {
                        let file = tokio::fs::File::create(
                            directory.path().join(format!("{}.pack", section.member)),
                        )
                        .await?;
                        Output::Pack(tokio::io::BufWriter::with_capacity(256 * 1024, file))
                    } else {
                        let capacity = usize::try_from(section.range.length())
                            .map_err(std::io::Error::other)?;
                        let mut buffer = Vec::new();
                        buffer
                            .try_reserve_exact(capacity)
                            .map_err(std::io::Error::other)?;
                        Output::Sidecar(buffer)
                    });
                    hasher = blake3::Hasher::new();
                }
                let length = (end - position).min(bytes.len() as u64) as usize;
                let part = &bytes[..length];
                match output
                    .as_mut()
                    .ok_or_else(|| ReadError::internal("section output disappeared"))?
                {
                    Output::Pack(file) => file.write_all(part).await?,
                    Output::Sidecar(buffer) => buffer.extend_from_slice(part),
                }
                hasher.update(part);
                position += length as u64;
                bytes = &bytes[length..];
                if position == end {
                    let mut complete = output
                        .take()
                        .ok_or_else(|| ReadError::internal("section output disappeared"))?;
                    complete.flush().await?;
                    if hasher.finalize().to_hex().as_str() != section.range.blake3() {
                        return Err(corrupt_path(
                            "capsule Git pack",
                            "layered source range hash does not match its descriptor",
                        ));
                    }
                    match complete {
                        Output::Pack(_) => {
                            read.packs.insert(
                                section.member,
                                GitPackBody::File {
                                    path: directory.path().join(format!("{}.pack", section.member)),
                                    length: section.range.length(),
                                    _owner: directory.clone(),
                                },
                            );
                        }
                        Output::Sidecar(bytes) => {
                            read.sidecars
                                .insert((section.member, section.kind), Bytes::from(bytes));
                        }
                    }
                    section_index += 1;
                }
            }
        }
        if position != window.range.end || section_index != window.sections.len() {
            return Err(corrupt_path(
                "capsule Git pack",
                "layered source range returned an unexpected length",
            ));
        }
        Ok(true)
    }
    .await;
    let flushed = match output.as_mut() {
        Some(output) => output.flush().await,
        None => Ok(()),
    };
    flushed?;
    let complete = result?;
    if !complete {
        return Ok(None);
    }
    read.bytes = position - window.range.start;
    Ok(Some(read))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_storage::cdn::{CdnAccess, CdnRoute, CdnTransport};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn layered_payload_cdn_verifies_the_window_before_returning_staged_files() {
        for (response, origin, authorized) in [
            ("206 Partial Content", false, true),
            ("206 Partial Content", true, true),
            ("403 Forbidden", true, false),
        ] {
            let target = [1; 32];
            let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
                .with_target_identity(target);
            let layout = StoreLayout::new(store, "repository".to_owned());
            let path = layout.capsule_pack_layer_path(&"b".repeat(64));
            if origin {
                layout
                    .store()
                    .create_strict(&path, Bytes::from_static(b"pack-indextail"))
                    .await
                    .unwrap();
            }

            let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let edge = CdnTransport::loopback_fixture(socket.local_addr().unwrap()).unwrap();
            let route_path = path.clone();
            let request = tokio::spawn(async move {
                let (stream, _) = socket.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut request = String::new();
                while !request.ends_with("\r\n\r\n") {
                    assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
                }
                assert!(request.starts_with(&format!("GET /{route_path} HTTP/1.1\r\n")));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("range: bytes=0-9\r\n")
                );
                let (headers, body) = if !authorized {
                    ("Content-Length: 0", "")
                } else {
                    (
                        "Content-Length: 10\r\nContent-Range: bytes 0-9/14",
                        if origin { "pack-wrong" } else { "pack-index" },
                    )
                };
                let reply = format!("HTTP/1.1 {response}\r\n{headers}\r\n\r\n{body}");
                stream.write_all(reply.as_bytes()).await.unwrap();
            });
            let store = layout
                .store()
                .clone()
                .with_cdn_route(CdnRoute::new(
                    target,
                    layout.repo_prefix().to_owned(),
                    None,
                    CdnAccess::Public,
                    edge,
                    Duration::from_secs(2),
                ))
                .unwrap();
            let directory = Arc::new(tempfile::tempdir().unwrap());
            let window = Window {
                path,
                range: 0..10,
                source_size: 14,
                sections: vec![
                    Section {
                        member: 0,
                        kind: SectionKind::Pack,
                        range: PackRange::new(0, b"pack").unwrap(),
                    },
                    Section {
                        member: 0,
                        kind: SectionKind::Index,
                        range: PackRange::new(5, b"index").unwrap(),
                    },
                ],
            };
            let result =
                read_window(&store, window, directory.clone(), &CancellationToken::new()).await;
            if authorized {
                let output = result.unwrap();
                assert_eq!(
                    output.sidecar(0, SectionKind::Index).unwrap(),
                    Bytes::from_static(b"index")
                );
                let path = output
                    .pack(0)
                    .unwrap()
                    .materialize(directory.path())
                    .unwrap();
                assert_eq!(std::fs::read(path).unwrap(), b"pack");
            } else {
                assert!(crab_storage::store::cdn_read_rejection(&result.err().unwrap()).is_some());
                assert!(!directory.path().join("0.pack").exists());
            }
            tokio::time::timeout(Duration::from_secs(2), request)
                .await
                .unwrap()
                .unwrap();
        }
    }
}
