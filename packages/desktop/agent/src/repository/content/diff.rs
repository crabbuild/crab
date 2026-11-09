use crab_diff::{ChunkOrigin, ChunkSequence, ChunkSequenceSourceKind, ChunkSpan};
use crab_xet::{chunker::GearChunker, hash::MerkleHash};
use serde::{Deserialize, Serialize};

use super::*;

use super::command as git;

// Bound both sequences before invoking the shared comparator. Its matching
// work is independently bounded; this limit also bounds ranges in the reply.
const MAX_CHUNKS: usize = 262_144;
const WINDOW: u32 = 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Version {
    Working,
    Index,
    Revision { rev: String },
    Empty,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Params {
    pub repo_id: String,
    pub path: String,
    pub old_path: Option<String>,
    pub before: Version,
    pub after: Version,
    pub op_id: Option<String>,
}

#[derive(Serialize)]
struct ContentIdentity {
    kind: &'static str,
    size: u64,
    hash: String,
}

struct Side {
    source: Option<FileSource>,
    exists: bool,
    version: Version,
}

impl Side {
    async fn close(&self) -> Result<()> {
        if let Some(source) = &self.source {
            source.close().await?;
        }
        Ok(())
    }
}

pub(crate) async fn compare(repo: &BrowseSession, p: &Params, scope: ReadScope) -> Result<Value> {
    GitPath::new(p.path.as_bytes().to_vec())?;
    let old_path = p.old_path.as_deref().unwrap_or(&p.path);
    GitPath::new(old_path.as_bytes().to_vec())?;
    let before = capture(repo, old_path, p.before.clone(), scope.clone()).await?;
    let after = match capture(repo, &p.path, p.after.clone(), scope.clone()).await {
        Ok(after) => after,
        Err(error) => {
            before.close().await?;
            return Err(error);
        }
    };
    let result = comparison(repo, p, &before, &after, &scope).await;
    // Both sources share operation cancellation. Close only after comparison
    // has drained their readers, even when one side failed.
    let closed_before = before.close().await;
    let closed_after = after.close().await;
    closed_before?;
    closed_after?;
    result
}

async fn capture(
    repo: &BrowseSession,
    path: &str,
    mut version: Version,
    scope: ReadScope,
) -> Result<Side> {
    scope.check()?;
    if matches!(version, Version::Empty) {
        return Ok(Side {
            source: None,
            exists: false,
            version,
        });
    }
    let Some(root) = &repo.work_dir else {
        let Version::Revision { ref rev } = version else {
            return Err(AgentError::Other(
                "remote comparisons require committed revisions".into(),
            ));
        };
        // Resolve the revision before interpreting a file-level NotFound. An
        // invalid ref or failed snapshot open is never an absent file.
        let snapshot = scope.run(repo.snapshot(Some(rev), scope.options())).await?;
        let validated = GitPath::new(path.as_bytes().to_vec())?;
        let metadata = scope
            .run(async {
                Ok(snapshot
                    .file_metadata(validated.clone())
                    .with_options(ReadOptions::default().with_operation(scope.options()))
                    .await?)
            })
            .await;
        let metadata = match metadata {
            Ok(metadata) => metadata,
            Err(AgentError::Sdk(error))
                if error.kind() == crab_sdk::ErrorKind::NotFound
                    && error.cleanup_error().is_none() =>
            {
                return Ok(Side {
                    source: None,
                    exists: false,
                    version,
                });
            }
            Err(error) => return Err(error),
        };
        let source = FileSource {
            scope,
            size: metadata.size,
            source: Arc::new(Source::Remote {
                snapshot,
                path: validated,
                kind: metadata.kind,
                oid: metadata.id.to_string(),
            }),
        };
        return Ok(Side {
            source: Some(source),
            exists: true,
            version,
        });
    };
    let (mode, oid) = match &version {
        Version::Revision { rev } => {
            if rev == "HEAD" && unborn_head(root, &scope).await? {
                return Ok(Side {
                    source: None,
                    exists: false,
                    version: Version::Empty,
                });
            }
            let tree = format!("{rev}^{{tree}}");
            let tree = String::from_utf8(
                git::run(
                    root,
                    &["rev-parse", "--verify", "--end-of-options", &tree],
                    &scope,
                )
                .await?,
            )?
            .trim()
            .to_owned();
            let bytes = git::run(root, &["ls-tree", "-z", &tree, "--", path], &scope).await?;
            version = Version::Revision { rev: tree };
            entry(&bytes, false)?
        }
        Version::Index => {
            let bytes = git::run(root, &["ls-files", "--stage", "-z", "--", path], &scope).await?;
            entry(&bytes, true)?
        }
        Version::Working => {
            let metadata = match tokio::fs::symlink_metadata(root.join(path)).await {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let Some(metadata) = metadata else {
                return Ok(Side {
                    source: None,
                    exists: false,
                    version,
                });
            };
            if !metadata.is_file() {
                return Ok(Side {
                    source: None,
                    exists: true,
                    version,
                });
            }
            let source = FileSource::capture_metadata(repo, path, None, scope).await?;
            return Ok(Side {
                source: Some(source),
                exists: true,
                version,
            });
        }
        Version::Empty => (None, None),
    };
    let source = match (&mode, oid) {
        (Some(mode), Some(oid)) if mode == "100644" || mode == "100755" => {
            Some(FileSource::git_blob(repo, root, oid, scope).await?)
        }
        _ => None,
    };
    Ok(Side {
        source,
        exists: mode.is_some(),
        version,
    })
}

async fn unborn_head(root: &Path, scope: &ReadScope) -> Result<bool> {
    let target = match git::run(root, &["symbolic-ref", "--quiet", "HEAD"], scope).await {
        Ok(target) => String::from_utf8(target)?.trim().to_owned(),
        Err(AgentError::Cli { status: 1, .. }) => return Ok(false),
        Err(error) => return Err(error),
    };
    // show-ref --verify --quiet returns 1 for a missing ref, distinct from
    // corrupt repository/setup failures. Only an unborn symbolic HEAD is empty.
    match git::run(root, &["show-ref", "--verify", "--quiet", &target], scope).await {
        Ok(_) => Ok(false),
        Err(AgentError::Cli { status: 1, .. }) => Ok(true),
        Err(error) => Err(error),
    }
}

fn entry(bytes: &[u8], index: bool) -> Result<(Option<String>, Option<String>)> {
    if bytes.is_empty() {
        return Ok((None, None));
    }
    let entries = bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect::<Vec<_>>();
    if entries.len() != 1 {
        return Err(AgentError::Other(
            "comparison requires one resolved file; resolve index conflicts first".into(),
        ));
    }
    let header = entries[0]
        .split(|byte| *byte == b'\t')
        .next()
        .ok_or_else(|| std::io::Error::other("invalid Git entry"))?;
    let fields = std::str::from_utf8(header)
        .map_err(std::io::Error::other)?
        .split(' ')
        .collect::<Vec<_>>();
    if fields.len() != 3 || (index && fields[2] != "0") {
        return Err(AgentError::Other(
            "comparison requires a resolved Git entry".into(),
        ));
    }
    Ok((
        Some(fields[0].into()),
        Some(fields[if index { 1 } else { 2 }].into()),
    ))
}

async fn comparison(
    repo: &BrowseSession,
    p: &Params,
    before: &Side,
    after: &Side,
    scope: &ReadScope,
) -> Result<Value> {
    let old_identity = identity(before).await?;
    let new_identity = identity(after).await?;
    let kind = old_identity
        .as_ref()
        .or(new_identity.as_ref())
        .map(|identity| identity.kind);
    if kind.is_none() {
        let root = repo.work_dir.as_ref().ok_or_else(|| {
            AgentError::Other("ordinary Git patches require a local repository".into())
        })?;
        let patch = if !before.exists && after.exists && matches!(after.version, Version::Working) {
            let output = git::output(
                root,
                &[
                    "diff",
                    "--no-color",
                    "--no-index",
                    "--",
                    "/dev/null",
                    &p.path,
                ],
                scope,
            )
            .await?;
            if !output.status.success() && output.status.code() != Some(1) {
                return Err(AgentError::Cli {
                    status: output.status.code().unwrap_or(-1),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                });
            }
            String::from_utf8_lossy(&output.stdout).into_owned()
        } else {
            patch(root, p, &before.version, &after.version, scope).await?
        };
        return Ok(json!({ "kind": "git", "path": p.path, "patch": patch }));
    }
    let (old_identity, new_identity) = match (old_identity, new_identity) {
        (Some(identity), None) => {
            let other = working_identity(after, identity.kind).await?;
            (Some(identity), other)
        }
        (None, Some(identity)) => {
            let other = working_identity(before, identity.kind).await?;
            (other, Some(identity))
        }
        pair => pair,
    };
    let same = match (&old_identity, &new_identity) {
        (Some(old), Some(new)) => {
            old.kind == new.kind && old.size == new.size && old.hash == new.hash
        }
        _ => false,
    };
    let status = if !before.exists && !after.exists {
        "unchanged"
    } else if !before.exists {
        "added"
    } else if !after.exists {
        "deleted"
    } else if same {
        "unchanged"
    } else {
        "modified"
    };
    let crab = old_identity
        .as_ref()
        .or(new_identity.as_ref())
        .is_some_and(|identity| identity.kind == "crab")
        && [&old_identity, &new_identity].iter().all(|identity| {
            identity
                .as_ref()
                .is_none_or(|identity| identity.kind == "crab")
        });
    let can_compare = [&before, &after]
        .iter()
        .all(|side| !side.exists || side.source.as_ref().is_some_and(chunkable));
    let metrics = if crab && !same && can_compare {
        let old = sequence(before).await?;
        let new = sequence(after).await?;
        let path = p.path.clone();
        let report =
            tokio::task::spawn_blocking(move || crab_diff::compare_sequences(&path, &old, &new))
                .await
                .map_err(std::io::Error::other)?;
        scope.check()?;
        report.chunk_metrics
    } else {
        None
    };
    scope.check()?;
    Ok(
        json!({ "kind": "large_file", "path": p.path, "status": status,
            "before": old_identity, "after": new_identity,
            "before_exists": before.exists, "after_exists": after.exists,
            "old_size": before.source.as_ref().map(|source| source.size),
            "new_size": after.source.as_ref().map(|source| source.size),
            "chunk_metrics": metrics,
            "metrics_note": if same { "Content identities match; no chunk recipe comparison was needed." }
                else if !crab { "LFS and tracking-format changes have no shared Crab chunk recipe. Chunk reuse and transfer cost are not measured." }
                else if !can_compare { "A version has no Crab recipe or captured working file. Chunk metrics are unavailable." }
                else { "Ordered matching chunk content, independent of xorb placement. This is not network transfer cost." }
        }),
    )
}

fn chunkable(source: &FileSource) -> bool {
    matches!(
        source.source.as_ref(),
        Source::Local { .. }
            | Source::Pointer {
                identity: Identity::Blake3(_),
                ..
            }
            | Source::Remote {
                kind: ContentKind::Crab,
                ..
            }
    )
}

async fn identity(side: &Side) -> Result<Option<ContentIdentity>> {
    let Some(source) = &side.source else {
        return Ok(None);
    };
    Ok(raw_identity(source)
        .await?
        .map(|identity| describe_identity(identity, source.size)))
}

async fn raw_identity(source: &FileSource) -> Result<Option<Identity>> {
    let identity = match source.source.as_ref() {
        Source::Pointer { identity, .. } => Some(*identity),
        Source::Remote {
            snapshot,
            path,
            kind: ContentKind::Crab | ContentKind::Lfs,
            ..
        } => {
            let blob = source
                .scope
                .run(async {
                    Ok(snapshot
                        .read_blob(path.clone())
                        .with_options(ReadOptions::default().with_operation(source.scope.options()))
                        .await?)
                })
                .await?;
            let (size, identity) = pointer_metadata(&blob).ok_or_else(|| {
                AgentError::Other("captured pointer metadata has no valid pointer".into())
            })?;
            if size != source.size {
                return Err(AgentError::Other(
                    "captured pointer size disagrees with metadata".into(),
                ));
            }
            Some(identity)
        }
        _ => None,
    };
    Ok(identity)
}

fn describe_identity(identity: Identity, size: u64) -> ContentIdentity {
    let (kind, hash) = match identity {
        Identity::Blake3(hash) => ("crab", hash),
        Identity::Sha256(hash) => ("lfs", hash),
    };
    ContentIdentity {
        kind,
        size,
        hash: crab_git::lfs_pointer::hex_encode(&hash),
    }
}

async fn working_identity(side: &Side, kind: &str) -> Result<Option<ContentIdentity>> {
    let Some(source) = &side.source else {
        return Ok(None);
    };
    if !matches!(source.source.as_ref(), Source::Local { .. }) {
        return Ok(None);
    }
    let identity = if kind == "crab" {
        Identity::Blake3([0; 32])
    } else {
        Identity::Sha256([0; 32])
    };
    let mut hasher = identity.hasher();
    let mut offset = 0;
    while offset < source.size {
        let bytes = source.read_range(offset, WINDOW).await?;
        offset += bytes.len() as u64;
        hasher = tokio::task::spawn_blocking(move || {
            hasher.update(&bytes);
            hasher
        })
        .await
        .map_err(std::io::Error::other)?;
    }
    source.read_range(source.size, 0).await?;
    let actual = hasher.finish();
    let identity = if kind == "crab" {
        Identity::Blake3(actual)
    } else {
        Identity::Sha256(actual)
    };
    Ok(Some(describe_identity(identity, source.size)))
}

async fn sequence(side: &Side) -> Result<ChunkSequence> {
    let source_kind = match side.version {
        Version::Working => ChunkSequenceSourceKind::Worktree,
        Version::Index => ChunkSequenceSourceKind::Staged,
        _ => ChunkSequenceSourceKind::Committed,
    };
    let Some(source) = &side.source else {
        return Ok(ChunkSequence {
            source: source_kind,
            file_size: 0,
            file_hash: [0; 32].into(),
            spans: Vec::new(),
        });
    };
    let mut spans = Vec::new();
    let hash;
    if matches!(source.source.as_ref(), Source::Local { .. }) {
        let mut chunker = GearChunker::new();
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0;
        while offset < source.size {
            let bytes = source.read_range(offset, WINDOW).await?;
            offset += bytes.len() as u64;
            (chunker, hasher, spans) = tokio::task::spawn_blocking(move || {
                hasher.update(&bytes);
                let chunks = chunker.feed_bytes(&bytes);
                for chunk in chunks {
                    push_chunk(&mut spans, chunk.hash, chunk.data.len() as u64)?;
                }
                Ok::<_, AgentError>((chunker, hasher, spans))
            })
            .await
            .map_err(std::io::Error::other)??;
        }
        source.read_range(source.size, 0).await?;
        if let Some(chunk) = chunker.finalize() {
            push_chunk(&mut spans, chunk.hash, chunk.data.len() as u64)?;
        }
        hash = (*hasher.finalize().as_bytes()).into();
    } else {
        let Some(Identity::Blake3(raw)) = raw_identity(source).await? else {
            return Err(AgentError::Other(
                "chunk source has no captured Crab identity".into(),
            ));
        };
        hash = MerkleHash::from(raw);
        let mut start = 0;
        let mut offset = 0;
        loop {
            let page = source.chunk_page(start, 512).await?;
            if page.chunk_count > MAX_CHUNKS as u64 || page.chunks.is_empty() && page.next.is_some()
            {
                return Err(chunk_budget());
            }
            for chunk in page.chunks {
                if chunk.offset != offset || chunk.length == 0 {
                    return Err(AgentError::Other(
                        "chunk recipe does not cover the captured file contiguously".into(),
                    ));
                }
                offset = offset.checked_add(chunk.length).ok_or_else(chunk_budget)?;
                if offset > source.size {
                    return Err(AgentError::Other(
                        "chunk recipe exceeds captured file size".into(),
                    ));
                }
                let hash = MerkleHash::from_hex(&chunk.hash).map_err(std::io::Error::other)?;
                push_chunk(&mut spans, hash, chunk.length)?;
            }
            let Some(next) = page.next else {
                if offset != source.size || spans.len() as u64 != page.chunk_count {
                    return Err(AgentError::Other(
                        "chunk recipe does not cover the complete captured file".into(),
                    ));
                }
                break;
            };
            if next <= start || next != spans.len() as u64 {
                return Err(AgentError::Other(
                    "chunk recipe cursor did not advance".into(),
                ));
            }
            start = next;
        }
    }
    Ok(ChunkSequence {
        source: source_kind,
        file_size: source.size,
        file_hash: hash,
        spans,
    })
}

fn push_chunk(spans: &mut Vec<ChunkSpan>, hash: MerkleHash, len: u64) -> Result<()> {
    if spans.len() == MAX_CHUNKS {
        return Err(chunk_budget());
    }
    let offset = spans.last().map_or(0, |span| span.offset + span.len);
    spans.push(ChunkSpan {
        chunk_hash: hash,
        offset,
        len,
        origin: ChunkOrigin {
            xorb_hash: None,
            xorb_chunk_index: None,
        },
    });
    Ok(())
}

fn chunk_budget() -> AgentError {
    AgentError::Other("chunk comparison exceeds its 262144-chunk metadata budget".into())
}

async fn patch(
    root: &Path,
    p: &Params,
    before: &Version,
    after: &Version,
    scope: &ReadScope,
) -> Result<String> {
    let mut args = vec![
        "diff".to_owned(),
        "--no-color".into(),
        "--no-renames".into(),
    ];
    let empty;
    let before = if matches!(before, Version::Empty) {
        empty = String::from_utf8(
            git::run(root, &["hash-object", "-t", "tree", "--stdin"], scope).await?,
        )?
        .trim()
        .to_owned();
        Version::Revision { rev: empty }
    } else {
        before.clone()
    };
    match (&before, after) {
        (Version::Revision { rev: old }, Version::Revision { rev: new }) => {
            args.extend([old.clone(), new.clone()])
        }
        (Version::Revision { rev }, Version::Index) => {
            args.extend(["--cached".into(), rev.clone()])
        }
        (Version::Revision { rev }, Version::Working) => args.push(rev.clone()),
        (Version::Index, Version::Working) => {}
        _ => return Err(AgentError::Other("unsupported comparison direction".into())),
    }
    args.push("--".into());
    if let Some(old_path) = &p.old_path {
        args.push(old_path.clone());
    }
    args.push(p.path.clone());
    let args = args.iter().map(String::as_str).collect::<Vec<_>>();
    Ok(String::from_utf8_lossy(&git::run(root, &args, scope).await?).into_owned())
}

#[cfg(test)]
mod tests;
